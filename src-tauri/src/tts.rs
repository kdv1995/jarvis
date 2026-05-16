//! Text-to-speech with barge-in support.
//!
//! Primary voice: ElevenLabs TTS when `ELEVENLABS_API_KEY` is configured, using
//! a professional Ukrainian conversational voice by default. Kokoro remains the
//! local fallback and can be forced with `JARVIS_TTS_ENGINE=kokoro`.
//!
//! While WAV audio is playing, a side thread streams real `hud://amplitude`
//! events to the webview at 20 Hz — pre-computed RMS envelope synchronized to
//! playback time — so the hologram's mouth follows the actual TTS voice.
//!
//! **Barge-in**: the `afplay` (or `say`) child process is *spawned* (not
//! `status()`-ed) and registered in a shared [`Mutex`] slot supplied by the
//! caller. The listener loop watches the mic during playback; when it detects
//! the user speaking over Jarvis, it `.take()`s the child out of the slot and
//! `.kill()`s it. The poll loop in [`run_player`] sees the empty slot and
//! returns `Err("TTS interrupted")` — Jarvis stops talking mid-sentence and
//! the orchestrator drops back to idle, ready to capture the interrupt.
//!
//! Playback goes through [`run_player`] so it stays interruptible by barge-in.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};

use crate::state::LockExt;

/// How often the playback poll loop checks the child for completion / kill.
/// Small enough that a barge-in feels instant (~50 ms latency).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

const DEFAULT_ELEVENLABS_TTS_VOICE_ID: &str = "h9NSQvWZaC4NFusYsxT9";
const DEFAULT_ELEVENLABS_TTS_VOICE_NAME: &str = "Artem Klopotenko - Podcast Pro";
const DEFAULT_ELEVENLABS_TTS_MODEL: &str = "eleven_multilingual_v2";

/// Speak one chunk of text, blocking until playback finishes *or* is interrupted
/// via the shared `tts_slot`. Designed to be called in a loop as streaming text
/// blocks arrive from `agent::run_claude_streaming` — each call queues naturally
/// behind the previous because `run_player` holds the lock until afplay exits.
///
/// Drives `hud://amplitude` events for real-time mouth lip-sync when the audio
/// format is WAV. ElevenLabs defaults to MP3 for broad plan compatibility, so
/// the mouth still closes cleanly but does not get a computed envelope.
pub fn speak_sentence(
    text: &str,
    app: &AppHandle,
    tts_slot: &Arc<Mutex<Option<Child>>>,
) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }

    if should_use_elevenlabs() {
        match speak_elevenlabs(text, app, tts_slot) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("[jarvis] ElevenLabs TTS failed ({e}); falling back to Kokoro");
            }
        }
    }

    speak_kokoro(text, app, tts_slot).map_err(|e| {
        eprintln!("[jarvis] Kokoro TTS failed ({e}). Jarvis will stay silent.");
        e
    })
}

pub fn voice_label() -> &'static str {
    if should_use_elevenlabs() {
        "ElevenLabs Artem Klopotenko"
    } else {
        "Kokoro bm_george"
    }
}

fn should_use_elevenlabs() -> bool {
    match std::env::var("JARVIS_TTS_ENGINE")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .as_deref()
    {
        Some("kokoro") | Some("local") => false,
        Some("elevenlabs") | Some("premium") => true,
        Some(other) => {
            eprintln!("[jarvis] unknown JARVIS_TTS_ENGINE={other:?}; using auto");
            elevenlabs_api_key().is_some()
        }
        None => elevenlabs_api_key().is_some(),
    }
}

