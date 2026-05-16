// Jarvis HUD bootstrap — mounts the depth hologram + HUD chrome and wires the
// Tauri events the visualization reacts to: assistant state, audio amplitude,
// and the fullscreen ↔ widget mode toggle.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  init as initGraph,
  setState as setGraphState,
  setAudioLevel,
  destroy as destroyGraph,
} from "./hud/graph";
import {
  initPanels,
  setPanelState,
  setAmplitude,
  destroyPanels,
  appendTraceLine,
} from "./hud/panels";
import { initSysinfo, stopSysinfo } from "./hud/sysinfo";
import type { HudState, StatePayload, AmplitudePayload } from "./types";

type WidgetMode = "fullscreen" | "widget";

const hudContainer = document.getElementById("hud")!;
const stageEl = document.getElementById("stage");

// --- Avatar + HUD chrome ----------------------------------------------------
initGraph(hudContainer);
// `initPanels` gets `setAssistantState` so the on-screen state buttons (live
// in the browser dev preview) drive both the hologram and the HUD chrome.
initPanels(setAssistantState);

// Real system telemetry — polls Rust `get_system_snapshot` every 500 ms and
// renders host / model / OS / battery / WiFi / CPU / RAM / disk / net into
// the four corner panels. Replaces the demo's mock values.
initSysinfo();

// One entry point that drives both the 3D avatar and the HUD chrome.
function setAssistantState(state: HudState): void {
  setGraphState(state);
  setPanelState(state);
}

// --- Fullscreen ↔ widget mode ----------------------------------------------
// The current mode lives on `<body data-mode="...">` so the CSS can hide the
// HUD panels in widget mode (the 480px window is too small for them).
let currentMode: WidgetMode = "fullscreen";

function applyModeUI(mode: WidgetMode): void {
  currentMode = mode;
  document.body.dataset.mode = mode;
  // Repaint the toggle button label.
  const label = document.getElementById("mode-toggle-label");
  if (label) label.textContent = mode === "fullscreen" ? "MIN" : "MAX";
  const btn = document.getElementById("mode-toggle");
  if (btn) {
    btn.setAttribute(
      "aria-label",
      mode === "fullscreen" ? "Collapse to widget" : "Expand to fullscreen",
    );
  }
}

// User click on the in-HUD toggle button. In fullscreen mode the button can't
// actually receive the click (the panel is OS-level click-through) — that's
// what the `⌃⌥J` global hotkey is for. In widget mode it works normally.
async function toggleMode(): Promise<void> {
  const next: WidgetMode = currentMode === "fullscreen" ? "widget" : "fullscreen";
  try {
    await invoke("set_widget_mode", { mode: next });
    // The Rust command emits `hud://mode` on success, which updates the UI;
    // we only need to flip the label here in the standalone preview where
    // the invoke fails.
  } catch (err) {
    // In a plain browser (no Tauri) just flip the label so the layout updates.
    console.warn("[jarvis] set_widget_mode unavailable, faking mode flip:", err);
    applyModeUI(next);
  }
}

// Apply initial mode immediately so the layout is correct before any events.
applyModeUI("fullscreen");

// --- Top-bar buttons (always visible) ---------------------------------------
// Close = quit Jarvis. Minimise = hide panel (process keeps running).
// Next-screen = move panel to the next available monitor.
function wireTopBar(): void {
  const close = document.getElementById("tl-close");
  const min = document.getElementById("tl-min");
  const screen = document.getElementById("tl-screen");
  if (close) {
    close.addEventListener("click", () => {
      void invoke("close_jarvis").catch((e) =>
        console.warn("[jarvis] close failed:", e),
      );
    });
  }
  if (min) {
    min.addEventListener("click", () => {
      void invoke("minimise_jarvis").catch((e) =>
        console.warn("[jarvis] minimise failed:", e),
      );
    });
  }
  if (screen) {
    screen.addEventListener("click", () => {
      void invoke("move_to_next_screen").catch((e) =>
        console.warn("[jarvis] move screen failed:", e),
      );
    });
  }
}
wireTopBar();

