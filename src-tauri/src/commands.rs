//! The Rust command surface exposed to the webview.
//!
//! The manual "TALK TO JARVIS" button records with [`Recorder`](crate::audio::Recorder)
//! and hands the result to the [`Engine`] as a direct command (no wake word
//! required). Hands-free capture runs independently inside the engine.

use std::sync::Arc;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::pipeline::{Engine, Trigger};
use crate::state::AppState;

/// Begin capturing microphone audio for a manual (button-triggered) command.
#[tauri::command]
pub fn start_recording(app: AppHandle, state: State<Arc<AppState>>) -> Result<(), String> {
    state.recorder.start()?;
    let _ = app.emit("hud://state", json!({ "state": "listening" }));
    Ok(())
}

/// Stop the manual recording and submit it to the engine as a direct command.
#[tauri::command]
pub fn stop_and_process(
    state: State<Arc<AppState>>,
    engine: State<Arc<Engine>>,
) -> Result<(), String> {
    let samples = state.recorder.stop()?;
    engine.submit(samples, Trigger::DirectCommand);
    Ok(())
}

/// The TTS voice currently in use — surfaced in the HUD readout.
#[tauri::command]
pub fn tts_voice(_state: State<Arc<AppState>>) -> &'static str {
    crate::tts::voice_label()
}

/// Switch the HUD between fullscreen overlay and bottom-right widget mode.
/// Called from the in-HUD toggle button; the global `⌃⌥J` shortcut takes the
/// same path through `overlay::apply_widget_mode`.
#[tauri::command]
pub fn set_widget_mode(app: AppHandle, mode: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let parsed = crate::overlay::WidgetMode::parse(&mode)
            .ok_or_else(|| format!("unknown mode: {mode}"))?;
        let window = app
            .get_webview_window("main")
            .ok_or("main window not found")?;
        crate::overlay::apply_widget_mode(&window, parsed)?;
        let _ = app.emit("hud://mode", json!({ "mode": parsed.as_str() }));
    }
    // On non-macOS the overlay module isn't compiled in — there's no panel
    // to resize, so this is a no-op.
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, mode);
    }
    Ok(())
}
