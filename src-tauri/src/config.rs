//! Runtime configuration loaded from the environment.
//!
//! Secrets live in a `.env` file at the project root (git-ignored) — see
//! `.env.example` for the keys Jarvis reads. `dotenvy` loads that file into the
//! process environment at startup; in a release build you can also export the
//! variables yourself.

/// ElevenLabs credentials. Used by `stt.rs` for speech-to-text (Scribe v1).
pub struct ElevenLabsConfig {
    pub api_key: String,
}

impl ElevenLabsConfig {
    /// Read the ElevenLabs credentials from the environment.
    ///
    /// Returns `None` unless `ELEVENLABS_API_KEY` is present and non-empty.
    /// Voice/model IDs are not required because TTS is handled by local Kokoro.
    pub fn from_env() -> Option<Self> {
        let api_key = env_non_empty("ELEVENLABS_API_KEY")?;
        Some(Self { api_key })
    }
}

/// Fetch an environment variable, treating empty/whitespace-only as unset.
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
