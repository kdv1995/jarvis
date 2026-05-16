// Jarvis HUD avatar — depth-map holographic portrait.
//
// A Three.js port of the embeddable `hologram.js` component: a displaced,
// glowing wireframe of the user's portrait, driven by a real depth map
// (white = closer, black = further) rather than guessed from photo brightness.
//
// The original component was a CDN-loading vanilla IIFE exposing a global
// `Hologram`. Here it's a TS module bound to the bundled `three` package,
// keeping the same public API the rest of the app already calls —
// init / setState / setAudioLevel / destroy — plus `getSignals()` so the HUD
// panels can read the live mouth/breath levels.
//
// State (idle / listening / speaking / thinking) is driven by the Rust voice
// pipeline via `setState`. The "speaking" state animates the mouth from a
// viseme dictionary (coarticulated lip + jaw deformation), no audio analysis
// required.

import * as THREE from "three";
import type { HudState } from "../types";

// --- Configuration ---------------------------------------------------------
const CFG = {
  src: "/portrait.png", // colour portrait (served from public/)
  depthSrc: "/lkg_depth.jpg", // depth map of that portrait (white = closer)
  // Cool silver-blue palette — Tony Stark holographic blueprint feel.
  color: "#c4d4e8",
  secondary: "#3a4754",
  ghost: "#a8b8c8",
  ghostStrength: 0.35,
  displace: 0.7,
  gridDensity: 80,
  lineWidth: 1.6,
  photoMix: 0.28,
  scan: 0.6,
};

// --- GLSL — vertex shader (shared by portrait + ghost) ---------------------
const VERT = /* glsl */ `
  uniform sampler2D uMap;
  uniform sampler2D uDepth;
  uniform float uHasDepth;
  uniform float uDisplace;
  uniform float uTime;
  uniform float uMaterialize;
  uniform float uMouth;
  uniform float uMouthWide;
  uniform float uJaw;
  uniform float uBreath;
  varying vec2 vUv;
  varying vec3 vNormal;
  varying float vEdge;
  varying float vMouth;
  float lum(vec3 c){ return dot(c, vec3(0.299, 0.587, 0.114)); }
  // Sample a depth value at uv: from real depth map if provided, else photo brightness.
  float depthAt(vec2 uv){
    if (uHasDepth > 0.5) {
      return texture2D(uDepth, uv).r;
    }
    return lum(texture2D(uMap, uv).rgb);
  }
  void main() {
    vUv = uv;
    float D = depthAt(uv);
    vec2 c = uv - 0.5;
    float radial = 1.0 - smoothstep(0.18, 0.55, length(c));
    // With a real depth map we don't need a face-bias; use depth directly,
    // centered around 0 so foreground pushes out and background recedes.
    float depthBias = (uHasDepth > 0.5) ? (D - 0.5) * 2.0 : (D - 0.35);

    // ===== Anatomical lip + jaw deformation =====
    // Mouth pivot in UV space. Plane UV: (0,0) bottom-left, (1,1) top-right.
    vec2 mouthPivot = vec2(0.5, 0.46);
    vec2 lipDelta = uv - mouthPivot;
    // Horizontal influence mask: only affect things within actual mouth width.
    // Real lips span ~14-18% of face width — keep this tight.
    float lipHalfWidth = 0.055 + uMouthWide * 0.022;
    float xMask = exp(-pow(lipDelta.x / lipHalfWidth, 2.0));
    // Vertical falloff for the lip band itself (narrow).
    float lipBand = exp(-pow(lipDelta.y / 0.045, 2.0));
    // Upper lip vs. lower lip
    float upperSide = step(0.0, lipDelta.y);  // 1 for upper, 0 for lower
    float lowerSide = 1.0 - upperSide;

    // Asymmetric opening: lower lip drops ~3x further than upper lip lifts (real anatomy).
    float upperLift = upperSide * 0.014 * uMouth * lipBand * xMask;
    float lowerDrop = lowerSide * 0.042 * uMouth * lipBand * xMask;
    float yShift = upperLift - lowerDrop;

    // Horizontal stretch: corners pull outward when wide is high (smile / 'ee'),
    // pucker inward when wide is low (round / 'oo'). Masked to stay near the
    // lips so the cheeks don't smear sideways with the mouth.
    float cornerPull = lipDelta.x * (uMouthWide - 0.5) * 0.05 * lipBand * xMask;

    // Jaw drop: a wider, softer mask covering chin region (below mouth) follows uJaw.
    float jawY = clamp((mouthPivot.y - uv.y) / 0.16, 0.0, 1.0); // 0 at mouth, 1 at full jaw
    float jawX = exp(-pow(lipDelta.x / 0.13, 2.0));
    float jawMask = jawY * (1.0 - jawY * 0.4) * jawX; // peaks just below mouth, fades to chin
    float jawShift = -uJaw * 0.028 * jawMask;

    // Used by fragment for subtle lip-edge brightening
    vMouth = (lipBand * xMask) * uMouth + jawMask * uJaw * 0.4;

    // Compose deformation
    float wipe = smoothstep(1.0 - uMaterialize - 0.05, 1.0 - uMaterialize + 0.05, uv.y);
    float dz = depthBias * uDisplace * (uHasDepth > 0.5 ? 0.9 : (0.4 + radial * 1.4)) * (1.0 + uBreath * 0.15);
    dz *= wipe;
    vEdge = wipe * (1.0 - wipe) * 4.0;
    vec3 deformed = vec3(
      position.x + cornerPull,
      position.y + (yShift + jawShift),
      position.z + dz
    );

    // Normal from neighboring depth samples
    vec2 eps = vec2(1.0 / 512.0, 0.0);
    float D1 = depthAt(uv + eps.xy);
    float D2 = depthAt(uv + eps.yx);
    vec3 nx = vec3(eps.x * 2.0, 0.0, (D1 - D) * uDisplace);
    vec3 ny = vec3(0.0, eps.x * 2.0, (D2 - D) * uDisplace);
    vNormal = normalize(cross(nx, ny));
    gl_Position = projectionMatrix * modelViewMatrix * vec4(deformed, 1.0);
  }
`;

