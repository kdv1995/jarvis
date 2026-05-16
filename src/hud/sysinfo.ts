// Polls the Rust `get_system_snapshot` command every 500 ms and renders
// the result into the HUD panels. Lightweight — no animation, no diffing,
// just textContent assignments.

import { invoke } from "@tauri-apps/api/core";

const POLL_MS = 500;

interface Identity {
  host: string;
  model: string;
  os: string;
  user: string;
  uptime_secs: number;
  power_source: string;
  wattage: number | null;
}

interface Environment {
  wifi_ssid: string | null;
  wifi_rssi_dbm: number | null;
  battery_pct: number | null;
  battery_time_min: number | null;
  battery_charging: boolean;
  front_app: string | null;
}

interface Pulse {
  cpu_pct: number;
  mem_used_gb: number;
  mem_total_gb: number;
  disk_free_gb: number;
  disk_total_gb: number;
  net_in_kb_s: number;
  net_out_kb_s: number;
  ping_ms: number | null;
  mic_db: number | null;
}

interface AudioIo {
  input_device: string;
  output_device: string;
  sample_rate_hz: number;
  channels: number;
}

interface SystemSnapshot {
  identity: Identity;
  env: Environment;
  pulse: Pulse;
  audio: AudioIo;
  at_ms: number;
}

let timer: number | null = null;

function $(id: string): HTMLElement | null {
  return document.getElementById(id);
}

function setText(id: string, value: string): void {
  const el = $(id);
  if (el && el.textContent !== value) el.textContent = value;
}

function formatUptime(secs: number): string {
  const d = Math.floor(secs / 86_400);
  const h = Math.floor((secs % 86_400) / 3_600);
  const m = Math.floor((secs % 3_600) / 60);
  if (d > 0) return `${d}d ${h}h ${m}m`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

function formatKbps(kb_s: number): string {
  if (kb_s < 1) return "—";
  if (kb_s < 1024) return `${kb_s.toFixed(1)} KB/s`;
  return `${(kb_s / 1024).toFixed(2)} MB/s`;
}

function formatBattery(env: Environment): string {
  if (env.battery_pct == null) return "—";
  const parts = [`${env.battery_pct}%`];
  if (env.battery_charging) parts.push("AC");
  if (env.battery_time_min && env.battery_time_min > 0) {
    const h = Math.floor(env.battery_time_min / 60);
    const m = env.battery_time_min % 60;
    parts.push(`${h}h ${m}m`);
  }
  return parts.join(" · ");
}

function formatWifi(env: Environment): string {
  if (!env.wifi_ssid) return "—";
  if (env.wifi_rssi_dbm != null)
    return `${env.wifi_ssid} · ${env.wifi_rssi_dbm} dBm`;
  return env.wifi_ssid;
}

function render(snap: SystemSnapshot): void {
  // Identity (top-left)
  setText("sys-host", snap.identity.host || "—");
  setText("sys-model", snap.identity.model || "—");
  setText("sys-os", snap.identity.os || "—");
  setText("sys-user", snap.identity.user || "—");
  setText("sys-uptime", formatUptime(snap.identity.uptime_secs));
  setText(
    "sys-power",
    snap.identity.power_source +
      (snap.identity.wattage ? ` · ${snap.identity.wattage} W` : ""),
  );

  // Environment (top-right meta)
  setText("env-wifi", formatWifi(snap.env));
  setText("env-battery", formatBattery(snap.env));
  setText("env-front", snap.env.front_app || "—");

  // Pulse (bottom-left)
  setText("pulse-cpu", `${snap.pulse.cpu_pct.toFixed(0)}%`);
  const memPct =
    snap.pulse.mem_total_gb > 0
      ? Math.round((snap.pulse.mem_used_gb / snap.pulse.mem_total_gb) * 100)
      : 0;
  setText(
    "pulse-mem",
    `${snap.pulse.mem_used_gb.toFixed(1)} / ${snap.pulse.mem_total_gb.toFixed(0)} GB · ${memPct}%`,
  );
  setText(
    "pulse-disk",
    `${snap.pulse.disk_free_gb.toFixed(0)} / ${snap.pulse.disk_total_gb.toFixed(0)} GB free`,
  );
  setText("pulse-net-in", formatKbps(snap.pulse.net_in_kb_s));
  setText("pulse-net-out", formatKbps(snap.pulse.net_out_kb_s));
  setText(
    "pulse-mic",
    snap.pulse.mic_db != null ? `${snap.pulse.mic_db.toFixed(0)} dB` : "—",
  );

  // Audio I/O (bottom-right sub-line)
  setText("audio-input", snap.audio.input_device || "—");
  setText("audio-output", snap.audio.output_device || "—");
}

async function tick(): Promise<void> {
  try {
    const snap = (await invoke("get_system_snapshot")) as SystemSnapshot;
    render(snap);
  } catch (e) {
    // Don't spam the console — Tauri command can fail briefly during teardown.
    console.warn("[sysinfo] poll failed:", e);
  }
}

export function initSysinfo(): void {
  if (timer != null) return;
  // Fire one immediate poll so the panels don't sit on "—" for half a second.
  tick();
  timer = window.setInterval(tick, POLL_MS);
}

export function stopSysinfo(): void {
  if (timer != null) {
    window.clearInterval(timer);
    timer = null;
  }
}
