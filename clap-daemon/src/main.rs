//! Jarvis clap-wake daemon.
//!
//! Standalone always-on background process that listens to the system mic
//! for hand-claps, and on detection launches Jarvis.app via `open -a`.
//!
//! The daemon is mic-polite: it only opens the mic when Jarvis itself is NOT
//! running. While Jarvis is alive, the daemon polls every 5 s and stays
//! silent — no mic LED, no audio capture, no resource use beyond a tiny
//! `pgrep` check. The moment Jarvis exits, the daemon resumes listening.
//!
//! Architecture:
//!
//!   loop {
//!     if Jarvis running   → sleep 5s
//!     else                → open mic, listen for clap, on detect
//!                           close mic, `open -a Jarvis`, sleep 15s for
//!                           Jarvis to boot, then loop
//!   }
//!
//! Clap detection logic is intentionally a near-copy of `src-tauri/src/vad.rs`'s
//! `ClapDetector` (loud + isolated transient). Single-clap wake is the default;
//! set `JARVIS_CLAP_WAKE_MODE=double` to require two claps within 90-1000 ms.
//! False positives just bring the HUD up unnecessarily; false negatives mean
//! the user has to launch Jarvis manually, defeating the point.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

// ---- Tunables ------------------------------------------------------------

/// Clap RMS must exceed the adaptive ambient baseline by this factor.
const CLAP_SPIKE_RATIO: f32 = 2.4;
/// ...and out-shout each neighbouring frame by this factor (rejects speech).
const CLAP_EDGE_RATIO: f32 = 1.3;
/// Absolute RMS floor so faint hiss can't satisfy the ratio tests.
const CLAP_ABS_MIN_RMS: f32 = 0.010;
/// Two claps at least this far apart (rejects a single clap's echo)...
const DOUBLE_CLAP_MIN_MS: u128 = 90;
/// ...and at most this far apart count as one double-clap gesture.
const DOUBLE_CLAP_MAX_MS: u128 = 1000;
/// Frame length in milliseconds — the unit of RMS analysis.
const FRAME_MS: usize = 32;
/// How often we poll for Jarvis's existence when we're idle (mic off).
const JARVIS_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// After firing `open -a Jarvis`, wait this long before resuming polling
/// — gives Jarvis time to start, claim the mic, and stabilise.
const POST_LAUNCH_DELAY: Duration = Duration::from_secs(15);

// ---- Clap detector -------------------------------------------------------

struct ClapDetector {
    rms_hist: [f32; 3],
    baseline: f32,
    last_clap_at_ms: Option<u128>,
    elapsed_ms: u128,
    cooldown_frames: u32,
}

impl ClapDetector {
    fn new() -> Self {
        Self {
            rms_hist: [0.0; 3],
            baseline: CLAP_ABS_MIN_RMS,
            last_clap_at_ms: None,
            elapsed_ms: 0,
            cooldown_frames: 0,
        }
    }

    /// Feed one frame's RMS. Returns `true` on the frame that completes the
    /// configured clap gesture.
    fn push(&mut self, rms: f32, frame_ms: usize) -> bool {
        self.elapsed_ms += frame_ms as u128;
        // Slide the 3-frame window.
        self.rms_hist[0] = self.rms_hist[1];
        self.rms_hist[1] = self.rms_hist[2];
        self.rms_hist[2] = rms;

        if self.cooldown_frames > 0 {
            self.cooldown_frames -= 1;
            return false;
        }

        let [before, mid, after] = self.rms_hist;
        let is_clap = mid > CLAP_ABS_MIN_RMS
            && mid > self.baseline * CLAP_SPIKE_RATIO
            && mid > before * CLAP_EDGE_RATIO
            && mid > after * CLAP_EDGE_RATIO;

        if !is_clap {
            // Adapt baseline on quiet frames only.
            self.baseline = 0.97 * self.baseline + 0.03 * rms;
            return false;
        }

        // Confirmed single clap on the middle frame (one frame back in time).
        self.cooldown_frames = 2;
        let clap_at = self.elapsed_ms.saturating_sub(frame_ms as u128);
        println!(
            "[clap-daemon] clap (rms={mid:.4} baseline={:.4}) @ {clap_at}ms",
            self.baseline
        );

        if single_clap_wake_enabled() {
            return true;
        }

        if let Some(prev) = self.last_clap_at_ms.take() {
            let gap = clap_at.saturating_sub(prev);
            if (DOUBLE_CLAP_MIN_MS..=DOUBLE_CLAP_MAX_MS).contains(&gap) {
                return true; // double-clap!
            }
        }
        self.last_clap_at_ms = Some(clap_at);
        false
    }
}

