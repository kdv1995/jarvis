//! Voice-activity gating. Turns a stream of 16 kHz frames into discrete
//! *utterances* using energy-based detection with an adaptive noise floor.
//! Also hosts [`ClapDetector`], which watches the same frame stream for a
//! double hand-clap used as an alternative wake trigger.
//!
//! Why energy-based rather than Silero VAD: the only job here is to chop the
//! mic stream into utterance-sized chunks. Whether a chunk is actually
//! addressed to Jarvis is decided downstream by Parakeet transcription + the
//! wake-word match — so the VAD just needs to be a reasonable "someone is
//! talking" gate, and a self-contained energy detector avoids a hard
//! dependency conflict with the ONNX Runtime version `transcribe-rs` uses.
//!
//! Hysteresis: speech starts on a loud frame, ends only after a run of quiet
//! frames (`hangover`), a short pre-roll is prepended so onsets aren't clipped,
//! and utterances shorter than `min_speech` are discarded as noise.

use std::collections::VecDeque;

use crate::audio::{TARGET_RATE, VAD_FRAME};

/// ~32 ms per frame at 16 kHz / 512 samples.
const MS_PER_FRAME: usize = (VAD_FRAME * 1000) / TARGET_RATE;

/// Absolute RMS floor — below this, audio is treated as silence regardless of
/// the adaptive noise floor (guards against the noise floor collapsing to ~0
/// in a very quiet room and then triggering on faint hiss).
const ABS_SILENCE_RMS: f32 = 0.006;

pub struct SpeechGate {
    speaking: bool,
    utterance: Vec<f32>,
    pre_roll: VecDeque<f32>,
    silent_run: usize,
    speech_frames: usize,

    /// Slowly-adapted estimate of the ambient noise RMS.
    noise_floor: f32,

    hangover_frames: usize,
    min_speech_frames: usize,
    pre_roll_samples: usize,
    max_utterance_samples: usize,
}

impl SpeechGate {
    pub fn new() -> Result<Self, String> {
        let frames = |ms: usize| (ms / MS_PER_FRAME).max(1);
        Ok(Self {
            speaking: false,
            utterance: Vec::new(),
            pre_roll: VecDeque::new(),
            silent_run: 0,
            speech_frames: 0,
            noise_floor: ABS_SILENCE_RMS,
            // Hangover: tightened from 800ms → 400ms. The original 800ms was
            // safe for natural speech with long pauses ("uhhh… and then…"),
            // but for short voice commands ("open Claude", "what time is it")
            // it added a perceptible silent tail before processing started.
            // 400ms still gives 1-2 frames of margin for mid-word breaths.
            hangover_frames: frames(400),
            min_speech_frames: frames(250),
            pre_roll_samples: frames(250) * VAD_FRAME,
            max_utterance_samples: 15 * TARGET_RATE,
        })
    }

    /// Feed one [`VAD_FRAME`]-length frame. Returns `Some(utterance)` (16 kHz
    /// mono f32) when a complete utterance has just ended.
    pub fn push_frame(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        let rms = rms(frame);

        // Speech is "clearly above the noise floor", with an absolute guard.
        let start_threshold = (self.noise_floor * 3.5).max(ABS_SILENCE_RMS * 2.0);
        let end_threshold = (self.noise_floor * 2.0).max(ABS_SILENCE_RMS);

        if !self.speaking {
            // Track the ambient noise floor only while idle.
            self.noise_floor = 0.97 * self.noise_floor + 0.03 * rms;

            // Maintain a rolling pre-roll buffer of recent quiet audio.
            self.pre_roll.extend(frame.iter().copied());
            while self.pre_roll.len() > self.pre_roll_samples {
                self.pre_roll.pop_front();
            }

            if rms >= start_threshold {
                self.speaking = true;
                self.silent_run = 0;
                self.speech_frames = 1;
                self.utterance.clear();
                self.utterance.extend(self.pre_roll.drain(..));
                self.utterance.extend_from_slice(frame);
            }
            return None;
        }

        // Currently inside an utterance.
        self.utterance.extend_from_slice(frame);
        if rms < end_threshold {
            self.silent_run += 1;
        } else {
            self.silent_run = 0;
            self.speech_frames += 1;
        }

        let ended = self.silent_run >= self.hangover_frames;
        let overflowed = self.utterance.len() >= self.max_utterance_samples;
        if ended || overflowed {
            self.speaking = false;
            let enough_speech = self.speech_frames >= self.min_speech_frames;
            let dur_ms = self.utterance.len() * 1000 / 16000;
            if enough_speech {
                println!(
                    "[vad] utterance accepted: {dur_ms}ms, {0} speech frames",
                    self.speech_frames
                );
            } else {
                println!(
                    "[vad] utterance DROPPED: {dur_ms}ms, only {0} speech frames (need {1})",
                    self.speech_frames, self.min_speech_frames
                );
            }
            let utterance = std::mem::take(&mut self.utterance);
            self.pre_roll.clear();
            return enough_speech.then_some(utterance);
        }
        None
    }