fn elevenlabs_api_key() -> Option<String> {
    std::env::var("ELEVENLABS_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn elevenlabs_tts_voice_id() -> String {
    env_non_empty("JARVIS_ELEVENLABS_TTS_VOICE_ID")
        .or_else(|| env_non_empty("ELEVENLABS_TTS_VOICE_ID"))
        .unwrap_or_else(|| DEFAULT_ELEVENLABS_TTS_VOICE_ID.to_string())
}

fn elevenlabs_tts_model() -> String {
    env_non_empty("JARVIS_ELEVENLABS_TTS_MODEL_ID")
        .or_else(|| env_non_empty("ELEVENLABS_TTS_MODEL_ID"))
        .or_else(|| env_non_empty("ELEVENLABS_MODEL_ID"))
        .unwrap_or_else(|| DEFAULT_ELEVENLABS_TTS_MODEL.to_string())
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Synthesize `text` with ElevenLabs, then play the returned audio.
fn speak_elevenlabs(
    text: &str,
    app: &AppHandle,
    tts_slot: &Arc<Mutex<Option<Child>>>,
) -> Result<(), String> {
    let api_key = elevenlabs_api_key().ok_or("missing ELEVENLABS_API_KEY")?;
    let voice_id = elevenlabs_tts_voice_id();
    let model_id = elevenlabs_tts_model();
    let latency = env_non_empty("JARVIS_ELEVENLABS_OPTIMIZE_LATENCY");
    let mut url = format!(
        "https://api.elevenlabs.io/v1/text-to-speech/{voice_id}?output_format=mp3_44100_128"
    );
    if let Some(latency) = latency {
        url.push_str("&optimize_streaming_latency=");
        url.push_str(&latency);
    }

    let mut body = serde_json::json!({
        "text": text,
        "model_id": model_id,
        "voice_settings": {
            "stability": 0.45,
            "similarity_boost": 0.82,
            "style": 0.18,
            "use_speaker_boost": true
        }
    });
    if let Some(language_code) = env_non_empty("JARVIS_TTS_LANG") {
        body["language_code"] = serde_json::json!(language_code);
    }

    let response = reqwest::blocking::Client::new()
        .post(url)
        .header("xi-api-key", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("ElevenLabs returned {status}: {}", body.trim()));
    }

    let audio = response
        .bytes()
        .map_err(|e| format!("could not read audio body: {e}"))?;

    println!(
        "[tts] ElevenLabs voice: {DEFAULT_ELEVENLABS_TTS_VOICE_NAME} ({voice_id}), model={model_id}"
    );
    play_audio(&audio, "mp3", app, tts_slot)
}

/// Synthesize `text` with the local Kokoro server, then play the WAV while a
/// side thread streams pre-computed amplitude levels to the webview.
fn speak_kokoro(
    text: &str,
    app: &AppHandle,
    tts_slot: &Arc<Mutex<Option<Child>>>,
) -> Result<(), String> {
    let response = reqwest::blocking::Client::new()
        .post("http://localhost:11435/speak")
        .json(&serde_json::json!({ "text": text }))
        .send()
        .map_err(|e| format!("request failed (is the Kokoro server running?): {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("Kokoro server returned {status}: {}", body.trim()));
    }

    let audio = response
        .bytes()
        .map_err(|e| format!("could not read audio body: {e}"))?;

    play_audio(&audio, "wav", app, tts_slot)
}

fn play_audio(
    audio: &[u8],
    extension: &str,
    app: &AppHandle,
    tts_slot: &Arc<Mutex<Option<Child>>>,
) -> Result<(), String> {
    let envelope = compute_amplitude_envelope(audio);
    let path = std::env::temp_dir().join(format!("jarvis-tts.{extension}"));
    std::fs::write(&path, audio).map_err(|e| format!("could not write temp audio: {e}"))?;
    // Spawn the amplitude emitter. The stop_flag lets us cut it off cleanly
    // when playback ends (naturally or via barge-in) so it doesn't keep
    // emitting events into a dead playback.
    //
    // **Sync gotcha**: this thread starts BEFORE afplay is spawned, and even
    // after spawn, afplay needs ~100 ms to decode the WAV header and emit
    // the first audible sample. Without compensation the hologram's mouth
    // opens visibly before any sound — looks like Jarvis is mouthing words
    // for a moment in silence. `AFPLAY_STARTUP_DELAY_MS` shifts the emitter
    // timeline to match audible audio. Tune empirically if mouth feels
    // ahead/behind.
    const AFPLAY_STARTUP_DELAY_MS: u64 = 120;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let app_clone = app.clone();
    let stop_clone = stop_flag.clone();
    let emitter = std::thread::spawn(move || {
        // Hold the mouth closed for the first ~120 ms while afplay warms up.
        // Bail early if barge-in fires during the wait.
        let warmup_until = Instant::now() + Duration::from_millis(AFPLAY_STARTUP_DELAY_MS);
        while Instant::now() < warmup_until {
            if stop_clone.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Anchor the timeline AFTER the warmup so envelope t=0 aligns with
        // the first audible audio sample, not with thread spawn.
        let start = Instant::now();
        for (t_ms, amp) in envelope {
            if stop_clone.load(Ordering::Relaxed) {
                break;
            }
            let elapsed = start.elapsed().as_millis() as u64;
            let target = t_ms as u64;
            if target > elapsed {
                std::thread::sleep(Duration::from_millis(target - elapsed));
            }
            if stop_clone.load(Ordering::Relaxed) {
                break;
            }
            let _ = app_clone.emit(
                "hud://amplitude",
                serde_json::json!({ "mic": 0.0f32, "tts": amp }),
            );
        }
        // Always close the mouth at end-of-play, however we got here.
        let _ = app_clone.emit(
            "hud://amplitude",
            serde_json::json!({ "mic": 0.0f32, "tts": 0.0f32 }),
        );
    });

    let mut cmd = Command::new("/usr/bin/afplay");
    cmd.arg(&path);
    let result = run_player(tts_slot, cmd, "afplay");

    // Tell the emitter to stop, then join. If playback was killed mid-clip
    // the emitter exits on its next loop iteration; on a normal end it's
    // already done.
    stop_flag.store(true, Ordering::Relaxed);
    let _ = emitter.join();

    result
}

/// Parse a 16-bit PCM WAV blob and return `(timestamp_ms, normalized_level)`
/// pairs sampled at 20 Hz (50 ms windows).
///
/// Walks the WAV chunk list to find `data` rather than assuming it sits at
/// byte 44 — Kokoro's serialized WAVs are well-formed, but the parser is just
/// as cheap and survives a future server upgrade that adds a LIST/INFO chunk.
///
/// The output is peak-normalized so the loudest window in the clip maps to
/// 1.0; that way a quiet phrase still drives the hologram's mouth visibly.
fn compute_amplitude_envelope(wav: &[u8]) -> Vec<(u32, f32)> {
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Vec::new();
    }

    let channels = u16::from_le_bytes([wav[22], wav[23]]) as usize;
    let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
    let bits_per_sample = u16::from_le_bytes([wav[34], wav[35]]);
    if bits_per_sample != 16 || channels == 0 || sample_rate == 0 {
        return Vec::new();
    }

    let mut data_start = 0usize;
    let mut data_len = 0usize;
    let mut i = 12usize;
    while i + 8 <= wav.len() {
        let chunk_id = &wav[i..i + 4];
        let chunk_size =
            u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        if chunk_id == b"data" {
            data_start = i + 8;
            data_len = chunk_size.min(wav.len().saturating_sub(data_start));
            break;
        }
        i += 8 + chunk_size + (chunk_size & 1);
    }
    if data_start == 0 || data_len == 0 {
        return Vec::new();
    }

    let samples = &wav[data_start..data_start + data_len];
    let bytes_per_frame = 2 * channels;
    let total_frames = samples.len() / bytes_per_frame;

    let window_ms = 50u32;
    let window_frames = (sample_rate as usize * window_ms as usize) / 1000;
    if window_frames == 0 || total_frames < window_frames {
        return Vec::new();
    }
    let num_windows = total_frames / window_frames;

    let mut rmss: Vec<f32> = Vec::with_capacity(num_windows);
    for w in 0..num_windows {
        let mut sum_sq = 0.0f64;
        for f in 0..window_frames {
            let idx = (w * window_frames + f) * bytes_per_frame;
            let s = i16::from_le_bytes([samples[idx], samples[idx + 1]]) as f64 / 32768.0;
            sum_sq += s * s;
        }
        rmss.push(((sum_sq / window_frames as f64).sqrt()) as f32);
    }

    let peak = rmss.iter().cloned().fold(0.0f32, f32::max).max(0.05);
    rmss.into_iter()
        .enumerate()
        .map(|(w, rms)| ((w as u32) * window_ms, (rms / peak).clamp(0.0, 1.0)))
        .collect()
}

/// Spawn `cmd` as a child, register it in `tts_slot`, and poll for completion.
/// Returns `Err("TTS interrupted")` if the slot is emptied externally (i.e.
/// barge-in) — that's the cancellation signal.
fn run_player(
    tts_slot: &Arc<Mutex<Option<Child>>>,
    mut cmd: Command,
    label: &str,
) -> Result<(), String> {
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch `{label}`: {e}"))?;

    *tts_slot.lock_recover() = Some(child);

    // Poll loop. We *release* the lock between checks so the barge-in killer
    // can `.take()` the child out without contending with us.
    loop {
        std::thread::sleep(POLL_INTERVAL);

        let result = {
            let mut guard = tts_slot.lock_recover();
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        Some(if status.success() {
                            Ok(())
                        } else {
                            Err(format!("`{label}` exited with {status}"))
                        })
                    }
                    Ok(None) => None, // still running — release lock and sleep again
                    Err(e) => {
                        *guard = None;
                        Some(Err(format!("`{label}` wait failed: {e}")))
                    }
                },
                None => Some(Err("TTS interrupted".into())),
            }
        };

        if let Some(r) = result {
            return r;
        }
    }
}
