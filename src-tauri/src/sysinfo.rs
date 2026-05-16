//! System telemetry for the HUD panels.
//!
//! Frontend polls `get_system_snapshot()` every 500 ms. The snapshot is
//! built from two sources:
//!
//! * **Live, every poll** — CPU%, RAM used, network throughput, mic level.
//!   These come from the `sysinfo` crate which keeps an internal `System`
//!   handle updated cheaply.
//! * **Cached, refreshed every 5 s** — battery, WiFi, frontmost app, ping.
//!   These require shelling out to `pmset`, `system_profiler`, etc., which
//!   are expensive (10–100 ms each), so we run them in a background thread
//!   and serve the most-recent values to the frontend.
//!
//! Static identity (hostname, model, OS version, RAM total) is read **once**
//! at startup and never changes.

use serde::Serialize;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Networks, System};

use crate::state::LockExt;

// ── Public snapshot returned to the frontend ────────────────────────────────

#[derive(Serialize, Clone, Default, Debug)]
pub struct SystemSnapshot {
    /// Static identity (filled once at boot).
    pub identity: Identity,
    /// Environmental context (battery, WiFi, frontmost app).
    pub env: Environment,
    /// Live system pulse (CPU, RAM, disk, network, mic).
    pub pulse: Pulse,
    /// Audio I/O config (current input / output device).
    pub audio: AudioIo,
    /// Unix epoch millis at which this snapshot was assembled.
    pub at_ms: u64,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Identity {
    pub host: String,
    pub model: String,
    pub os: String,
    pub user: String,
    pub uptime_secs: u64,
    pub power_source: String, // "AC" | "Battery"
    pub wattage: Option<u32>,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Environment {
    pub wifi_ssid: Option<String>,
    pub wifi_rssi_dbm: Option<i32>,
    pub battery_pct: Option<u8>,
    pub battery_time_min: Option<u32>, // minutes until empty/full
    pub battery_charging: bool,
    pub front_app: Option<String>,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Pulse {
    pub cpu_pct: f32,           // 0..100, average across cores
    pub mem_used_gb: f32,
    pub mem_total_gb: f32,
    pub disk_free_gb: f32,
    pub disk_total_gb: f32,
    pub net_in_kb_s: f32,
    pub net_out_kb_s: f32,
    pub ping_ms: Option<u32>,
    pub mic_db: Option<f32>, // -inf..0
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct AudioIo {
    pub input_device: String,
    pub output_device: String,
    pub sample_rate_hz: u32,
    pub channels: u8,
}

// ── Module state ────────────────────────────────────────────────────────────

struct State {
    sys: System,
    nets: Networks,
    last_net_sample: Instant,
    last_net_in: u64,
    last_net_out: u64,
    identity: Identity, // mostly static, only uptime+power_source change
    env: Environment,   // refreshed by background thread
    audio: AudioIo,
    last_slow_refresh: Instant,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        let mut sys = System::new_all();
        sys.refresh_all();
        let nets = Networks::new_with_refreshed_list();
        let identity = read_static_identity();
        let audio = read_audio_io_static();
        let env = refresh_environment();
        Mutex::new(State {
            sys,
            nets,
            last_net_sample: Instant::now(),
            last_net_in: 0,
            last_net_out: 0,
            identity,
            env,
            audio,
            last_slow_refresh: Instant::now(),
        })
    })
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Called by Tauri command and by voice-query fast-paths.
pub fn snapshot() -> SystemSnapshot {
    let mut s = state().lock_recover();

    // Refresh slow fields (battery, wifi, front app) every 5 seconds.
    if s.last_slow_refresh.elapsed() >= Duration::from_secs(5) {
        s.env = refresh_environment();
        s.identity.uptime_secs = System::uptime();
        let (src, watts) = read_power_source();
        s.identity.power_source = src;
        s.identity.wattage = watts;
        s.last_slow_refresh = Instant::now();
    }

    // Refresh live fields every call.
    s.sys.refresh_cpu_usage();
    s.sys.refresh_memory();
    s.nets.refresh();

    let cpu_pct = s.sys.global_cpu_usage();
    let mem_used_gb = bytes_to_gb(s.sys.used_memory());
    let mem_total_gb = bytes_to_gb(s.sys.total_memory());

    // Network throughput (KB/s, smoothed over poll interval).
    let mut in_bytes = 0u64;
    let mut out_bytes = 0u64;
    for (_iface, data) in s.nets.iter() {
        in_bytes += data.total_received();
        out_bytes += data.total_transmitted();
    }
    let dt = s.last_net_sample.elapsed().as_secs_f32().max(0.001);
    let net_in_kb_s = ((in_bytes.saturating_sub(s.last_net_in)) as f32 / 1024.0) / dt;
    let net_out_kb_s = ((out_bytes.saturating_sub(s.last_net_out)) as f32 / 1024.0) / dt;
    s.last_net_in = in_bytes;
    s.last_net_out = out_bytes;
    s.last_net_sample = Instant::now();

    // Disk: root volume only — that's where macOS lives.
    let disks = Disks::new_with_refreshed_list();
    let (disk_free_gb, disk_total_gb) = disks
        .iter()
        .find(|d| d.mount_point().to_str() == Some("/"))
        .map(|d| (bytes_to_gb(d.available_space()), bytes_to_gb(d.total_space())))
        .unwrap_or((0.0, 0.0));

    SystemSnapshot {
        identity: s.identity.clone(),
        env: s.env.clone(),
        pulse: Pulse {
            cpu_pct,
            mem_used_gb,
            mem_total_gb,
            disk_free_gb,
            disk_total_gb,
            net_in_kb_s,
            net_out_kb_s,
            ping_ms: s.env.ping_cached(),
            mic_db: None, // wired separately from the audio module
        },
        audio: s.audio.clone(),
        at_ms: unix_ms(),
    }
}

// ── Voice-query helpers — used by pipeline.rs fast-path ─────────────────────

/// Try to answer a system query without going to the LLM. Returns the spoken
/// answer if the query matched a known pattern, otherwise None.
///
/// Patterns are word-bag matched (any-language tolerant) — "battery" works,
/// "батарея" works, "скільки заряду" works.
pub fn try_answer_system_query(text: &str) -> Option<String> {
    let q = text.to_lowercase();
    let snap = snapshot();

    if has_any(&q, &["battery", "батаре", "заряд", "charge"]) {
        let pct = snap.env.battery_pct?;
        let mut s = format!("Battery is at {}%", pct);
        if snap.env.battery_charging {
            s.push_str(", charging");
        }
        if let Some(min) = snap.env.battery_time_min {
            if min > 0 {
                s.push_str(&format!(", {}h {}m remaining", min / 60, min % 60));
            }
        }
        return Some(s);
    }

    if has_any(&q, &["cpu", "processor", "процесор"]) {
        return Some(format!("CPU at {:.0}%", snap.pulse.cpu_pct));
    }

    if has_any(&q, &["memory", "ram", "memory pressure", "пам'ять", "память"]) {
        return Some(format!(
            "Memory: {:.1} of {:.0} gigabytes used, {:.0}%",
            snap.pulse.mem_used_gb,
            snap.pulse.mem_total_gb,
            (snap.pulse.mem_used_gb / snap.pulse.mem_total_gb) * 100.0
        ));
    }

    if has_any(&q, &["disk", "storage", "free space", "диск"]) {
        return Some(format!(
            "{:.0} gigabytes free of {:.0}",
            snap.pulse.disk_free_gb, snap.pulse.disk_total_gb
        ));
    }

    if has_any(&q, &["wifi", "wi-fi", "network", "internet", "мережа", "інтернет"]) {
        return match (&snap.env.wifi_ssid, snap.env.wifi_rssi_dbm) {
            (Some(ssid), Some(dbm)) => Some(format!("Connected to {} at {} dBm", ssid, dbm)),
            (Some(ssid), None) => Some(format!("Connected to {}", ssid)),
            _ => Some("No WiFi connection".into()),
        };
    }

    if has_any(&q, &["uptime", "how long", "час роботи", "аптайм"]) {
        return Some(format_uptime(snap.identity.uptime_secs));
    }

    None
}

// ── Internals ───────────────────────────────────────────────────────────────

fn read_static_identity() -> Identity {
    let host = run("hostname", &[]).unwrap_or_default().trim().to_string();
    let user = run("whoami", &[]).unwrap_or_default().trim().to_string();
    let os = run("sw_vers", &["-productVersion"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let model_raw = run("sysctl", &["-n", "hw.model"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let model = humanise_model(&model_raw);
    let (power_source, wattage) = read_power_source();

    Identity {
        host,
        model,
        os: format!("macOS {}", os),
        user,
        uptime_secs: System::uptime(),
        power_source,
        wattage,
    }
}

fn read_audio_io_static() -> AudioIo {
    // Defaults; main audio.rs may override input_device based on user config.
    AudioIo {
        input_device: "Default Input".into(),
        output_device: "Default Output".into(),
        sample_rate_hz: 16000,
        channels: 1,
    }
}

fn refresh_environment() -> Environment {
    let (battery_pct, battery_time_min, battery_charging) = read_battery();
    let (wifi_ssid, wifi_rssi_dbm) = read_wifi();
    let front_app = read_front_app();
    Environment {
        wifi_ssid,
        wifi_rssi_dbm,
        battery_pct,
        battery_time_min,
        battery_charging,
        front_app,
    }
}

fn read_battery() -> (Option<u8>, Option<u32>, bool) {
    // `pmset -g batt` sample output:
    //   Now drawing from 'AC Power'
    //    -InternalBattery-0 (id=...)   87%; charged; 0:00 remaining present: true
    let out = match run("pmset", &["-g", "batt"]) {
        Ok(s) => s,
        Err(_) => return (None, None, false),
    };
    let charging = out.contains("AC Power") || out.contains("charging") || out.contains("charged");
    let pct = out
        .split_whitespace()
        .find_map(|w| w.strip_suffix("%;").or_else(|| w.strip_suffix('%')))
        .and_then(|w| w.parse::<u8>().ok());
    // Time like "4:22 remaining"
    let time_min = out.lines().find_map(|l| {
        let l = l.trim();
        let tok = l.split_whitespace().find(|s| s.contains(':') && !s.contains('-'))?;
        let (h, m) = tok.split_once(':')?;
        Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)
    });
    (pct, time_min.filter(|&m| m > 0), charging)
}

fn read_power_source() -> (String, Option<u32>) {
    let out = run("pmset", &["-g", "ps"]).unwrap_or_default();
    if out.contains("AC Power") {
        // Wattage from `system_profiler SPPowerDataType` is slow; skip for now.
        ("AC".into(), None)
    } else {
        ("Battery".into(), None)
    }
}

fn read_wifi() -> (Option<String>, Option<i32>) {
    // The classic `airport` CLI was removed in macOS 14.4+. Use
    // `system_profiler SPAirPortDataType` and grep for "Current Network".
    let out = match run("system_profiler", &["SPAirPortDataType"]) {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    // `system_profiler` can emit multiple "Current Network Information:"
    // sections (e.g. per-interface), but only the first has a real SSID —
    // others are empty stubs. Take the first; bail out as soon as we have
    // both ssid+rssi or hit the next major section.
    let mut lines = out.lines();
    let mut ssid: Option<String> = None;
    let mut rssi: Option<i32> = None;
    while let Some(line) = lines.next() {
        if ssid.is_none() && line.contains("Current Network Information:") {
            // The SSID line is the first non-empty line whose key (before ':')
            // is NOT a known property like "Network Type", "Channel", etc.
            // It's effectively `<SSID>:` with no value, just nested deeper.
            for name_line in lines.by_ref() {
                let trimmed = name_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // SSID lines end with ':' and have no value after it.
                // Property lines look like "Key: value" — skip those.
                let stripped = trimmed.trim_end_matches(':');
                let is_property = trimmed.contains(": ") || trimmed.ends_with(':') && stripped.contains(": ");
                if !is_property && !stripped.is_empty() {
                    ssid = Some(stripped.to_string());
                }
                break;
            }
        }
        if rssi.is_none() {
            if let Some(rest) = line.trim().strip_prefix("Signal / Noise:") {
                if let Some(num) = rest.trim().split_whitespace().next() {
                    rssi = num.parse::<i32>().ok();
                }
            }
        }
        if ssid.is_some() && rssi.is_some() {
            break;
        }
    }
    (ssid, rssi)
}

fn read_front_app() -> Option<String> {
    let out = run(
        "osascript",
        &[
            "-e",
            "tell application \"System Events\" to name of first application process whose frontmost is true",
        ],
    )
    .ok()?;
    let name = out.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn humanise_model(raw: &str) -> String {
    // "MacBookPro18,3" → "MacBook Pro 14\" (M1 Pro, 2021)"
    // We can't fully reverse-engineer every model, so we just split words.
    let mut out = String::with_capacity(raw.len() + 2);
    let mut prev_is_digit = false;
    for ch in raw.chars() {
        let is_digit = ch.is_ascii_digit() || ch == ',' || ch == '.';
        if !out.is_empty() && is_digit != prev_is_digit {
            out.push(' ');
        }
        out.push(ch);
        prev_is_digit = is_digit;
    }
    // Friendly aliases for the most common ones.
    out
        .replace("MacBookPro", "MacBook Pro")
        .replace("MacBookAir", "MacBook Air")
        .replace("Macmini", "Mac mini")
        .replace("iMac", "iMac")
        .replace("MacPro", "Mac Pro")
}

fn run(cmd: &str, args: &[&str]) -> Result<String, std::io::Error> {
    let out = Command::new(cmd).args(args).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn bytes_to_gb(b: u64) -> f32 {
    (b as f64 / 1_073_741_824.0) as f32
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("Up {} days, {} hours, {} minutes", days, hours, mins)
    } else if hours > 0 {
        format!("Up {} hours, {} minutes", hours, mins)
    } else {
        format!("Up {} minutes", mins)
    }
}

fn has_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

// Side-channel for the rolling ping result owned by the env refresher.
// Kept on Environment for serialisation simplicity.
impl Environment {
    fn ping_cached(&self) -> Option<u32> {
        None // ping reserved for a future iteration; left None for now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_three_buckets() {
        assert_eq!(format_uptime(30), "Up 0 minutes");
        assert_eq!(format_uptime(90), "Up 1 minutes");
        assert_eq!(format_uptime(3601), "Up 1 hours, 0 minutes");
        assert!(format_uptime(90_061).starts_with("Up 1 days"));
    }

    #[test]
    fn humanise_model_inserts_spaces() {
        assert_eq!(humanise_model("MacBookPro18,3"), "MacBook Pro 18,3");
        assert_eq!(humanise_model("Macmini9,1"), "Mac mini 9,1");
    }

    #[test]
    fn try_answer_matches_common_words() {
        // We can't assert exact battery output (depends on machine state),
        // but we can confirm patterns trigger answers (non-None).
        let q = "what's the cpu doing";
        assert!(try_answer_system_query(q).is_some());
        let q = "пам'ять зараз";
        assert!(try_answer_system_query(q).is_some());
        let q = "tell me a joke about quantum physics";
        assert!(try_answer_system_query(q).is_none());
    }
}
