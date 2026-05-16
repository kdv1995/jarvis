// Jarvis HUD panels — the chrome around the depth hologram.
//
// Ported from the inline <script> of the standalone `hologram-depth.html`
// demo: a live clock (top-right), a synthetic audio spectrum (bottom-right),
// a signal readout (bottom-left), the top-center state badge, and the
// bottom-center state switcher.
//
// The spectrum + signal panels are driven by the hologram's live mouth/breath
// levels, read straight from `graph.ts` via `getSignals()` — so when Jarvis is
// "speaking" the bars dance with the synthetic visemes.

import { getSignals } from "./graph";
import type { HudState } from "../types";

const BAR_COUNT = 28;

// Per-state channel hint shown in the spectrum panel's sub-line.
const CHANNEL: Record<HudState, string> = {
  idle: "CH-01 · STANDBY",
  listening: "CH-01 · MIC OPEN",
  speaking: "CH-02 · TTS OUT",
  thinking: "CH-00 · BUFFER",
};

interface Bar {
  el: HTMLDivElement;
  h: number;
  target: number;
}

// --- Module state ----------------------------------------------------------
let bars: Bar[] = [];
let raf = 0;
let stageEl: HTMLElement | null = null;
let stateNameEl: HTMLElement | null = null;
let buttons: HTMLButtonElement[] = [];

let startedAt = 0;
let lastSec = -1;
let phonemes = 0;
let lastMouthCrossed = false;
let mouthVisual = 0;

// Latest pipeline audio amplitude (0..1) — folded into the spectrum so real
// TTS energy nudges the bars on top of the synthetic viseme activity.
let amplitude = 0;

// Callback into the app when a state button is clicked (browser preview only).
let onStateRequest: ((s: HudState) => void) | null = null;

// --- Helpers ---------------------------------------------------------------
const fmt2 = (n: number) => n.toString().padStart(2, "0");
const fmt3 = (n: number) => n.toString().padStart(3, "0");

// Per-bar baseline weights so the spectrum doesn't look uniform — two humps
// for the voice fundamental (~150–500 Hz) and the formant band (~1–3 kHz).
function bandWeight(i: number): number {
  const x = i / (BAR_COUNT - 1);
  const a = Math.exp(-Math.pow((x - 0.18) / 0.16, 2));
  const b = Math.exp(-Math.pow((x - 0.55) / 0.2, 2)) * 0.85;
  const c = Math.exp(-Math.pow((x - 0.85) / 0.22, 2)) * 0.35;
  return Math.max(0.08, a + b + c);
}

function $(id: string): HTMLElement | null {
  return document.getElementById(id);
}

