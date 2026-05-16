//! Shared application state. Managed as `Arc<AppState>` so the command layer
//! can hand clones to background worker threads.

use std::sync::{Mutex, MutexGuard};

use crate::audio::Recorder;
use crate::config::ElevenLabsConfig;

pub struct AppState {
    /// Microphone recorder (start/stop serialized internally).
    pub recorder: Recorder,
    /// ElevenLabs credentials for STT fallback and optional premium TTS.
    pub elevenlabs: Option<ElevenLabsConfig>,
}

impl AppState {
    pub fn new(elevenlabs: Option<ElevenLabsConfig>) -> Self {
        Self {
            recorder: Recorder::new(),
            elevenlabs,
        }
    }
}

/// Poison-safe `Mutex::lock()` for our cross-thread state. A poisoned lock
/// means *another* worker thread panicked while holding it — but the data
/// inside is still readable, just possibly in a transitional state. For a
/// long-running voice loop, "carry on with potentially-stale state" is much
/// better than "the whole app crashes because the TTS thread blew up
/// computing one weird amplitude frame."
///
/// Use everywhere we'd otherwise write `.lock().unwrap()`.
pub trait LockExt<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