// --- GLSL — fragment shader ------------------------------------------------
const FRAG = /* glsl */ `
  precision highp float;
  uniform sampler2D uMap;
  uniform vec3 uColor;
  uniform vec3 uColor2;
  uniform float uTime;
  uniform float uGridDensity;
  uniform float uLineWidth;
  uniform float uPhotoMix;
  uniform float uScan;
  uniform float uOpacity;
  uniform float uMaterialize;
  uniform float uReveal;
  uniform float uBreath;
  uniform float uGlow;
  varying vec2 vUv;
  varying vec3 vNormal;
  varying float vEdge;
  varying float vMouth;
  void main(){
    float wipe = smoothstep(1.0 - uMaterialize - 0.05, 1.0 - uMaterialize + 0.05, vUv.y);
    vec2 uv = vUv;
    vec4 photo = texture2D(uMap, uv);

    vec2 g = uv * uGridDensity;
    vec2 dg = fwidth(g);
    vec2 gridDist = abs(fract(g - 0.5) - 0.5) / max(dg, vec2(0.0001));
    float gridLine = 1.0 - smoothstep(0.0, uLineWidth, min(gridDist.x, gridDist.y));

    vec2 g2 = uv * uGridDensity * 4.0;
    vec2 dg2 = fwidth(g2);
    vec2 gd2 = abs(fract(g2 - 0.5) - 0.5) / max(dg2, vec2(0.0001));
    float microGrid = (1.0 - smoothstep(0.0, 1.0, min(gd2.x, gd2.y))) * 0.18;

    float fres = pow(1.0 - max(0.0, dot(normalize(vNormal), vec3(0.0, 0.0, 1.0))), 1.6);
    float scanRepeat = sin(vUv.y * 220.0) * 0.5 + 0.5;

    vec3 lineCol = mix(uColor2, uColor, clamp(gridLine + fres, 0.0, 1.0));
    float lines = gridLine * 0.95 + fres * 0.55 * uGlow + microGrid;
    vec3 col = lineCol * lines * 1.35;
    col += uColor * scanRepeat * 0.08 * uScan;
    col += vec3(1.0) * gridLine * fres * 0.25;

    float photoL = dot(photo.rgb, vec3(0.299, 0.587, 0.114));
    vec2 cc = vUv - 0.5;
    float inner = 1.0 - smoothstep(0.15, 0.42, length(cc));
    col += uColor * photoL * uPhotoMix * inner;
    col += uColor * vEdge * 1.8 * smoothstep(0.0, 0.1, vEdge) * (1.0 - smoothstep(0.1, 0.25, vEdge));

    // Breathing rim boost (listening/thinking)
    col += uColor * fres * uBreath * 0.4;

    // Mask lines to photo content so grid doesn't fill empty background plane
    float photoMask = smoothstep(0.06, 0.30, photoL);
    float a = clamp(lines * 1.3 + photoL * uPhotoMix * 2.0 * inner + fres * 0.55, 0.0, 1.0);
    a *= photoMask;
    a *= uOpacity * wipe * uReveal;
    float radial = 1.0 - smoothstep(0.36, 0.5, length(cc));
    a *= radial;
    gl_FragColor = vec4(col, a);
  }
`;

