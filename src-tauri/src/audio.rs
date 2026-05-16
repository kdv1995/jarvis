//! Microphone capture for Jarvis.
//!
//! Two capture modes share one cpal stream-building helper:
//!   * [`Recorder`] — manual start/stop, batch-resampled on `stop()`. Backs the
//!     "TALK TO JARVIS" button.
//!   * [`Listener`] — continuous capture for hands-free operation. Resamples in
//!     the audio callback and streams fixed 512-sample 16 kHz frames out a
//!     channel, ready for Silero VAD.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::state::LockExt;

pub const TARGET_RATE: usize = 16_000;
/// Frame size handed to the VAD — 512 samples = 32 ms at 16 kHz, the size the
/// Silero model expects.
pub const VAD_FRAME: usize = 512;

// ---------------------------------------------------------------------------
// Shared cpal stream construction
// ---------------------------------------------------------------------------

/// Describes the input device we opened.
struct InputInfo {
    rate: u32,
    channels: usize,
    format: cpal::SampleFormat,
    config: cpal::StreamConfig,
}

/// Pick the input device Jarvis should listen on. Strategy:
///   1. `JARVIS_MIC_DEVICE` env var (substring match against device names) —
///      lets you force any specific mic.
///   2. Otherwise prefer a HyperX / QuadCast (the user's external mic) over
///      the built-in laptop microphone.
///   3. Otherwise fall back to the system default input device.
///
/// Substring match is case-insensitive. On the first run, every available
/// device name is logged so you can copy the right substring into the env
/// var if auto-pick chose wrong.
fn open_default_input() -> Result<(cpal::Device, InputInfo), String> {
    let host = cpal::default_host();

    // Enumerate every available input — for logging and for the picker.
    let inputs: Vec<cpal::Device> = host
        .input_devices()
        .map_err(|e| format!("could not list input devices: {e}"))?
        .collect();
    if inputs.is_empty() {
        return Err("no input devices found".into());
    }
    println!("[audio] available input devices:");
    for d in &inputs {
        let name = d.name().unwrap_or_else(|_| "<unnamed>".to_string());
        println!("[audio]   - {name}");
    }

    // Step 1: explicit env override.
    let explicit = std::env::var("JARVIS_MIC_DEVICE")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty());
    if let Some(needle) = explicit.as_deref() {
        if let Some(picked) = inputs.iter().find(|d| {
            d.name()
                .map(|n| n.to_lowercase().contains(needle))
                .unwrap_or(false)
        }) {
            return finalize_input(picked.clone(), "JARVIS_MIC_DEVICE");
        }
        eprintln!(
            "[audio] JARVIS_MIC_DEVICE={needle:?} didn't match any device — falling back to auto-pick"
        );
    }

    // Step 2: prefer HyperX / QuadCast over built-in.
    for needle in &["hyperx", "quadcast"] {
        if let Some(picked) = inputs.iter().find(|d| {
            d.name()
                .map(|n| n.to_lowercase().contains(needle))
                .unwrap_or(false)
        }) {
            return finalize_input(picked.clone(), needle);
        }
    }

    // Step 3: fall back to the OS default.
    let device = host
        .default_input_device()
        .ok_or("no default input device")?;
    finalize_input(device, "system default")
}

fn finalize_input(device: cpal::Device, source: &str) -> Result<(cpal::Device, InputInfo), String> {
    let name = device.name().unwrap_or_else(|_| "<unnamed>".to_string());
    println!("[audio] using input: {name}  (picked via: {source})");
    let config = device
        .default_input_config()
        .map_err(|e| format!("no default input config for {name}: {e}"))?;
    let info = InputInfo {
        rate: config.sample_rate().0,
        channels: config.channels() as usize,
        format: config.sample_format(),
        config: config.into(),
    };
    Ok((device, info))
}