fn single_clap_wake_enabled() -> bool {
    std::env::var("JARVIS_CLAP_WAKE_MODE")
        .map(|v| !v.trim().eq_ignore_ascii_case("double"))
        .unwrap_or(true)
}

// ---- Jarvis lifecycle helpers --------------------------------------------

/// Is Jarvis.app currently running? Detection has to work reliably under
/// `launchd` (where the daemon lives), which means avoiding tools that may
/// behave differently in that minimal environment. `pgrep -f` proved flaky
/// — sometimes returned empty stdout for matching processes when invoked
/// from launchd. `ps -ax | grep` via `/bin/sh -c` is portable and works
/// identically from any spawn context.
fn jarvis_running() -> bool {
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg("ps -ax -o command | grep -v grep | grep -q 'Jarvis.app/Contents/MacOS/jarvis'")
        .status();
    match status {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

fn launch_jarvis() {
    println!("[clap-daemon] → launching Jarvis");
    mark_wake_launch();
    let app_path = jarvis_app_path();
    let mut cmd = Command::new("/usr/bin/open");
    if app_path.exists() {
        cmd.arg(&app_path);
    } else {
        eprintln!(
            "[clap-daemon] configured Jarvis.app not found at {}; falling back to `open -a Jarvis`",
            app_path.display()
        );
        cmd.arg("-a").arg("Jarvis");
    }
    let status = cmd.status();
    match status {
        Ok(s) if s.success() => println!("[clap-daemon] launch OK"),
        Ok(s) => eprintln!("[clap-daemon] `open Jarvis.app` exited with {s}"),
        Err(e) => eprintln!("[clap-daemon] `open Jarvis.app` failed: {e}"),
    }
}

fn jarvis_app_path() -> PathBuf {
    std::env::var("JARVIS_APP_PATH")
        .ok()
        .map(|v| PathBuf::from(v.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(
                "/Users/user/Desktop/Jarvis/src-tauri/target/release/bundle/macos/Jarvis.app",
            )
        })
}

fn wake_launch_marker_path() -> PathBuf {
    std::env::temp_dir().join("jarvis-wake-launch")
}

fn mark_wake_launch() {
    if let Err(e) = std::fs::write(wake_launch_marker_path(), b"clap\n") {
        eprintln!("[clap-daemon] wake marker write failed: {e}");
    }
}

// ---- Audio capture -------------------------------------------------------

/// Open the system's default input device and stream frames of `FRAME_MS`
/// length to the detector. Returns `Ok(())` on the frame that completes the
/// configured clap wake gesture; `Err` on audio setup failure.
fn listen_until_double_clap() -> Result<(), String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no default input device")?;
    let device_name = device.name().unwrap_or_else(|_| "<unnamed>".into());
    let config = device
        .default_input_config()
        .map_err(|e| format!("config: {e}"))?;
    let sample_rate = config.sample_rate().0 as usize;
    let channels = config.channels() as usize;
    let frame_samples = (sample_rate * FRAME_MS) / 1000;
    println!(
        "[clap-daemon] mic open: {device_name} @ {sample_rate}Hz, {channels}ch, frame_samples={frame_samples}"
    );

    // Channel: callback → main thread. We send `()` on a confirmed double-clap.
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let detector = Arc::new(Mutex::new(ClapDetector::new()));

    // Per-thread accumulator buffered until we have `frame_samples` mono samples.
    let pending: Arc<Mutex<VecDeque<f32>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(frame_samples * 2)));

    let detector_cb = detector.clone();
    let pending_cb = pending.clone();
    let tx_cb = tx.clone();

    let err_fn = |e| eprintln!("[clap-daemon] audio stream error: {e}");

    let stream_cfg: cpal::StreamConfig = config.clone().into();

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_input_stream(
                &stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    handle_chunk(
                        data,
                        channels,
                        frame_samples,
                        &pending_cb,
                        &detector_cb,
                        &tx_cb,
                    );
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build_input_stream f32: {e}"))?,
        cpal::SampleFormat::I16 => device
            .build_input_stream(
                &stream_cfg,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                    handle_chunk(
                        &f,
                        channels,
                        frame_samples,
                        &pending_cb,
                        &detector_cb,
                        &tx_cb,
                    );
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build_input_stream i16: {e}"))?,
        cpal::SampleFormat::U16 => device
            .build_input_stream(
                &stream_cfg,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    handle_chunk(
                        &f,
                        channels,
                        frame_samples,
                        &pending_cb,
                        &detector_cb,
                        &tx_cb,
                    );
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build_input_stream u16: {e}"))?,
        other => return Err(format!("unsupported sample format: {other:?}")),
    };

    stream.play().map_err(|e| format!("stream.play: {e}"))?;

    // Block here until the callback signals a double-clap. The `stream`
    // is dropped at scope end, releasing the mic.
    rx.recv().map_err(|e| format!("rx.recv: {e}"))?;
    drop(stream);
    Ok(())
}

fn handle_chunk(
    data: &[f32],
    channels: usize,
    frame_samples: usize,
    pending: &Arc<Mutex<VecDeque<f32>>>,
    detector: &Arc<Mutex<ClapDetector>>,
    tx: &mpsc::SyncSender<()>,
) {
    // Downmix to mono.
    let mut q = pending.lock().unwrap();
    if channels == 1 {
        q.extend(data.iter().copied());
    } else {
        for chunk in data.chunks(channels) {
            let avg = chunk.iter().sum::<f32>() / channels as f32;
            q.push_back(avg);
        }
    }
    // Pull out full frames and run them through the detector.
    while q.len() >= frame_samples {
        let frame: Vec<f32> = q.drain(..frame_samples).collect();
        drop(q); // release before lock-take below

        let rms = {
            let sum_sq: f32 = frame.iter().map(|&s| s * s).sum();
            (sum_sq / frame.len() as f32).sqrt()
        };
        let triggered = detector.lock().unwrap().push(rms, FRAME_MS);
        if triggered {
            println!("[clap-daemon] clap wake — firing");
            // Try to signal once; if the receiver already heard it, ignore.
            let _ = tx.try_send(());
            return;
        }

        q = pending.lock().unwrap();
    }
}

// ---- Main loop -----------------------------------------------------------

fn main() {
    println!("[clap-daemon] starting");

    loop {
        if jarvis_running() {
            // Jarvis is alive — it owns the mic + has its own clap detector.
            // We back off completely (no mic LED, no resource use) and just
            // poll every few seconds for him to exit.
            std::thread::sleep(JARVIS_POLL_INTERVAL);
            continue;
        }

        println!("[clap-daemon] Jarvis is OFF — listening for clap wake…");
        match listen_until_double_clap() {
            Ok(()) => {
                launch_jarvis();
                // Don't immediately poll — give Jarvis time to fully start
                // and claim the mic so the next iteration sees it.
                std::thread::sleep(POST_LAUNCH_DELAY);
            }
            Err(e) => {
                eprintln!("[clap-daemon] listen failed: {e}");
                // Back off so we don't spin if there's no mic device.
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}