// --- Viseme dictionary -----------------------------------------------------
// Each phoneme class has an open/wide target and a hold duration. The
// "speaking" state walks this list with coarticulated (cosine-eased)
// transitions so the mouth never snaps between shapes.
interface Viseme {
  name: string;
  open: number; // vertical jaw/mouth opening 0..1
  wide: number; // horizontal spread: high = smile/'ee', low = round/'oo'
  dur: number; // seconds held before the next shape
  weight: number; // relative frequency in random speech
}
const VISEMES: Viseme[] = [
  { name: "ah", open: 0.95, wide: 0.45, dur: 0.13, weight: 8 },
  { name: "eh", open: 0.55, wide: 0.72, dur: 0.1, weight: 7 },
  { name: "ee", open: 0.28, wide: 0.96, dur: 0.1, weight: 7 },
  { name: "ih", open: 0.38, wide: 0.62, dur: 0.08, weight: 6 },
  { name: "oh", open: 0.65, wide: 0.18, dur: 0.13, weight: 6 },
  { name: "oo", open: 0.32, wide: 0.02, dur: 0.13, weight: 5 },
  { name: "uh", open: 0.48, wide: 0.5, dur: 0.08, weight: 5 },
  { name: "m/b/p", open: 0.0, wide: 0.5, dur: 0.07, weight: 6 },
  { name: "f/v", open: 0.14, wide: 0.68, dur: 0.07, weight: 3 },
  { name: "th", open: 0.2, wide: 0.58, dur: 0.07, weight: 2 },
  { name: "s/sh", open: 0.22, wide: 0.78, dur: 0.07, weight: 4 },
  { name: "t/d/n", open: 0.18, wide: 0.55, dur: 0.06, weight: 5 },
  { name: "k/g", open: 0.3, wide: 0.45, dur: 0.06, weight: 3 },
  { name: "_pause", open: 0.0, wide: 0.5, dur: 0.18, weight: 4 },
];
const VISEME_TOTAL_WEIGHT = VISEMES.reduce((a, v) => a + v.weight, 0);

function pickViseme(prev: Viseme | null): Viseme {
  // Bias away from repeating the same shape, for better variety.
  let r = Math.random() * VISEME_TOTAL_WEIGHT;
  for (const v of VISEMES) {
    r -= v.weight;
    if (r <= 0) {
      if (v === prev && Math.random() < 0.5) return pickViseme(prev);
      return v;
    }
  }
  return VISEMES[0];
}

// --- Module state ----------------------------------------------------------
let canvas: HTMLCanvasElement | null = null;
let renderer: THREE.WebGLRenderer | null = null;
let scene: THREE.Scene | null = null;
let camera: THREE.PerspectiveCamera | null = null;
let portraitGroup: THREE.Group | null = null;
let clock: THREE.Clock | null = null;
let raf = 0;

let portraitGeo: THREE.PlaneGeometry | null = null;
let portraitMat: THREE.ShaderMaterial | null = null;
let ghostMat: THREE.ShaderMaterial | null = null;
let portraitTex: THREE.Texture | null = null;
let depthTex: THREE.Texture | null = null;

// Shared shader uniforms (the ghost copy reuses these by reference, overriding
// only colour/opacity, so both meshes animate in lockstep).
let uniforms: Record<string, { value: unknown }> = {};

// Head orientation — gentle idle drift, lerp-smoothed.
let yaw = 0;
let pitch = 0;
let targetYaw = 0;
let targetPitch = 0;

// State machine.
let state: HudState = "idle";
let mouthOpen = 0;
let mouthOpenTarget = 0;
let mouthWide = 0.5;
let mouthWideTarget = 0.5;
let jaw = 0; // inertial follower of mouthOpen
let currentViseme: Viseme | null = null;
let nextVisemeAt = 0;
let visemeStartAt = 0;
let prevOpen = 0;
let prevWide = 0.5;
let breath = 0;
let breathTarget = 0;
let glow = 1.0;
let glowTarget = 1.0;

