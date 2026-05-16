//! macOS overlay: the Jarvis HUD as a non-activating `NSPanel` with two modes.
//!
//!  - **Fullscreen** — full-monitor, click-through, floats above other apps
//!    (including their fullscreen Spaces). Immersive HUD experience.
//!  - **Widget** — a 480-px square pinned to the bottom-right corner, NOT
//!    click-through, so the in-HUD toggle button (and any future controls)
//!    can be clicked.
//!
//! Mode is switched at runtime via [`apply_widget_mode`] — called from the
//! `set_widget_mode` Tauri command or the `⌃⌥J` global hotkey. The current
//! mode lives in a process-wide [`Mutex`] so the hotkey handler can toggle
//! it without going through the webview.

use std::sync::Mutex;

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelLevel, StyleMask, WebviewWindowExt};

use crate::state::LockExt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WidgetMode {
    Fullscreen,
    Widget,
}

impl WidgetMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fullscreen" => Some(Self::Fullscreen),
            "widget" => Some(Self::Widget),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Widget => "widget",
        }
    }
}

/// Process-wide current mode. Read by the global-shortcut handler and updated
/// by [`apply_widget_mode`].
static CURRENT_MODE: Mutex<WidgetMode> = Mutex::new(WidgetMode::Fullscreen);

/// Widget-mode edge length (logical pixels — scaled to physical via the
/// monitor's `scale_factor` before being handed to the windowing system).
const WIDGET_SIZE: f64 = 480.0;
/// Margin from the bottom-right corner of the screen, logical pixels.
const WIDGET_MARGIN: f64 = 24.0;

tauri_panel! {
    panel!(JarvisPanel {
        config: {
            // The HUD is informational — it must never become the key window
            // or it would steal focus from whatever the user is working in.
            can_become_key_window: false,
            is_floating_panel: true,
            // NSPanel defaults `hidesOnDeactivate = YES`, which makes the HUD
            // vanish whenever the user clicks on another app — exactly the
            // wrong behavior for an always-visible agent. Force it off.
            hides_on_deactivate: false
        }
    })
}

/// Convert the main window into a non-activating `NSPanel` and apply the
/// initial (fullscreen) mode. Safe to call once during `setup`.
pub fn install(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or("overlay: 'main' window not found")?;

    let panel = window
        .to_panel::<JarvisPanel>()
        .map_err(|e| format!("overlay: to_panel failed: {e:?}"))?;

    // Float above normal windows.
    panel.set_level(PanelLevel::Floating.value());

    // Non-activating: clicks/interaction never bring Jarvis to the foreground.
    panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());

    // Join every Space and sit alongside other apps' fullscreen windows.
    panel.set_collection_behavior(
        CollectionBehavior::new()
            .full_screen_auxiliary()
            .can_join_all_spaces()
            .into(),
    );

    // Apply the default mode (fullscreen). The panel reset above can clobber
    // any earlier frame, so we always (re)apply size + position here.
    apply_widget_mode(&window, current_mode())?;
    Ok(())
}

/// Read the live mode. Used by the global-shortcut handler to toggle.
pub fn current_mode() -> WidgetMode {
    *CURRENT_MODE.lock_recover()
}

