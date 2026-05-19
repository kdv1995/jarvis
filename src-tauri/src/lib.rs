//! Jarvis — voice-driven Iron Man HUD that orchestrates Claude Code.
//!
//! Voice loop: capture mic audio → local Whisper / ElevenLabs STT →
//! `claude` CLI → ElevenLabs/Kokoro TTS. The HUD is a fullscreen,
//! click-through NSPanel overlay.

mod agent;
mod audio;
mod commands;
mod config;
mod pipeline;
mod state;
mod skills;
mod stt;
mod sysinfo;
mod tts;
mod vad;

#[cfg(target_os = "macos")]
mod overlay;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

use state::AppState;

/// Load environment variables from a `.env` file.
///
/// A bundled `.app` launched from Finder runs with its CWD set to `/`, so the
/// plain CWD-relative `dotenvy::dotenv()` finds nothing. We also check
/// `~/.jarvis/.env` — the stable per-user location the bundled app reads.
/// `dotenvy` never overrides an already-set variable, so the CWD file (loaded
/// first) still wins for a developer running `cargo run` / `tauri dev`.
fn load_env() {
    // 1. CWD and ancestor dirs — works for `cargo run` / `tauri dev`.
    if let Ok(path) = dotenvy::dotenv() {
        println!("[jarvis] env: loaded {}", path.display());
    }
    // 2. ~/.jarvis/.env — the location the bundled app reads.
    if let Ok(home) = std::env::var("HOME") {
        let path = PathBuf::from(home).join(".jarvis/.env");
        if dotenvy::from_path(&path).is_ok() {
            println!("[jarvis] env: loaded {}", path.display());
        }
    }
}

// NOTE: the historical `wake_launch_marker` path (a background clap daemon
// dropped a temp file before launching Jarvis, and we greeted the user on
// startup) was removed when wake-triggers were restricted to the voice
// wake-word. Keep the marker filename reserved if we ever resurrect it.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Load `.env` before reading any config (CWD for dev, ~/.jarvis for the
    // bundled app).
    load_env();

    let elevenlabs = config::ElevenLabsConfig::from_env();
    match elevenlabs {
        Some(_) => println!(
            "[jarvis] STT: ElevenLabs fallback available — TTS: {}",
            tts::voice_label()
        ),
        None => println!(
            "[jarvis] STT: local Whisper only — TTS: {}",
            tts::voice_label()
        ),
    }

    let app_state = Arc::new(AppState::new(elevenlabs));

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build());

    // Global hotkeys (⌃⌥J etc.) are intentionally NOT registered. Wake/open
    // is restricted to the voice wake-word so casual keyboard activity can't
    // surprise-summon Jarvis. The tray menu still exposes "Toggle Fullscreen
    // / Widget" via direct click.

    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());

    let engine_state = app_state.clone();

    builder
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::start_recording,
            commands::stop_and_process,
            commands::tts_voice,
            commands::set_widget_mode,
            commands::get_system_snapshot,
            commands::move_to_next_screen,
            commands::minimise_jarvis,
            commands::close_jarvis,
            commands::move_window_by
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            {
                if let Err(e) = overlay::install(app.handle()) {
                    eprintln!("[jarvis] overlay setup failed: {e}");
                }
                // Global toggle hotkey is intentionally not registered; see
                // the comment on the builder above.
            }

            // Menu-bar tray icon — the idiomatic "close button" for an
            // always-running overlay app. Click the J in your menu bar to
            // see a menu with "Quit Jarvis", toggle widget/fullscreen, etc.
            // The HUD itself stays click-through-transparent, so the tray
            // is the discoverable way to exit without `pkill`.
            let quit_item = MenuItem::with_id(app, "quit", "Quit Jarvis", true, Some("Cmd+Q"))?;
            let toggle_item = MenuItem::with_id(
                app,
                "toggle_mode",
                "Toggle Fullscreen / Widget",
                true,
                None::<&str>,
            )?;
            let sep = tauri::menu::PredefinedMenuItem::separator(app)?;
            let about_item = MenuItem::with_id(
                app,
                "about",
                "Jarvis — voice assistant",
                false, // disabled, just a label
                None::<&str>,
            )?;
            let menu = Menu::with_items(app, &[&about_item, &sep, &toggle_item, &sep, &quit_item])?;

            let _tray = TrayIconBuilder::with_id("jarvis-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .icon_as_template(true) // monochrome menu-bar style
                .tooltip("Jarvis")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => {
                        println!("[jarvis] tray → quit requested");
                        app.exit(0);
                    }
                    "toggle_mode" => {
                        #[cfg(target_os = "macos")]
                        {
                            let next = match overlay::current_mode() {
                                overlay::WidgetMode::Fullscreen => overlay::WidgetMode::Widget,
                                overlay::WidgetMode::Widget => overlay::WidgetMode::Fullscreen,
                            };
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = overlay::apply_widget_mode(&w, next);
                                let _ = app.emit("hud://mode", json!({ "mode": next.as_str() }));
                            }
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            // Start the orchestrator + hands-free listener.
            let engine = pipeline::start(app.handle().clone(), engine_state);
            app.manage(engine);

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Jarvis")
        .run(|_app_handle, _event| {
            // No `RunEvent::Reopen` handler: a dock-icon click no longer
            // un-hides Jarvis. Wake/open is restricted to the voice
            // wake-word; minimising via the in-HUD button is intended to
            // be undone by speaking, not by clicking the dock.
        });
}