/// Build a cpal input stream that downmixes every frame to mono and hands the
/// mono samples (still at the device's native rate) to `on_mono`.
fn build_input_stream<F>(
    device: &cpal::Device,
    info: &InputInfo,
    mut on_mono: F,
) -> Result<cpal::Stream, String>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    let channels = info.channels;
    let err_fn = |e| eprintln!("[jarvis] audio stream error: {e}");
    let mut scratch: Vec<f32> = Vec::with_capacity(2048);

    macro_rules! mono_callback {
        ($sample_ty:ty, $to_f32:expr) => {
            move |data: &[$sample_ty], _: &_| {
                scratch.clear();
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|&s| $to_f32(s)).sum();
                    scratch.push(sum / channels as f32);
                }
                on_mono(&scratch);
            }
        };
    }

    let stream = match info.format {
        cpal::SampleFormat::F32 => {
            device.build_input_stream(&info.config, mono_callback!(f32, |s: f32| s), err_fn, None)
        }
        cpal::SampleFormat::I16 => device.build_input_stream(
            &info.config,
            mono_callback!(i16, |s: i16| s as f32 / i16::MAX as f32),
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &info.config,
            mono_callback!(u16, |s: u16| (s as f32 - 32768.0) / 32768.0),
            err_fn,
            None,
        ),
        other => return Err(format!("unsupported sample format: {other:?}")),
    };
    stream.map_err(|e| format!("failed to build input stream: {e}"))
}

// ---------------------------------------------------------------------------
// Streaming resampler (native rate -> 16 kHz, fixed-size frames)
// ---------------------------------------------------------------------------

/// Resamples a continuous mono stream to 16 kHz and emits fixed-length frames.
pub struct StreamingResampler {
    resampler: Option<rubato::FftFixedIn<f32>>,
    chunk_in: usize,
    in_buf: Vec<f32>,
    frame_len: usize,
    pending: Vec<f32>,
}

impl StreamingResampler {
    const CHUNK_IN: usize = 1024;

    pub fn new(in_rate: usize, frame_len: usize) -> Self {
        use rubato::FftFixedIn;
        let resampler = (in_rate != TARGET_RATE).then(|| {
            FftFixedIn::<f32>::new(in_rate, TARGET_RATE, Self::CHUNK_IN, 1, 1)
                .expect("failed to create resampler")
        });
        Self {
            resampler,
            chunk_in: Self::CHUNK_IN,
            in_buf: Vec::with_capacity(Self::CHUNK_IN),
            frame_len,
            pending: Vec::with_capacity(frame_len),
        }
    }

    /// Push native-rate mono samples; `emit` is called for each full 16 kHz frame.
    pub fn push(&mut self, mut src: &[f32], mut emit: impl FnMut(&[f32])) {
        use rubato::Resampler;

        if self.resampler.is_none() {
            // Already at 16 kHz — just reframe.
            self.reframe(src, &mut emit);
            return;
        }

        while !src.is_empty() {
            let space = self.chunk_in - self.in_buf.len();
            let take = space.min(src.len());
            self.in_buf.extend_from_slice(&src[..take]);
            src = &src[take..];

            if self.in_buf.len() == self.chunk_in {
                // Re-borrow the resampler per chunk so the `&mut self` borrow
                // ends with `process()` — `reframe` needs `&mut self` too.
                let out = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .process(&[&self.in_buf[..]], None);
                self.in_buf.clear();
                if let Ok(mut out) = out {
                    let frame = out.swap_remove(0);
                    self.reframe(&frame, &mut emit);
                }
            }
        }
    }