// Live TTS amplitude from the pipeline (0..1). During "speaking" this drives
// the mouth's openness directly — real lip-sync. `audioTarget` is the latest
// value pushed in; `audioLevel` is lerp-smoothed toward it each frame so the
// 20 Hz amplitude stream renders as smooth 60 Hz mouth motion.
let audioLevel = 0;
let audioTarget = 0;

// --- Animation -------------------------------------------------------------
function updateStateMotion(t: number): void {
  // Smooth the 20 Hz TTS amplitude stream into a 60 Hz follower — keeps the
  // mouth motion fluid even when the backend emits levels at a slower rate.
  audioLevel += (audioTarget - audioLevel) * 0.4;

  if (state === "idle") {
    mouthOpenTarget = 0;
    mouthWideTarget = 0.5;
    breathTarget = 0;
    glowTarget = 1.0;
  } else if (state === "listening") {
    mouthOpenTarget = 0;
    mouthWideTarget = 0.5;
    breathTarget = 0.5 + 0.5 * Math.sin(t * 1.4);
    glowTarget = 1.1;
  } else if (state === "thinking") {
    mouthOpenTarget = 0;
    mouthWideTarget = 0.5;
    breathTarget = 0.3 + 0.3 * Math.sin(t * 2.6) + 0.2 * Math.sin(t * 5.1);
    glowTarget = 0.85;
  } else if (state === "speaking") {
    // Pick a new viseme when the current one expires; coarticulate from the
    // previous shape to the current one with cosine easing over its duration.
    if (t >= nextVisemeAt || currentViseme == null) {
      prevOpen = mouthOpenTarget;
      prevWide = mouthWideTarget;
      currentViseme = pickViseme(currentViseme);
      visemeStartAt = t;
      nextVisemeAt = t + currentViseme.dur;
    }
    const k = Math.min(1, (t - visemeStartAt) / currentViseme.dur);
    const ease = 0.5 - 0.5 * Math.cos(k * Math.PI);
    const visemeOpen = prevOpen + (currentViseme.open - prevOpen) * ease;
    // If the backend is streaming real TTS amplitude (Kokoro path), drive
    // openness from it so the mouth lip-syncs to the actual voice. Fall back
    // to the synthetic viseme value when there's no live audio level (e.g.
    // macOS `say` fallback path, where no envelope is emitted).
    mouthOpenTarget = audioLevel > 0.02 ? Math.min(1, audioLevel * 1.3) : visemeOpen;
    mouthWideTarget = prevWide + (currentViseme.wide - prevWide) * ease;
    breathTarget = 0.15;
    glowTarget = 1.0;
  }

  // Smooth toward targets. Mouth opening is quick (lip muscles), width slower,
  // jaw slower still (mass + inertia).
  mouthOpen += (mouthOpenTarget - mouthOpen) * 0.45;
  mouthWide += (mouthWideTarget - mouthWide) * 0.3;
  jaw += (mouthOpen - jaw) * 0.18;
  breath += (breathTarget - breath) * 0.06;
  glow += (glowTarget - glow) * 0.06;

  uniforms.uMouth.value = mouthOpen;
  uniforms.uMouthWide.value = mouthWide;
  uniforms.uJaw.value = jaw;
  uniforms.uBreath.value = breath;
  uniforms.uGlow.value = glow;
}

function resize(): void {
  if (!canvas || !renderer || !camera) return;
  const w = canvas.clientWidth || canvas.parentElement?.clientWidth || 600;
  const h = canvas.clientHeight || canvas.parentElement?.clientHeight || 600;
  const pr = renderer.getPixelRatio();
  if (canvas.width !== w * pr || canvas.height !== h * pr) {
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  }
}

function animate(): void {
  raf = requestAnimationFrame(animate);
  if (!renderer || !scene || !camera || !clock || !portraitGroup) return;

  resize();
  const dt = Math.min(clock.getDelta(), 0.05);
  const t = clock.getElapsedTime();
  uniforms.uTime.value = t;

  yaw += (targetYaw - yaw) * 0.12;
  pitch += (targetPitch - pitch) * 0.12;
  portraitGroup.rotation.y = yaw;
  portraitGroup.rotation.x = pitch;

  // One-time materialize wipe on load.
  const mat = uniforms.uMaterialize.value as number;
  if (mat < 1.0) {
    uniforms.uMaterialize.value = Math.min(1.0, mat + dt * 0.65);
  }

  updateStateMotion(t);
  renderer.render(scene, camera);
}