/// Resize + reposition the panel for the requested mode and apply the matching
/// click-through setting. Persists `mode` in [`CURRENT_MODE`].
///
/// - **Fullscreen**: full monitor frame, `ignore_cursor_events(true)` so the
///   desktop and apps behind the overlay stay usable.
/// - **Widget**: 480×480 (logical) in the bottom-right corner, NOT
///   click-through, so on-screen controls inside the widget can be clicked.
pub fn apply_widget_mode(window: &WebviewWindow, mode: WidgetMode) -> Result<(), String> {
    let monitor = window
        .primary_monitor()
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| {
            window
                .available_monitors()
                .ok()
                .and_then(|all| all.into_iter().next())
        })
        .ok_or("overlay: no monitor found")?;

    let mon_pos = *monitor.position();
    let mon_size = *monitor.size();

    match mode {
        WidgetMode::Fullscreen => {
            // 70% of monitor, centered — leaves desktop visible around Jarvis
            // and keeps the custom top bar clearly readable (no menu-bar
            // overlap, no edge crowding). "Fullscreen" name is now slightly
            // misleading but we keep it to avoid changing the public enum.
            let w = (mon_size.width as f64 * 0.70) as u32;
            let h = (mon_size.height as f64 * 0.70) as u32;
            let x = mon_pos.x + ((mon_size.width as i32 - w as i32) / 2);
            let y = mon_pos.y + ((mon_size.height as i32 - h as i32) / 2);
            let _ = window.set_size(PhysicalSize::new(w, h));
            let _ = window.set_position(PhysicalPosition::new(x, y));
            // Interactive so the custom top bar receives clicks.
            window
                .set_ignore_cursor_events(false)
                .map_err(|e| format!("overlay: set_ignore_cursor_events(false) failed: {e}"))?;
            println!("[jarvis] overlay: NORMAL {w}x{h}px at ({x}, {y}) [70% of monitor]");
        }
        WidgetMode::Widget => {
            let scale = monitor.scale_factor();
            let widget = (WIDGET_SIZE * scale) as i32;
            let margin = (WIDGET_MARGIN * scale) as i32;
            let x = mon_pos.x + mon_size.width as i32 - widget - margin;
            let y = mon_pos.y + mon_size.height as i32 - widget - margin;
            let _ = window.set_size(PhysicalSize::new(widget as u32, widget as u32));
            let _ = window.set_position(PhysicalPosition::new(x, y));
            // Widget mode is interactive — the toggle button needs clicks.
            window
                .set_ignore_cursor_events(false)
                .map_err(|e| format!("overlay: set_ignore_cursor_events(false) failed: {e}"))?;
            println!("[jarvis] overlay: WIDGET {widget}x{widget}px at ({x}, {y})");
        }
    }

    *CURRENT_MODE.lock_recover() = mode;
    Ok(())
}

/// Move the panel to the next monitor in the available-monitors list,
/// preserving the current widget mode (fullscreen / widget) on the new
/// screen. Triggered from the top-bar "next screen" button.
///
/// If only one monitor is connected, this is a no-op (returns Ok).
pub fn move_to_next_screen(window: &WebviewWindow) -> Result<(), String> {
    let monitors = window
        .available_monitors()
        .map_err(|e| format!("overlay: list monitors failed: {e}"))?;
    if monitors.len() <= 1 {
        return Ok(());
    }

    let current = window
        .current_monitor()
        .map_err(|e| format!("overlay: current monitor failed: {e}"))?
        .ok_or("overlay: no current monitor")?;
    let cur_pos = current.position();

    // Pick the next monitor in the list, wrapping around.
    let mut next_idx = 0usize;
    for (i, m) in monitors.iter().enumerate() {
        if m.position() == cur_pos {
            next_idx = (i + 1) % monitors.len();
            break;
        }
    }
    let next = &monitors[next_idx];
    let mode = current_mode();

    let mon_pos = *next.position();
    let mon_size = *next.size();
    match mode {
        WidgetMode::Fullscreen => {
            let _ = window.set_size(mon_size);
            let _ = window.set_position(mon_pos);
        }
        WidgetMode::Widget => {
            let scale = next.scale_factor();
            let widget = (WIDGET_SIZE * scale) as i32;
            let margin = (WIDGET_MARGIN * scale) as i32;
            let x = mon_pos.x + mon_size.width as i32 - widget - margin;
            let y = mon_pos.y + mon_size.height as i32 - widget - margin;
            let _ = window.set_position(PhysicalPosition::new(x, y));
        }
    }
    println!(
        "[jarvis] overlay: moved to monitor {} of {}",
        next_idx + 1,
        monitors.len()
    );
    Ok(())
}

/// Minimise the panel — hides it without quitting Jarvis. Triggered from
/// the top-bar minimise button. Re-show by clicking the dock icon or via
/// the wake hotkey.
pub fn minimise(window: &WebviewWindow) -> Result<(), String> {
    window
        .hide()
        .map_err(|e| format!("overlay: hide failed: {e}"))
}
