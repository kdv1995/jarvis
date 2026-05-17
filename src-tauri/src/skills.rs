//! Skills system — YAML-defined workflows that compose fast-path commands.
//!
//! A skill is a named recipe with one or more trigger phrases and an ordered
//! list of steps. When the user speaks a trigger phrase after the wake chime,
//! the pipeline matches it against the in-memory `SkillRegistry` BEFORE the
//! generic fast-path table. On a hit, `run_skill` executes each step (TTS
//! announcements, fast-path commands, raw shell, AppleScript, or waits).
//!
//! Layout:
//! - Bundled defaults live in `src-tauri/skills/*.yaml` (shipped inside the
//!   app bundle's `Contents/Resources/skills/` directory).
//! - User overrides live in `~/.jarvis/skills/*.yaml`. Filenames decide
//!   identity — `~/.jarvis/skills/work-mode.yaml` overrides the bundled
//!   `work-mode.yaml`.
//!
//! Hot-reload: a background `notify` watcher re-parses the user directory
//! and atomically swaps the registry under an `RwLock`. Bundled skills are
//! read once at startup (they only change when the user reinstalls the app).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where the user can drop their own skill YAML files. Created lazily.
pub fn user_skills_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".jarvis").join("skills"))
}

/// Where the app's shipped skills live inside the bundle. Returns None if the
/// running binary isn't inside a `.app` bundle (e.g. `cargo run` from the dev
/// tree — we fall back to the source tree for that case).
pub fn bundled_skills_dir() -> Option<PathBuf> {
    // First: relative to the running executable inside Jarvis.app
    if let Ok(exe) = std::env::current_exe() {
        // exe = /Applications/Jarvis.app/Contents/MacOS/jarvis
        if let Some(macos_dir) = exe.parent() {
            // .../Contents/MacOS  →  .../Contents/Resources/skills
            if let Some(contents_dir) = macos_dir.parent() {
                let path = contents_dir.join("Resources").join("skills");
                if path.is_dir() {
                    return Some(path);
                }
            }
        }
    }
    // Dev fallback: src-tauri/skills/ relative to the executable's target/release
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.parent()?.to_path_buf();
        for _ in 0..4 {
            let candidate = p.join("src-tauri").join("skills");
            if candidate.is_dir() {
                return Some(candidate);
            }
            if !p.pop() {
                break;
            }
        }
    }
    None
}

// ── Schema ──────────────────────────────────────────────────────────────────

/// One named skill, loaded from a YAML file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    /// Phrases that, when matched against the user's utterance, invoke this
    /// skill. Compared lowercased, trimmed of punctuation, and tested with
    /// `starts_with` so "start work mode now" matches "start work mode".
    pub triggers: Vec<String>,
    /// Free-form description; surfaced by "list skills".
    #[serde(default)]
    pub description: String,
    /// Steps to run in order. An empty list is a no-op (legal but useless).
    pub steps: Vec<Step>,
    /// On error: "continue" (default) or "stop".
    #[serde(default = "default_on_error")]
    pub on_error: ErrorPolicy,
}

