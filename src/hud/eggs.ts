// Easter-egg visual effects. Triggered when the Rust pipeline emits the
// `hud://egg` event after matching a hidden voice command (see
// pipeline.rs::try_easter_egg).
//
// We deliberately keep these effects pure-CSS overlays + minimal DOM mutations
// instead of going into the Three.js shader pipeline — easter eggs are rare
// signature moments, not core hot path. Adding shader uniforms for one-off
// triggers would bloat the per-frame cost.

import { listen } from "@tauri-apps/api/event";

type EggPayload = { effect: string };

/** Mount the egg listener. Idempotent — second call is a no-op. */
let mounted = false;
export function initEggs(): void {
  if (mounted) return;
  mounted = true;
  void listen<EggPayload>("hud://egg", (e) => fire(e.payload.effect));
}

function fire(effect: string): void {
  switch (effect) {
    case "alive":
      runAliveBeat();
      break;
    case "hologram":
      runHologramOrbit();
      break;
    case "reactor":
      runReactorGlow();
      break;
    case "compliment":
      runComplimentBrighten();
      break;
    default:
      console.warn("[eggs] unknown effect:", effect);
  }
}

// ── "Are you alive?" — dim everything, single red pulsing dot center-screen ──
function runAliveBeat(): void {
  const stage = document.getElementById("stage");
  if (!stage) return;
  stage.classList.add("egg-alive");
  const dot = document.createElement("div");
  dot.className = "egg-alive-dot";
  document.body.appendChild(dot);
  // The CSS animation runs ~3s; clean up after.
  setTimeout(() => {
    stage.classList.remove("egg-alive");
    dot.remove();
  }, 3200);
}

// ── "Give me a hologram" — three translucent portrait copies orbit ──
function runHologramOrbit(): void {
  const stage = document.getElementById("stage");
  if (!stage) return;
  stage.classList.add("egg-hologram");
  setTimeout(() => stage.classList.remove("egg-hologram"), 4200);
}

// ── "Reactor status" — red-hot core glow for 2s, then back ──
function runReactorGlow(): void {
  const stage = document.getElementById("stage");
  if (!stage) return;
  stage.classList.add("egg-reactor");
  setTimeout(() => stage.classList.remove("egg-reactor"), 2200);
}

// ── Compliment received — brighten the whole stage by 20% for 700ms ──
function runComplimentBrighten(): void {
  const stage = document.getElementById("stage");
  if (!stage) return;
  stage.classList.add("egg-compliment");
  setTimeout(() => stage.classList.remove("egg-compliment"), 700);
}