// --- Public API ------------------------------------------------------------
export function init(container: HTMLElement): void {
  canvas = document.createElement("canvas");
  canvas.id = "hologram-canvas";
  container.appendChild(canvas);

  renderer = new THREE.WebGLRenderer({
    canvas,
    antialias: true,
    alpha: true,
    powerPreference: "high-performance",
  });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
  renderer.setClearColor(0x000000, 0);

  scene = new THREE.Scene();
  camera = new THREE.PerspectiveCamera(32, 1, 0.1, 100);
  camera.position.set(0, 0, 4.4);
  portraitGroup = new THREE.Group();
  scene.add(portraitGroup);

  const loader = new THREE.TextureLoader();
  portraitTex = loader.load(CFG.src, (tex) => {
    tex.colorSpace = THREE.SRGBColorSpace;
    tex.minFilter = THREE.LinearFilter;
    tex.magFilter = THREE.LinearFilter;
  });
  depthTex = loader.load(CFG.depthSrc, (tex) => {
    tex.minFilter = THREE.LinearFilter;
    tex.magFilter = THREE.LinearFilter;
  });

  uniforms = {
    uTime: { value: 0 },
    uMap: { value: portraitTex },
    uDepth: { value: depthTex },
    uHasDepth: { value: 1.0 },
    uColor: { value: new THREE.Color(CFG.color) },
    uColor2: { value: new THREE.Color(CFG.secondary) },
    uDisplace: { value: CFG.displace },
    uGridDensity: { value: CFG.gridDensity },
    uLineWidth: { value: CFG.lineWidth },
    uPhotoMix: { value: CFG.photoMix },
    uScan: { value: CFG.scan },
    uOpacity: { value: 1.0 },
    uMaterialize: { value: 0.0 },
    uReveal: { value: 1.0 },
    uMouth: { value: 0.0 },
    uMouthWide: { value: 0.0 },
    uJaw: { value: 0.0 },
    uBreath: { value: 0.0 },
    uGlow: { value: 1.0 },
  };

  portraitGeo = new THREE.PlaneGeometry(2.6, 2.6, 280, 280);

  portraitMat = new THREE.ShaderMaterial({
    uniforms: uniforms as THREE.ShaderMaterialParameters["uniforms"],
    vertexShader: VERT,
    fragmentShader: FRAG,
    transparent: true,
    depthWrite: false,
    blending: THREE.AdditiveBlending,
    side: THREE.DoubleSide,
  });
  // Legacy GLSL derivatives extension — harmless on WebGL2, where `fwidth`
  // (used by the grid shader) is core.
  (portraitMat as unknown as { extensions: { derivatives: boolean } }).extensions = {
    derivatives: true,
  };
  portraitGroup.add(new THREE.Mesh(portraitGeo, portraitMat));

  // Chromatic ghost copy — shares the animated uniforms by reference, but
  // overrides colour/opacity/photoMix for a subtle offset aberration.
  const ghostUniforms = Object.assign({}, uniforms, {
    uOpacity: { value: CFG.ghostStrength },
    uColor: { value: new THREE.Color(CFG.ghost) },
    uPhotoMix: { value: 0.05 },
  });
  ghostMat = new THREE.ShaderMaterial({
    uniforms: ghostUniforms as THREE.ShaderMaterialParameters["uniforms"],
    vertexShader: VERT,
    fragmentShader: FRAG,
    transparent: true,
    depthWrite: false,
    blending: THREE.AdditiveBlending,
    side: THREE.DoubleSide,
  });
  (ghostMat as unknown as { extensions: { derivatives: boolean } }).extensions = {
    derivatives: true,
  };
  const ghost = new THREE.Mesh(portraitGeo, ghostMat);
  ghost.position.x = 0.012;
  ghost.position.y = -0.004;
  portraitGroup.add(ghost);

  clock = new THREE.Clock();
  resize();
  animate();
}

export function setState(s: HudState): void {
  state = s;
}

export function setAudioLevel(level: number): void {
  audioTarget = Math.max(0, Math.min(1, level));
}

/// Live signals for the HUD panels (spectrum / signal readout).
export function getSignals(): { state: HudState; mouth: number; breath: number; audio: number } {
  return { state, mouth: mouthOpen, breath, audio: audioLevel };
}

export function destroy(): void {
  cancelAnimationFrame(raf);
  raf = 0;
  portraitGeo?.dispose();
  portraitMat?.dispose();
  ghostMat?.dispose();
  portraitTex?.dispose();
  depthTex?.dispose();
  renderer?.dispose();
  if (canvas) {
    canvas.remove();
    canvas = null;
  }
  renderer = null;
  scene = null;
  camera = null;
  portraitGroup = null;
  clock = null;
}