fn default_on_error() -> ErrorPolicy {
    ErrorPolicy::Continue
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorPolicy {
    Continue,
    Stop,
}

/// One step in a skill. Untagged enum so YAML can use a clean syntax:
///   - say: "Hello"
///   - command: "open Cursor"
///   - shell: "echo hi"
///   - applescript: "tell app ..."
///   - wait: 500
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Step {
    /// TTS announce — `{ say: "text" }`. Spoken via the existing TTS engine.
    Say { say: String },
    /// Run a fast-path voice command verbatim — same as if the user had
    /// spoken it. Recurses through the pipeline's fast-path table.
    Command { command: String },
    /// Run a shell command. Output is logged but not spoken.
    Shell { shell: String },
    /// Run an AppleScript snippet.
    AppleScript { applescript: String },
    /// Sleep for N milliseconds. Useful between actions so e.g. an app has
    /// time to launch before the next step runs.
    Wait { wait: u64 },
}

// ── Registry ────────────────────────────────────────────────────────────────

/// Process-wide skill registry. Atomically swappable via `RwLock`. Inner
/// `Arc<HashMap>` lets readers clone-cheap during lookup without holding the
/// lock for execution duration.
#[derive(Default, Clone)]
pub struct SkillRegistry {
    /// Name → skill. Last-write-wins (user files override bundled).
    inner: Arc<HashMap<String, Skill>>,
}

impl SkillRegistry {
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(HashMap::new()),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn list(&self) -> Vec<&Skill> {
        let mut v: Vec<&Skill> = self.inner.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Find a skill whose triggers match the spoken text. Comparison rules:
    ///  - both sides lowercased, trimmed of trailing punctuation
    ///  - test `cleaned.starts_with(trigger)` (so trailing context like
    ///    "start work mode now please" still matches "start work mode")
    ///  - longest-trigger-wins tiebreaker (so "start work mode now" wins
    ///    over "start work mode" if both are registered)
    pub fn find_match(&self, spoken: &str) -> Option<&Skill> {
        let cleaned = spoken
            .to_lowercase()
            .trim_end_matches(['.', '?', '!', ','])
            .trim()
            .to_string();
        let mut best: Option<(&Skill, usize)> = None;
        for skill in self.inner.values() {
            for trigger in &skill.triggers {
                let t = trigger.to_lowercase();
                let t = t.trim();
                if t.is_empty() {
                    continue;
                }
                if cleaned == t || cleaned.starts_with(&format!("{t} ")) {
                    let score = t.len();
                    if best.map(|(_, s)| score > s).unwrap_or(true) {
                        best = Some((skill, score));
                    }
                }
            }
        }
        best.map(|(s, _)| s)
    }
}

// ── Executor ────────────────────────────────────────────────────────────────

/// Side-effects the skill runner needs from its caller. The pipeline owns
/// these — passing a trait object keeps `skills.rs` free of `pipeline.rs`
/// imports, so the modules stay decoupled and the runner is unit-testable
/// against a mock.
pub trait SkillContext: Send + Sync {
    /// Speak a sentence (TTS).
    fn speak(&self, sentence: &str);
    /// Try to run a fast-path verb. Returns `Some(ack)` on a hit, `None` on miss.
    /// The implementation should NOT speak the ack — the runner decides what
    /// to say (or whether to suppress speech inside a skill).
    fn run_fast_action(&self, command: &str) -> Option<String>;
    /// Run a shell command (`/bin/sh -c <cmd>`). Return Ok on exit 0.
    fn run_shell(&self, cmd: &str) -> Result<(), String>;
    /// Run an AppleScript snippet.
    fn run_applescript(&self, src: &str) -> Result<(), String>;
}

/// Run a skill's steps in order. Returns the final ack string for the
/// pipeline to speak (last `say:` is the natural conclusion).
///
/// On error: respect `on_error` policy. With `Continue` (default), failures
/// are logged but the run proceeds. With `Stop`, the runner bails out.
pub fn run_skill<C: SkillContext>(skill: &Skill, ctx: &C) -> String {
    let mut last_say = format!("Running {}.", skill.name);
    for (idx, step) in skill.steps.iter().enumerate() {
        let result: Result<(), String> = match step {
            Step::Say { say } => {
                ctx.speak(say);
                last_say = say.clone();
                Ok(())
            }
            Step::Command { command } => match ctx.run_fast_action(command) {
                Some(_) => Ok(()), // ack swallowed; the skill's own `say:` steps narrate
                None => Err(format!("no fast-path for: {command}")),
            },
            Step::Shell { shell } => ctx.run_shell(shell),
            Step::AppleScript { applescript } => ctx.run_applescript(applescript),
            Step::Wait { wait } => {
                std::thread::sleep(Duration::from_millis(*wait));
                Ok(())
            }
        };
        if let Err(e) = result {
            eprintln!(
                "[skills] '{}' step {} failed: {e}",
                skill.name,
                idx + 1
            );
            if skill.on_error == ErrorPolicy::Stop {
                ctx.speak("Skill stopped due to an error.");
                return "Skill stopped due to an error.".into();
            }
        }
    }
    last_say
}

// ── Loader ──────────────────────────────────────────────────────────────────

/// Read every `*.yaml` in `dir` and try to parse each as a `Skill`. Bad files
/// log to stderr and are skipped — one broken skill never breaks startup.
fn load_dir(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_yaml = matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yaml") | Some("yml")
        );
        if !is_yaml {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_yaml::from_str::<Skill>(&text) {
                Ok(skill) => {
                    if let Err(e) = validate_skill(&skill) {
                        eprintln!("[skills] reject {}: {e}", path.display());
                        continue;
                    }
                    skills.push(skill);
                }
                Err(e) => eprintln!("[skills] parse error in {}: {e}", path.display()),
            },
            Err(e) => eprintln!("[skills] read error {}: {e}", path.display()),
        }
    }
    skills
}

