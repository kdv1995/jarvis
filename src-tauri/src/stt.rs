//! Speech-to-text with two engines:
//!
//! 1. **Local Whisper (primary)** — POSTs the WAV to a persistent
//!    `faster-whisper` server on `localhost:11436` (the `whisper-stt` Python
//!    server, parallel to the Kokoro TTS server on :11435). Fully offline,
//!    multilingual auto-detect, no API cost. With the `small` model and
//!    `beam_size=1` it does a 2 s utterance in ~1.4 s on M2 Pro CPU int8.
//!
//! 2. **ElevenLabs Scribe (fallback)** — used if the local server is down
//!    or returns an error. Cloud API call, requires `ELEVENLABS_API_KEY`.
//!
//! Set `JARVIS_STT_ENGINE=elevenlabs` in `.env` to force-skip local Whisper.
//!
//! **Language**: read from `JARVIS_STT_LANG`. Accepts either ISO-639-1
//! 2-letter codes (`en`, `uk`, `ru`) or ISO-639-3 3-letter codes (`eng`,
//! `ukr`, `rus`) — both are normalised internally to the format each engine
//! expects. `auto` (or unset) means auto-detect via both engines.

use std::time::Duration;

use crate::config::ElevenLabsConfig;

/// Local Whisper server endpoint (see `whisper-stt/server.py`).
const WHISPER_URL: &str = "http://127.0.0.1:11436/transcribe";

/// Transcribe 16 kHz mono f32 samples to text. Tries local Whisper first,
/// falls back to ElevenLabs on any failure (server down, timeout, malformed
/// response). Both engines honour `JARVIS_STT_LANG`.
pub fn transcribe(samples: &[f32], config: &ElevenLabsConfig) -> Result<String, String> {
    if samples.is_empty() {
        return Ok(String::new());
    }

    let normalized = normalize_audio(samples);
    let wav = encode_wav_16k_mono(&normalized);

    // Debug: save the WAV so we can inspect audio quality
    let _ = std::fs::write("/tmp/jarvis_last_utterance.wav", &wav);
    println!(
        "[stt] saved {}-byte WAV ({} samples, {:.1}s) to /tmp/jarvis_last_utterance.wav",
        wav.len(),
        samples.len(),
        samples.len() as f64 / 16000.0
    );

    let engine = std::env::var("JARVIS_STT_ENGINE")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .unwrap_or_else(|| "whisper".to_string());

    // Try local Whisper unless the user explicitly chose ElevenLabs.
    if engine != "elevenlabs" {
        let t = std::time::Instant::now();
        match transcribe_local(&wav) {
            Ok(text) => {
                println!("[stt] local whisper → {:?} ({:?})", text, t.elapsed());
                return Ok(text);
            }
            Err(e) => {
                eprintln!(
                    "[stt] local whisper failed ({e}) — falling back to ElevenLabs. \
                     Is whisper-stt/server.py running on :11436?"
                );
            }
        }
    }

    transcribe_elevenlabs(wav, config)
}