    fn reframe(&mut self, mut data: &[f32], emit: &mut impl FnMut(&[f32])) {
        while !data.is_empty() {
            let space = self.frame_len - self.pending.len();
            let take = space.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() == self.frame_len {
                emit(&self.pending);
                self.pending.clear();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Recorder — manual start/stop (the "TALK TO JARVIS" button)
// ---------------------------------------------------------------------------

pub struct Recorder {
    active: Mutex<Option<Active>>,
}

struct Active {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<f32>>>,
    input_rate: u32,
    thread: JoinHandle<()>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    /// Whether a recording is currently in progress.
    #[allow(dead_code)] // used by the orchestrator state machine
    pub fn is_recording(&self) -> bool {
        self.active.lock_recover().is_some()
    }

    pub fn start(&self) -> Result<(), String> {
        let mut guard = self.active.lock_recover();
        if guard.is_some() {
            return Err("already recording".into());
        }

        let stop = Arc::new(AtomicBool::new(false));
        let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();

        let thread = {
            let stop = stop.clone();
            let samples = samples.clone();
            std::thread::spawn(move || {
                let (device, info) = match open_default_input() {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let rate = info.rate;
                let sink = samples.clone();
                let stream = match build_input_stream(&device, &info, move |mono| {
                    sink.lock_recover().extend_from_slice(mono);
                }) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(format!("failed to start stream: {e}")));
                    return;
                }
                let _ = ready_tx.send(Ok(rate));
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(stream);
            })
        };

        match ready_rx.recv() {
            Ok(Ok(input_rate)) => {
                *guard = Some(Active {
                    stop,
                    samples,
                    input_rate,
                    thread,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("audio thread exited before signalling readiness".into()),
        }
    }

    /// Stop capturing and return the recording as 16 kHz mono f32 samples.
    pub fn stop(&self) -> Result<Vec<f32>, String> {
        let active = self.active.lock_recover().take().ok_or("not recording")?;
        active.stop.store(true, Ordering::Relaxed);
        let _ = active.thread.join();
        let raw = std::mem::take(&mut *active.samples.lock_recover());
        Ok(resample_batch_to_16k(&raw, active.input_rate))
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

/// Batch-resample a complete buffer to 16 kHz (used by [`Recorder::stop`]).
fn resample_batch_to_16k(input: &[f32], input_rate: u32) -> Vec<f32> {
    if input.is_empty() || input_rate as usize == TARGET_RATE {
        return input.to_vec();
    }
    let mut resampler = StreamingResampler::new(input_rate as usize, VAD_FRAME);
    let mut out = Vec::with_capacity(input.len() * TARGET_RATE / input_rate.max(1) as usize);
    resampler.push(input, |frame| out.extend_from_slice(frame));
    out
}

// ---------------------------------------------------------------------------
// Listener — continuous capture for hands-free operation
// ---------------------------------------------------------------------------

/// Command the audio thread accepts from the rest of the app. cpal::Stream
/// isn't `Send` on macOS (it holds CoreAudio resources tied to a thread), so
/// we can't move it across threads — we communicate via this channel and the
/// owning thread acts on commands inside its own scope.
enum AudioCmd {
    Pause,
    Resume,
}

pub struct Listener {
    stop: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<AudioCmd>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Start continuous capture. 16 kHz mono frames of [`VAD_FRAME`] samples are
    /// sent on `frame_tx` until the listener is dropped or `stop()`-ed.
    pub fn start(frame_tx: mpsc::Sender<Vec<f32>>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();

        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (device, info) = match open_default_input() {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                println!(
                    "[audio] mic: rate={}Hz channels={} format={:?}",
                    info.rate, info.channels, info.format
                );
                let mut resampler = StreamingResampler::new(info.rate as usize, VAD_FRAME);
                let tx = frame_tx.clone();
                let stream = match build_input_stream(&device, &info, move |mono| {
                    resampler.push(mono, |frame| {
                        // A send error means the consumer is gone — ignore;
                        // the stop flag will tear this thread down.
                        let _ = tx.send(frame.to_vec());
                    });
                }) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(format!("failed to start stream: {e}")));
                    return;
                }
                let _ = ready_tx.send(Ok(()));

                // Main loop: poll for stop and command messages. Pause/Resume
                // go through cpal at the driver level — when paused the OS
                // stops feeding samples to our callback entirely, the mic
                // LED turns off on hardware that has one, and no frames
                // queue up. Resume restarts capture in ~10-30 ms.
                while !stop.load(Ordering::Relaxed) {
                    match cmd_rx.try_recv() {
                        Ok(AudioCmd::Pause) => {
                            if let Err(e) = stream.pause() {
                                eprintln!("[audio] stream.pause() failed: {e}");
                            } else {
                                println!("[audio] mic PAUSED (driver-level)");
                            }
                        }
                        Ok(AudioCmd::Resume) => {
                            if let Err(e) = stream.play() {
                                eprintln!("[audio] stream.play() failed: {e}");
                            } else {
                                println!("[audio] mic RESUMED");
                            }
                        }
                        Err(_) => {} // no command — keep looping
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(stream);
            })
        };

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                cmd_tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("listener thread exited before signalling readiness".into()),
        }
    }

    /// Stop capturing audio at the cpal/driver level. On supported hardware
    /// this also turns the mic indicator LED off. Resume() to restart.
    pub fn pause(&self) {
        let _ = self.cmd_tx.send(AudioCmd::Pause);
    }

    /// Restart audio capture after [`pause`]. Safe to call when already
    /// running (cpal::Stream::play is idempotent).
    pub fn resume(&self) {
        let _ = self.cmd_tx.send(AudioCmd::Resume);
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