    /// Whether the gate is currently inside an utterance — speech has been
    /// detected and hasn't ended yet. Drives the HUD's "listening" indicator.
    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    /// Drop any in-progress utterance and reset (used when the orchestrator
    /// transitions away from listening).
    pub fn reset(&mut self) {
        self.speaking = false;
        self.silent_run = 0;
        self.speech_frames = 0;
        self.utterance.clear();
        self.pre_roll.clear();
    }
}

// ---------------------------------------------------------------------------
// ClapDetector — recognises a hand-clap as a wake trigger.
//
// A clap is an impulsive, broadband transient: one ~32 ms frame whose energy
// spikes far above the ambient baseline *and* far above the frames immediately
// before and after it. That "loud frame flanked by quiet frames" test is what
// separates a clap from speech — a speech onset is just as loud but *sustains*
// across many frames. By default, one confirmed clap fires the trigger. Set
// `JARVIS_CLAP_WAKE_MODE=double` if the room is noisy and you prefer the old
// two-clap gesture.
// ---------------------------------------------------------------------------

/// A clap frame's RMS must exceed the adaptive ambient baseline by this factor.
/// Tuned intentionally easy: a missed wake is more annoying than an occasional
/// false wake, and the final wake-word/command flow still gates real actions.
const CLAP_SPIKE_RATIO: f32 = 2.4;
/// ...and must out-shout each neighbouring frame by this factor (the test that
/// rejects sustained speech). Low enough to tolerate soft claps and mic
/// compression smearing the transient across neighbouring frames.
const CLAP_EDGE_RATIO: f32 = 1.3;
/// Absolute RMS floor, so faint hiss can't satisfy the ratio tests in a silent
/// room where the baseline has collapsed toward zero.
const CLAP_ABS_MIN_RMS: f32 = 0.010;
/// Two claps at least this far apart (also rejects a single clap's echo)...
const DOUBLE_CLAP_MIN_MS: usize = 90;
/// ...and at most this far apart count as one double-clap gesture.
const DOUBLE_CLAP_MAX_MS: usize = 1000;

fn single_clap_wake_enabled() -> bool {
    std::env::var("JARVIS_CLAP_WAKE_MODE")
        .map(|v| !v.trim().eq_ignore_ascii_case("double"))
        .unwrap_or(true)
}

pub struct ClapDetector {
    /// RMS of the last three frames: `[oldest, middle, newest]`.
    rms_hist: [f32; 3],
    /// Slowly-adapted ambient RMS, updated only on non-clap frames.
    baseline: f32,
    /// Frame index of the last confirmed single clap (for double-clap timing).
    last_clap_frame: Option<u64>,
    frame_idx: u64,
    /// Frames left to ignore after a clap, so its tail can't re-trigger.
    cooldown: u32,
}

impl ClapDetector {
    pub fn new() -> Self {
        Self {
            rms_hist: [0.0; 3],
            baseline: CLAP_ABS_MIN_RMS,
            last_clap_frame: None,
            frame_idx: 0,
            cooldown: 0,
        }
    }

    /// Feed one [`VAD_FRAME`]-length frame. Returns `true` on the frame that
    /// completes the configured clap gesture. Detection lags real time by one
    /// frame (~32 ms) because the clap is judged on the *middle* of the
    /// three-frame window.
    pub fn push_frame(&mut self, frame: &[f32]) -> bool {
        self.frame_idx += 1;
        // Shift the history window and append the new frame's RMS.
        self.rms_hist[0] = self.rms_hist[1];
        self.rms_hist[1] = self.rms_hist[2];
        self.rms_hist[2] = rms(frame);

        if self.cooldown > 0 {
            self.cooldown -= 1;
            return false;
        }

        let [before, mid, after] = self.rms_hist;

        // The middle frame is a clap if it's loud versus the ambient baseline
        // AND isolated — sharply louder than the frames on either side.
        let is_clap = mid > CLAP_ABS_MIN_RMS
            && mid > self.baseline * CLAP_SPIKE_RATIO
            && mid > before * CLAP_EDGE_RATIO
            && mid > after * CLAP_EDGE_RATIO;

        if !is_clap {
            // Adapt the ambient baseline only on quiet, non-transient frames.
            self.baseline = 0.97 * self.baseline + 0.03 * self.rms_hist[2];
            return false;
        }

        // Confirmed single clap — it occurred on the middle frame, one back.
        self.cooldown = 2;
        let clap_frame = self.frame_idx - 1;
        println!(
            "[clap] clap (rms={:.4}, baseline={:.4})",
            mid, self.baseline
        );

        if single_clap_wake_enabled() {
            return true;
        }

        if let Some(prev) = self.last_clap_frame.take() {
            let gap_ms = (clap_frame - prev) as usize * MS_PER_FRAME;
            if (DOUBLE_CLAP_MIN_MS..=DOUBLE_CLAP_MAX_MS).contains(&gap_ms) {
                return true; // double-clap!
            }
        }
        // First clap (or the gap was out of range) — remember it and wait for
        // a partner clap.
        self.last_clap_frame = Some(clap_frame);
        false
    }

    /// Clear all state (used when the orchestrator stops listening).
    pub fn reset(&mut self) {
        self.rms_hist = [0.0; 3];
        self.last_clap_frame = None;
        self.cooldown = 0;
    }
}

impl Default for ClapDetector {
    fn default() -> Self {
        Self::new()
    }
}

pub fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = frame.iter().map(|&s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt()
}