/// POST to the local `whisper-stt` HTTP server.
fn transcribe_local(wav: &[u8]) -> Result<String, String> {
    let mut req = reqwest::blocking::Client::new()
        .post(WHISPER_URL)
        .body(wav.to_vec())
        .header("Content-Type", "audio/wav")
        // Short timeout: if local server is down or stuck we want to fall
        // back to ElevenLabs fast, not hang the voice command for 30 s.
        .timeout(Duration::from_secs(10));
    if let Some(lang) = whisper_lang_code() {
        req = req.header("X-Language", lang);
    }
    let resp = req
        .send()
        .map_err(|e| format!("local whisper unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("local whisper returned {}", resp.status()));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| format!("local whisper response parse: {e}"))?;
    let raw = json["text"].as_str().unwrap_or("").trim().to_string();
    // Drop obvious Whisper hallucinations — non-speech audio (room noise,
    // breathing, clap echo) regularly produces phonetic gibberish like
    // "ʕ ʔ ʔ ʔ ʔ" or canned phrases ("Thank you for watching.", "you", etc.).
    // Returning "" makes the pipeline treat this as "I didn't catch that"
    // and stay in AwaitingCommand instead of trying to execute the garbage.
    if looks_like_hallucination(&raw) {
        println!("[stt] dropped likely hallucination: {raw:?}");
        return Ok(String::new());
    }
    Ok(raw)
}

/// Heuristic filter for Whisper non-speech hallucinations. Catches two
/// categories:
///   1. **Phonetic-gibberish**: transcripts where less than ~50 % of
///      characters are ASCII letters/digits/spaces (Whisper falls into IPA
///      symbols and Korean glyphs when fed pure noise).
///   2. **Canned phrases**: a small list of known-bad Whisper hallucinations
///      that appear when transcribing silence or music (verified in the
///      whisper community as the most common false positives).
///
/// Empty strings pass through unchanged — they're handled separately in the
/// pipeline as a clean "no speech" signal.
fn looks_like_hallucination(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }

    // 1. Character-quality heuristic. Skip for very short text (would
    // false-positive on legit "yes" / "no" / "ok").
    let total = t.chars().count();
    if total >= 4 {
        let ascii_friendly = t
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || ".,!?'-".contains(*c))
            .count();
        let ratio = (ascii_friendly * 100) / total;
        if ratio < 50 {
            return true;
        }
    }

    // 2. Known Whisper hallucination phrases (lower-cased, punctuation-stripped).
    let canonical = t
        .to_lowercase()
        .trim_end_matches(['.', '!', '?', ',', ' '])
        .trim()
        .to_string();
    const CANNED: &[&str] = &[
        "you",
        "thank you",
        "thanks",
        "thank you for watching",
        "thank you so much",
        "thanks for watching",
        "thanks for listening",
        "thank you for listening",
        "subtitles by the amara.org community",
        "subtitled by",
        "subtitles by",
        "subtitle by",
        "bye",
        "bye bye",
        "okay",
        "ok",
        "[music]",
        "[applause]",
        "[laughter]",
        "music",
        ".",
        "..",
        "...",
    ];
    CANNED.contains(&canonical.as_str())
}

/// Compute the language code to send to local Whisper. Returns `None` for
/// auto-detect. Maps 3-letter ISO codes (ElevenLabs convention) to 2-letter
/// (Whisper convention).
fn whisper_lang_code() -> Option<String> {
    let raw = std::env::var("JARVIS_STT_LANG")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())?;
    if raw == "auto" {
        return None;
    }
    Some(match raw.as_str() {
        "eng" | "en" => "en".into(),
        "ukr" | "uk" => "uk".into(),
        "rus" | "ru" => "ru".into(),
        "spa" | "es" => "es".into(),
        "fra" | "fre" | "fr" => "fr".into(),
        "deu" | "ger" | "de" => "de".into(),
        "ita" | "it" => "it".into(),
        "por" | "pt" => "pt".into(),
        "jpn" | "ja" => "ja".into(),
        "kor" | "ko" => "ko".into(),
        "zho" | "chi" | "zh" => "zh".into(),
        "pol" | "pl" => "pl".into(),
        "nld" | "dut" | "nl" => "nl".into(),
        other => other.to_string(), // unknown codes — pass through and hope
    })
}

fn transcribe_elevenlabs(wav: Vec<u8>, config: &ElevenLabsConfig) -> Result<String, String> {
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| format!("mime error: {e}"))?;

    let lang = std::env::var("JARVIS_STT_LANG")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "eng".to_string());

    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", part)
        .text("model_id", "scribe_v1")
        .text("tag_audio_events", "false")
        .text("diarize", "false");
    if lang != "auto" {
        // Normalize 2-letter → 3-letter for ElevenLabs convention.
        let eleven_lang = match lang.as_str() {
            "en" => "eng".into(),
            "uk" => "ukr".into(),
            "ru" => "rus".into(),
            "es" => "spa".into(),
            "fr" => "fra".into(),
            "de" => "deu".into(),
            "it" => "ita".into(),
            "pt" => "por".into(),
            "ja" => "jpn".into(),
            "ko" => "kor".into(),
            "zh" => "zho".into(),
            other => other.to_string(),
        };
        form = form.text("language_code", eleven_lang);
    }
    println!(
        "[stt] elevenlabs language: {}",
        if lang == "auto" {
            "auto-detect"
        } else {
            lang.as_str()
        }
    );

    let resp = reqwest::blocking::Client::new()
        .post("https://api.elevenlabs.io/v1/speech-to-text")
        .header("xi-api-key", &config.api_key)
        .multipart(form)
        .send()
        .map_err(|e| format!("STT request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(format!("STT API returned {status}: {}", body.trim()));
    }

    let json: serde_json::Value = resp
        .json()
        .map_err(|e| format!("could not parse STT response: {e}"))?;

    let text = json["text"].as_str().unwrap_or("").trim().to_string();

    Ok(text)
}