/// Basic semantic validation. Returns Err if the skill is structurally
/// usable but logically broken (no triggers, empty name).
fn validate_skill(s: &Skill) -> Result<(), String> {
    if s.name.trim().is_empty() {
        return Err("name is empty".into());
    }
    if s.triggers.is_empty() {
        return Err("no triggers".into());
    }
    if s.triggers.iter().all(|t| t.trim().is_empty()) {
        return Err("all triggers are blank".into());
    }
    Ok(())
}

/// Merge bundled + user dirs into a single registry. User wins on collision.
pub fn build_registry() -> SkillRegistry {
    let mut map: HashMap<String, Skill> = HashMap::new();
    let mut count_bundled = 0;
    let mut count_user = 0;
    if let Some(d) = bundled_skills_dir() {
        for s in load_dir(&d) {
            map.insert(s.name.clone(), s);
            count_bundled += 1;
        }
    }
    if let Some(d) = user_skills_dir() {
        // Best-effort create the dir so the user has a clear "drop files here" spot.
        let _ = std::fs::create_dir_all(&d);
        for s in load_dir(&d) {
            // user wins
            map.insert(s.name.clone(), s);
            count_user += 1;
        }
    }
    println!(
        "[skills] loaded {} bundled + {} user → {} total",
        count_bundled,
        count_user,
        map.len()
    );
    SkillRegistry {
        inner: Arc::new(map),
    }
}

// ── Live reload ─────────────────────────────────────────────────────────────

