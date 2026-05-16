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
mod stt;
mod tts;
mod vad;

#[cfg(target_os = "macos")]
mod overlay;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

use state::AppState;

#[cfg(target_os = "macos")]
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

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

fn wake_launch_marker_path() -> PathBuf {
    std::env::temp_dir().join("jarvis-wake-launch")
}

fn consume_wake_launch_marker() -> bool {
    let path = wake_launch_marker_path();
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|age| age < Duration::from_secs(30))
        .unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    fresh
}

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

    // `⌃⌥J` toggles the HUD between fullscreen and the bottom-right widget.
    // The plugin's handler fires on key press from anywhere in the OS — works
    // regardless of the panel's click-through state, which is the whole point
    // (a click-through fullscreen overlay can never receive a real button click).
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_handler(|app, _shortcut, event| {
                if event.state() != ShortcutState::Pressed {
                    return;
                }
                let next = match overlay::current_mode() {
                    overlay::WidgetMode::Fullscreen => overlay::WidgetMode::Widget,
                    overlay::WidgetMode::Widget => overlay::WidgetMode::Fullscreen,
                };
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = overlay::apply_widget_mode(&window, next) {
                        eprintln!("[jarvis] hotkey: apply mode failed: {e}");
                        return;
                    }
                    let _ = app.emit("hud://mode", json!({ "mode": next.as_str() }));
                }
            })
            .build(),
    );

    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());

    let engine_state = app_state.clone();

    builder
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::start_recording,
            commands::stop_and_process,
            commands::tts_voice,
            commands::set_widget_mode
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            {
                if let Err(e) = overlay::install(app.handle()) {
                    eprintln!("[jarvis] overlay setup failed: {e}");
                }

                // Register the global toggle hotkey now that the app is up.
                let toggle = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyJ);
                if let Err(e) = app.global_shortcut().register(toggle) {
                    eprintln!("[jarvis] global hotkey register failed: {e}");
                } else {
                    println!("[jarvis] hotkey ⌃⌥J → toggle fullscreen ↔ widget");
                }
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
                Some("Ctrl+Alt+J"),
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
            let greet_from_clap_launch = consume_wake_launch_marker();
            app.manage(engine.clone());

            if greet_from_clap_launch {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(700));
                    engine.greet_launch_wake();
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Jarvis");
}