/// Normalize audio so the peak reaches ~80% of full scale.
/// Mic input on macOS is often very quiet; without this the STT model
/// can barely hear the speech.
fn normalize_audio(samples: &[f32]) -> Vec<f32> {
    let peak = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    if peak < 1e-6 {
        return samples.to_vec();
    }
    let target_peak = 0.8;
    let gain = (target_peak / peak).min(20.0); // cap at 20x to avoid amplifying pure noise
    println!("[stt] audio normalize: peak={:.4}, gain={:.1}x", peak, gain);
    samples
        .iter()
        .map(|&s| (s * gain).clamp(-1.0, 1.0))
        .collect()
}

/// Encode f32 samples (16 kHz, mono) as a 16-bit PCM WAV in memory.
fn encode_wav_16k_mono(samples: &[f32]) -> Vec<u8> {
    let num_samples = samples.len();
    let data_len = num_samples * 2; // 16-bit = 2 bytes per sample
    let file_len = 36 + data_len;

    let mut buf = Vec::with_capacity(44 + data_len);

    // RIFF header
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(file_len as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");

    // fmt chunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&16000u32.to_le_bytes()); // sample rate
    buf.extend_from_slice(&32000u32.to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i16_val = (clamped * 32767.0) as i16;
        buf.extend_from_slice(&i16_val.to_le_bytes());
    }

    buf
}

// ---------------------------------------------------------------------------
// Unit tests for the hallucination filter.
//
// The filter has two layers: (1) char-quality heuristic for phonetic
// gibberish (Whisper transcribing background noise as e.g. "ʕʔʔʔ"), and
// (2) a canned-phrase blocklist for Whisper's well-known stock phrases
// ("Thank you for watching.", subtitle credits, etc.). Tests pin both.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::looks_like_hallucination;

    #[test]
    fn allows_legitimate_short_utterance() {
        // "yes" / "no" pass — they're below the char-quality threshold AND
        // not on the canned blocklist. "ok" / "okay" are DELIBERATELY on
        // the blocklist (see `rejects_known_whisper_canned_phrases`)
        // because Whisper hallucinates them on near-silence far too often
        // to trust them as real commands.
        assert!(!looks_like_hallucination("yes"));
        assert!(!looks_like_hallucination("no"));
        assert!(!looks_like_hallucination("hi"));
    }

    #[test]
    fn rejects_short_canned_phrases() {
        // Pinning the intentional blocklist of short ambiguous tokens.
        // If these are ever moved off the list, this test will catch it
        // (and `allows_legitimate_short_utterance` will need updating).
        assert!(looks_like_hallucination("ok"));
        assert!(looks_like_hallucination("okay"));
        assert!(looks_like_hallucination("bye"));
    }

    #[test]
    fn allows_real_speech() {
        assert!(!looks_like_hallucination("what time is it"));
        assert!(!looks_like_hallucination("open Chrome"));
        assert!(!looks_like_hallucination(
            "Jarvis, can you remind me about the meeting?"
        ));
    }

    #[test]
    fn rejects_known_whisper_canned_phrases() {
        assert!(looks_like_hallucination("Thank you for watching."));
        assert!(looks_like_hallucination("thanks for watching"));
        assert!(looks_like_hallucination(
            "Subtitles by the Amara.org community"
        ));
        assert!(looks_like_hallucination("you"));
    }

    #[test]
    fn rejects_phonetic_gibberish() {
        // Mostly non-ASCII glyphs — Whisper sometimes spits these out for
        // silence/static. The 50% ASCII-friendly ratio catches them.
        assert!(looks_like_hallucination("ʕ ʔ ʔ ʔ ʔ"));
        assert!(looks_like_hallucination("。。。。。"));
    }

    #[test]
    fn allows_empty_string() {
        // Empty isn't really a "hallucination" — caller decides what to do
        // with it. We just don't claim it IS one.
        assert!(!looks_like_hallucination(""));
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        // The canned-phrase check normalises both case and trailing punct.
        assert!(looks_like_hallucination("THANK YOU"));
        assert!(looks_like_hallucination("thank you!"));
        assert!(looks_like_hallucination("Thank you,"));
    }
}