// --- Per-frame updates -----------------------------------------------------
function tickClock(now: number): void {
  const d = new Date(now);
  const time = $("clock-time");
  const ms = $("clock-ms");
  if (time) {
    time.textContent =
      fmt2(d.getHours()) + ":" + fmt2(d.getMinutes()) + ":" + fmt2(d.getSeconds());
  }
  if (ms) ms.textContent = "." + fmt3(d.getMilliseconds());

  // Slower fields refresh ~1 Hz.
  const sec = (now / 1000) | 0;
  if (sec !== lastSec) {
    lastSec = sec;
    const months = [
      "JAN", "FEB", "MAR", "APR", "MAY", "JUN",
      "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    const date = $("clock-date");
    if (date) {
      date.textContent =
        fmt2(d.getDate()) + " " + months[d.getMonth()] + " " + d.getFullYear();
    }
    const tz = $("clock-tz");
    if (tz) {
      tz.textContent =
        "UTC" +
        (d.getTimezoneOffset() <= 0 ? "+" : "−") +
        fmt2(Math.abs((d.getTimezoneOffset() / 60) | 0));
    }
    const utc = $("clock-utc");
    if (utc) {
      utc.textContent =
        fmt2(d.getUTCHours()) + ":" + fmt2(d.getUTCMinutes()) + ":" + fmt2(d.getUTCSeconds());
    }
    const uptime = $("clock-uptime");
    if (uptime) {
      const upS = ((now - startedAt) / 1000) | 0;
      uptime.textContent =
        fmt2((upS / 3600) | 0) + ":" + fmt2(((upS / 60) | 0) % 60) + ":" + fmt2(upS % 60);
    }
  }
}

function tickSignal(now: number): void {
  const { state, mouth, breath } = getSignals();
  mouthVisual += (mouth - mouthVisual) * 0.4;

  // Count phoneme "events" — rising edges on the mouth level.
  if (mouth > 0.25 && !lastMouthCrossed) {
    phonemes++;
    lastMouthCrossed = true;
  }
  if (mouth < 0.1) lastMouthCrossed = false;

  // Spectrum activity target by state, with the real audio amplitude blended
  // in while speaking / listening.
  let activity = 0;
  if (state === "speaking") activity = 0.55 + mouth * 0.6 + amplitude * 0.3;
  else if (state === "listening") activity = 0.1 + breath * 0.1 + amplitude * 0.2;
  else if (state === "thinking") activity = 0.18 + breath * 0.18;
  else activity = 0.04;

  for (let i = 0; i < BAR_COUNT; i++) {
    const w = bandWeight(i);
    const jitter = Math.random() * 0.6 + 0.4;
    const slow = Math.sin((now / 1000) * (0.6 + i * 0.07) + i) * 0.15 + 0.85;
    bars[i].target = Math.min(1, activity * w * jitter * slow);
    bars[i].h += (bars[i].target - bars[i].h) * 0.35;
    bars[i].el.style.height = (4 + bars[i].h * 52).toFixed(0) + "px";
    bars[i].el.style.opacity = (0.55 + bars[i].h * 0.45).toFixed(2);
  }

  // Peak frequency — map the loudest bar's index onto a log scale 80 Hz..16 kHz.
  let peakIdx = 0;
  let peakVal = 0;
  for (let i = 0; i < BAR_COUNT; i++) {
    if (bars[i].h > peakVal) {
      peakVal = bars[i].h;
      peakIdx = i;
    }
  }
  const f = 80 * Math.pow(16000 / 80, peakIdx / (BAR_COUNT - 1));
  const hzPeak = $("hz-peak");
  if (hzPeak) {
    hzPeak.textContent = f >= 1000 ? (f / 1000).toFixed(1) + "k" : f.toFixed(0);
  }
  const hzDb = $("hz-db");
  if (hzDb) hzDb.textContent = (-60 + peakVal * 56).toFixed(0);

  // Signal panel.
  const sigMouth = $("sig-mouth");
  if (sigMouth) sigMouth.textContent = mouth.toFixed(2);
  const sigPh = $("sig-ph");
  if (sigPh) sigPh.textContent = String(phonemes);
  const sigLat = $("sig-lat");
  if (sigLat) {
    const latBase = state === "thinking" ? 38 : 14;
    sigLat.textContent =
      (latBase + Math.sin(now / 700) * 3 + Math.random() * 1.5).toFixed(0) + " ms";
  }
  const sigJit = $("sig-jit");
  if (sigJit) sigJit.textContent = (1.4 + Math.random() * 1.2).toFixed(1) + " ms";
  const sigBr = $("sig-br");
  if (sigBr) {
    const br =
      state === "speaking" ? 768 : state === "listening" ? 256 : state === "thinking" ? 384 : 128;
    sigBr.textContent = br + ((Math.random() * 16) | 0) + " kbps";
  }
}

function loop(): void {
  const now = Date.now();
  tickClock(now);
  tickSignal(now);
  raf = requestAnimationFrame(loop);
}

// Repaint the state-dependent chrome: badge text, button highlight, channel
// hint, and the stage's `data-state` attribute.
function paint(name: HudState): void {
  if (stateNameEl) stateNameEl.textContent = name.toUpperCase();
  for (const b of buttons) {
    b.setAttribute("aria-current", b.dataset.state === name ? "true" : "false");
  }
  const chan = $("hz-channel");
  if (chan) chan.textContent = CHANNEL[name] ?? "CH-01";
  if (stageEl) stageEl.dataset.state = name;
}

// --- Public API ------------------------------------------------------------
export function initPanels(onState: (s: HudState) => void): void {
  onStateRequest = onState;
  stageEl = document.getElementById("stage");
  stateNameEl = $("state-name");
  startedAt = Date.now();

  // Build the spectrum bars.
  const host = $("spectrum-bars");
  bars = [];
  if (host) {
    for (let i = 0; i < BAR_COUNT; i++) {
      const el = document.createElement("div");
      el.className = "bar";
      el.style.height = "2px";
      host.appendChild(el);
      bars.push({ el, h: 0, target: 0 });
    }
  }

  // Wire the state-switcher buttons (clickable in the browser dev preview).
  buttons = Array.from(
    document.querySelectorAll<HTMLButtonElement>(".states button"),
  );
  for (const b of buttons) {
    b.addEventListener("click", () => {
      const s = b.dataset.state as HudState | undefined;
      if (s) onStateRequest?.(s);
    });
  }

  paint("idle");
  raf = requestAnimationFrame(loop);
}

export function setPanelState(s: HudState): void {
  paint(s);
}

export function setAmplitude(level: number): void {
  amplitude = Math.max(0, Math.min(1, level));
}

/**
 * Append a single line to the HUD execution-trace stack. Each line fades in,
 * lingers ~3s, then fades out. Max 5 lines visible at once — older ones get
 * shifted out as new ones arrive. Driven by `hud://trace` events emitted by
 * the Rust pipeline's streaming agent path (one per Claude tool call / result).
 */
export function appendTraceLine(action: string, kind: "use" | "ok" | "error"): void {
  const host = document.getElementById("trace");
  if (!host) return;

  const line = document.createElement("div");
  line.className = `trace-line trace-${kind}`;
  line.textContent = action;
  host.appendChild(line);

  // Trim the stack so it never exceeds 5 lines — drops the oldest first.
  while (host.children.length > 5) {
    host.firstElementChild?.remove();
  }

  // Trigger the fade-in on the next frame so the transition runs.
  requestAnimationFrame(() => line.classList.add("visible"));

  // Auto-fade out after 3 seconds, then remove from the DOM 400ms later
  // (matches the CSS transition duration so we don't get a stale ghost).
  window.setTimeout(() => {
    line.classList.remove("visible");
    window.setTimeout(() => line.remove(), 400);
  }, 3000);
}

export function destroyPanels(): void {
  cancelAnimationFrame(raf);
  raf = 0;
  for (const b of bars) b.el.remove();
  bars = [];
  buttons = [];
  stageEl = null;
  stateNameEl = null;
  onStateRequest = null;
}