/// Spawn a background thread that watches `~/.jarvis/skills/` for changes and
/// hot-reloads `shared`. Debounced: each event sleeps 250ms then rebuilds.
pub fn spawn_watcher(shared: Arc<RwLock<SkillRegistry>>) {
    use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc;

    let user_dir = match user_skills_dir() {
        Some(d) => d,
        None => return,
    };
    let _ = std::fs::create_dir_all(&user_dir);

    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel::<()>();
        let tx2 = tx.clone();
        let mut watcher: RecommendedWatcher =
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                if let Ok(ev) = res {
                    if matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        let _ = tx2.send(());
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("[skills] watcher init failed: {e}");
                    return;
                }
            };
        if let Err(e) = watcher.watch(&user_dir, RecursiveMode::NonRecursive) {
            eprintln!("[skills] watch failed: {e}");
            return;
        }
        // Debounce loop — coalesce bursts of events into a single rebuild.
        loop {
            // Block for the first event of a burst.
            if rx.recv().is_err() {
                return;
            }
            // Drain whatever else arrives during the debounce window.
            std::thread::sleep(Duration::from_millis(250));
            while rx.try_recv().is_ok() {}
            let fresh = build_registry();
            match shared.write() {
                Ok(mut w) => *w = fresh,
                Err(e) => eprintln!("[skills] reload write-lock poisoned: {e}"),
            }
        }
    });
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_skill() {
        let yaml = r#"
name: hello
triggers: ["say hi"]
steps:
  - say: "Hello there"
"#;
        let s: Skill = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(s.name, "hello");
        assert_eq!(s.triggers, vec!["say hi"]);
        assert_eq!(s.steps.len(), 1);
        assert!(matches!(s.steps[0], Step::Say { .. }));
        assert_eq!(s.on_error, ErrorPolicy::Continue);
    }

    #[test]
    fn parse_multi_step_skill() {
        let yaml = r#"
name: work-mode
triggers: ["start work mode", "work mode on"]
description: DND + dev apps
steps:
  - say: "Starting work mode"
  - command: "open Cursor"
  - wait: 500
  - shell: "echo hi"
  - applescript: "tell application \"Finder\" to activate"
on_error: stop
"#;
        let s: Skill = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(s.steps.len(), 5);
        assert!(matches!(s.steps[0], Step::Say { .. }));
        assert!(matches!(s.steps[1], Step::Command { .. }));
        assert!(matches!(s.steps[2], Step::Wait { .. }));
        assert!(matches!(s.steps[3], Step::Shell { .. }));
        assert!(matches!(s.steps[4], Step::AppleScript { .. }));
        assert_eq!(s.on_error, ErrorPolicy::Stop);
    }

    fn make_registry(skills: Vec<Skill>) -> SkillRegistry {
        let mut map = HashMap::new();
        for s in skills {
            map.insert(s.name.clone(), s);
        }
        SkillRegistry {
            inner: Arc::new(map),
        }
    }

    #[test]
    fn find_match_exact_phrase() {
        let s = Skill {
            name: "work".into(),
            triggers: vec!["start work mode".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let reg = make_registry(vec![s]);
        assert!(reg.find_match("start work mode").is_some());
        assert!(reg.find_match("Start Work Mode.").is_some()); // case + punct
    }

    #[test]
    fn find_match_prefix_with_trailing_text() {
        let s = Skill {
            name: "wm".into(),
            triggers: vec!["work mode".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let reg = make_registry(vec![s]);
        assert!(reg.find_match("work mode now please").is_some());
    }

    #[test]
    fn find_match_longest_trigger_wins() {
        let short = Skill {
            name: "short".into(),
            triggers: vec!["start".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let long = Skill {
            name: "long".into(),
            triggers: vec!["start work mode".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let reg = make_registry(vec![short, long]);
        let hit = reg.find_match("start work mode").unwrap();
        assert_eq!(hit.name, "long");
    }

    #[test]
    fn find_match_no_substring_match() {
        // "morning" should NOT match "start morning routine"
        let s = Skill {
            name: "wm".into(),
            triggers: vec!["start morning routine".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let reg = make_registry(vec![s]);
        assert!(reg.find_match("morning").is_none());
    }

    #[test]
    fn validate_rejects_empty_name() {
        let s = Skill {
            name: "  ".into(),
            triggers: vec!["x".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        assert!(validate_skill(&s).is_err());
    }

    #[test]
    fn validate_rejects_no_triggers() {
        let s = Skill {
            name: "ok".into(),
            triggers: vec![],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        assert!(validate_skill(&s).is_err());
    }

    #[test]
    fn validate_rejects_all_blank_triggers() {
        let s = Skill {
            name: "ok".into(),
            triggers: vec!["  ".into(), "".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        assert!(validate_skill(&s).is_err());
    }

    #[test]
    fn registry_list_sorted_by_name() {
        let a = Skill {
            name: "zebra".into(),
            triggers: vec!["z".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let b = Skill {
            name: "alpha".into(),
            triggers: vec!["a".into()],
            description: "".into(),
            steps: vec![],
            on_error: ErrorPolicy::Continue,
        };
        let reg = make_registry(vec![a, b]);
        let names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    // ───── Step executor (run_skill) — mock context ─────

    use std::sync::Mutex;

    struct MockCtx {
        log: Mutex<Vec<String>>,
        /// Words to fail on for fast_action (return None)
        fail_commands: Vec<String>,
        /// Words to fail on for shell (return Err)
        fail_shell: Vec<String>,
    }

    impl MockCtx {
        fn new() -> Self {
            Self {
                log: Mutex::new(Vec::new()),
                fail_commands: Vec::new(),
                fail_shell: Vec::new(),
            }
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    impl SkillContext for MockCtx {
        fn speak(&self, sentence: &str) {
            self.log.lock().unwrap().push(format!("SAY:{sentence}"));
        }
        fn run_fast_action(&self, command: &str) -> Option<String> {
            self.log.lock().unwrap().push(format!("CMD:{command}"));
            if self.fail_commands.iter().any(|f| command.contains(f)) {
                None
            } else {
                Some(format!("ack({command})"))
            }
        }
        fn run_shell(&self, cmd: &str) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("SH:{cmd}"));
            if self.fail_shell.iter().any(|f| cmd.contains(f)) {
                Err("fail".into())
            } else {
                Ok(())
            }
        }
        fn run_applescript(&self, src: &str) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("AS:{src}"));
            Ok(())
        }
    }

    #[test]
    fn run_skill_executes_steps_in_order() {
        let skill = Skill {
            name: "test".into(),
            triggers: vec!["t".into()],
            description: "".into(),
            steps: vec![
                Step::Say {
                    say: "hello".into(),
                },
                Step::Command {
                    command: "open Safari".into(),
                },
                Step::AppleScript {
                    applescript: "tell app".into(),
                },
                Step::Say {
                    say: "done".into(),
                },
            ],
            on_error: ErrorPolicy::Continue,
        };
        let ctx = MockCtx::new();
        let last = run_skill(&skill, &ctx);
        let log = ctx.take_log();
        assert_eq!(log.len(), 4);
        assert_eq!(log[0], "SAY:hello");
        assert_eq!(log[1], "CMD:open Safari");
        assert_eq!(log[2], "AS:tell app");
        assert_eq!(log[3], "SAY:done");
        assert_eq!(last, "done");
    }

    #[test]
    fn run_skill_continues_past_command_miss_when_policy_is_continue() {
        let skill = Skill {
            name: "test".into(),
            triggers: vec!["t".into()],
            description: "".into(),
            steps: vec![
                Step::Command {
                    command: "unknown thing".into(),
                },
                Step::Say {
                    say: "after".into(),
                },
            ],
            on_error: ErrorPolicy::Continue,
        };
        let mut ctx = MockCtx::new();
        ctx.fail_commands.push("unknown".into());
        let last = run_skill(&skill, &ctx);
        assert_eq!(last, "after");
        let log = ctx.take_log();
        assert!(log.iter().any(|l| l == "SAY:after"));
    }

    #[test]
    fn run_skill_stops_on_error_when_policy_is_stop() {
        let skill = Skill {
            name: "test".into(),
            triggers: vec!["t".into()],
            description: "".into(),
            steps: vec![
                Step::Shell {
                    shell: "bad-thing".into(),
                },
                Step::Say {
                    say: "should not run".into(),
                },
            ],
            on_error: ErrorPolicy::Stop,
        };
        let mut ctx = MockCtx::new();
        ctx.fail_shell.push("bad-thing".into());
        let _ = run_skill(&skill, &ctx);
        let log = ctx.take_log();
        // We executed the shell step (which failed), then the executor
        // spoke a "stopped" message — but never reached "should not run".
        assert!(log.iter().any(|l| l.starts_with("SH:bad-thing")));
        assert!(!log.iter().any(|l| l == "SAY:should not run"));
    }

    #[test]
    fn run_skill_wait_does_not_emit_log() {
        // We can't easily assert exact sleep duration in a test, but we can
        // verify that Wait doesn't show up in the log AND the next step runs.
        let skill = Skill {
            name: "test".into(),
            triggers: vec!["t".into()],
            description: "".into(),
            steps: vec![
                Step::Wait { wait: 10 }, // 10 ms — fast enough
                Step::Say {
                    say: "after wait".into(),
                },
            ],
            on_error: ErrorPolicy::Continue,
        };
        let ctx = MockCtx::new();
        let last = run_skill(&skill, &ctx);
        assert_eq!(last, "after wait");
        let log = ctx.take_log();
        assert_eq!(log, vec!["SAY:after wait"]);
    }
}