// --- Manual drag for the borderless NSPanel ---------------------------------
// `data-tauri-drag-region` calls NSWindow.performWindowDragWithEvent which is
// a no-op on borderless panels. We bridge with our own move_window_by command
// driven by screenX/screenY deltas, throttled to requestAnimationFrame so the
// IPC traffic stays sane during fast drags.
function wireDrag(): void {
  const dragEl = document.querySelector<HTMLElement>(".topbar-drag");
  if (!dragEl) return;

  let dragging = false;
  let lastX = 0;
  let lastY = 0;
  let pendingDx = 0;
  let pendingDy = 0;
  let rafScheduled = false;

  const flush = (): void => {
    rafScheduled = false;
    const dx = pendingDx;
    const dy = pendingDy;
    pendingDx = 0;
    pendingDy = 0;
    if (dx !== 0 || dy !== 0) {
      void invoke("move_window_by", { dx, dy }).catch((e) =>
        console.warn("[jarvis] move_window_by failed:", e),
      );
    }
  };

  dragEl.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    dragging = true;
    lastX = e.screenX;
    lastY = e.screenY;
    e.preventDefault();
  });

  window.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    pendingDx += e.screenX - lastX;
    pendingDy += e.screenY - lastY;
    lastX = e.screenX;
    lastY = e.screenY;
    if (!rafScheduled) {
      rafScheduled = true;
      requestAnimationFrame(flush);
    }
  });

  const stop = (): void => {
    dragging = false;
  };
  window.addEventListener("mouseup", stop);
  window.addEventListener("mouseleave", stop);
}
wireDrag();

// --- Tauri event wiring -----------------------------------------------------
// `listen` reaches into Tauri internals; in a plain browser (or before the
// API is ready) that throws. Guard it so the avatar still renders standalone.
try {
  listen<StatePayload>("hud://state", (e) => setAssistantState(e.payload.state));

  // Mic / TTS amplitude → drives both the mouth lip-sync in the hologram and
  // the spectrum + signal panels. Harmless if the backend never emits it.
  listen<AmplitudePayload>("hud://amplitude", (e) => {
    const level = Math.max(e.payload.mic, e.payload.tts);
    setAudioLevel(level);
    setAmplitude(level);
  });

  // Mode changes from either the toggle button (round-trip) or the global
  // hotkey (push from Rust). Updates the body attribute so CSS can react.
  listen<{ mode: WidgetMode }>("hud://mode", (e) => applyModeUI(e.payload.mode));

  // Live trace of what the brain is doing — tool calls and their results
  // emitted by the streaming agent path. Each event appears as a fading line
  // in the top-center trace stack so onlookers see Jarvis actually working.
  listen<{ action: string; kind: "use" | "ok" | "error" }>("hud://trace", (e) =>
    appendTraceLine(e.payload.action, e.payload.kind),
  );
} catch (err) {
  console.warn(
    "[jarvis] Tauri event API unavailable — avatar runs standalone:",
    err,
  );
}

// Wire the toggle button (works in widget mode and in the browser preview).
const toggleBtn = document.getElementById("mode-toggle");
if (toggleBtn) toggleBtn.addEventListener("click", toggleMode);

// Suppress an "unused" warning if no one references it; keeps `stageEl` here
// for any future stage-level work without forcing a re-import.
void stageEl;

// --- Vite HMR cleanup -------------------------------------------------------
// Without this, each hot-reload re-runs this module and stacks another
// Three.js canvas + render loop on top of the old one.
if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    destroyGraph();
    destroyPanels();
    stopSysinfo();
  });
}

// --- Manual hooks ----------------------------------------------------------
// Exposed for quick testing from the devtools console:
//   setAssistantState("idle" | "listening" | "thinking" | "speaking")
//   setAudioLevel(0..1)
//   setWidgetMode("fullscreen" | "widget")
const api = window as unknown as {
  setAssistantState: (s: HudState) => void;
  setAudioLevel: (v: number) => void;
  setWidgetMode: (m: WidgetMode) => void;
};
api.setAssistantState = setAssistantState;
api.setAudioLevel = setAudioLevel;
api.setWidgetMode = (m) => {
  invoke("set_widget_mode", { mode: m }).catch(() => applyModeUI(m));
};
