//! Bridge to the `claude` CLI (Claude Code) — Jarvis's "brain".
//!
//! **Architecture**: a *persistent* `claude -p --input-format stream-json`
//! process runs in the background for the lifetime of the app. Each voice
//! command writes a JSON-encoded user message to its stdin and reads stream-
//! json events from stdout until the terminal `result` event. The Node
//! runtime, plugin sync, model warmup and CLAUDE.md discovery are paid ONCE
//! at process spawn — every subsequent voice command skips that ~3 s tax.
//! Without this design, spawning a fresh CLI per call costs ~6 s end-to-end;
//! with it, ~1.5-2 s.
//!
//! The session is respawned on (a) child death, (b) IO error mid-stream, or
//! (c) after 40 successful turns to prevent context-window bloat. Each
//! respawn is a one-off ~3 s tax; intervening calls stay fast.
//!
//! [`run_claude_streaming`] is the public entry point. It hides the session
//! behind a process-wide singleton so the rest of the codebase doesn't have
//! to think about lifecycle.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::state::LockExt;

/// Build a PATH that includes the usual locations a `claude` install lives in.
/// A bundled macOS `.app` inherits only a minimal PATH, so we have to help it
/// find the CLI.
fn augmented_path() -> String {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{home}/.local/bin"));
        dirs.push(format!("{home}/.claude/local"));
        dirs.push(format!("{home}/.cargo/bin"));
        dirs.push(format!("{home}/bin"));
    }
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    if let Ok(existing) = std::env::var("PATH") {
        dirs.push(existing);
    } else {
        dirs.push("/usr/bin:/bin:/usr/sbin:/sbin".into());
    }
    dirs.join(":")
}

/// Appended to Claude's system prompt for every voice command. Tells Claude it
/// actually controls this Mac and must *use its tools to act* — without this it
/// falls back to its coding-assistant framing and replies "I can't do that".
/// Also keeps replies short (faster to generate, faster for TTS to speak) and
/// stops it wandering into docs/codebase exploration on a quick spoken ask.
const VOICE_SYSTEM_PROMPT: &str = "You are Jarvis, a voice assistant that runs on and \
fully controls this Mac. Whenever the user asks you to DO something — open or quit any \
app, click a button, type text into any app, change a setting, run a command, edit a \
file, search the web, send a message, control music, take a screenshot, anything — \
actually carry it out using your Bash tool. NEVER say \"I can't\" or \"I don't have the \
ability\" — you do; just do it.\n\
\n\
YOUR TOOLBOX — pick the right tool, don't reach for Bash when a dedicated tool exists:\n\
• **GitHub**: you have GitHub tools (issues, PRs, repos, commits). Use them for \
\"create an issue\", \"open a PR\", \"check my pull requests\", etc. — NOT `gh` CLI \
through Bash.\n\
• **Supabase**: you have Supabase tools (SQL, edge functions, migrations, logs). \
Use them for \"query my database\", \"deploy an edge function\", \"check supabase \
logs\", etc.\n\
• **Vercel** (if MCP loaded): deploy, list deployments, check logs.\n\
• **Web** (`WebFetch` / `WebSearch`): fetching URLs, searching the web. Use for \
\"what's the latest news on X\", \"check stackoverflow for Y\".\n\
• **Mac control** via Bash + osascript: anything OS-level (open apps, files, \
volume, screenshots). See the Mac toolkit below.\n\
\n\
THE MAC TOOLKIT (Bash + osascript, for OS-level stuff):\n\
• `open <url>` opens a website. `open -a \"<App Name>\"` launches an app. \
`open <file-path>` opens a file in its default app.\n\
• `ls /Applications | grep -i <name>` finds the exact name of an installed app if you're \
unsure (run this before guessing).\n\
• `osascript -e '<applescript>'` (or multiple `-e` lines) drives any Mac app. Key \
patterns:\n\
    - **Activate an app** — ALWAYS use the `jarvis-open` shell helper, NOT \
raw `osascript ... to activate`. The helper activates the app AND positions \
its window on the user's Dell 4K external display (the user is working on \
the Dell, not the built-in screen — windows MUST land there). It does \
runtime display detection so the coordinates are always correct.\n\
      Usage:   jarvis-open \"<App Name>\"\n\
      Examples:\n\
        jarvis-open Calendar\n\
        jarvis-open \"Visual Studio Code\"\n\
        jarvis-open Chrome\n\
      If you ever see yourself writing `tell application X to activate`, STOP and \
use `jarvis-open` instead. The only exception is when you need to chain \
keystrokes into an app you just activated (paste text, press keys) — in that \
case use `jarvis-open X` first, then do the keystroke as a separate osascript.\n\
    - Press a keyboard shortcut: tell application \"System Events\" to keystroke \"n\" using command down\n\
    - Press a single special key by code: tell application \"System Events\" to key code 36  \
(36 = Return, 53 = Escape, 51 = Backspace, 49 = Space, 124 = Right, 123 = Left, 125 = Down, 126 = Up)\n\
    - Type short text: tell application \"System Events\" to keystroke \"<short text>\"\n\
• For LONG or quoted text into any app, ALWAYS use the clipboard pattern, never \
`keystroke`. Combine with `jarvis-open` so the target window is on the Dell:\n\
    printf '%s' \"<text>\" | pbcopy && jarvis-open \"<App>\" && osascript \\\n\
      -e 'tell application \"System Events\" to keystroke \"v\" using command down' \\\n\
      -e 'tell application \"System Events\" to key code 36'\n\
  (Add `-e 'tell application \"System Events\" to keystroke \"n\" using command down' -e 'delay 0.2'` before the paste line if the app needs a fresh window/chat first.)\n\
• `pbcopy` / `pbpaste` for clipboard. `say \"text\"` is reserved for the system voice — do not use it; speak by returning text to the user instead.\n\
\n\
COMMON TARGETS (the user's installed apps include):\n\
• \"Claude\" — Claude desktop. Use the new-chat + paste recipe (Cmd+N, then Cmd+V, then Return).\n\
• \"Codex\" — OpenAI Codex desktop. Same new-chat + paste recipe.\n\
• \"Terminal\" — to start a fresh `claude` CLI session in a new window:\n\
    osascript -e 'tell application \"Terminal\" to do script \"claude \\\"<prompt>\\\"\"'\n\
  Escape inner quotes. Works for any CLI, not just `claude`.\n\
\n\
PICK THE TARGET from the user's words: \"ask claude\" → Claude.app, \"ask codex\" / \
\"ask openai\" → Codex.app, \"in terminal\" / \"spawn claude\" / \"new claude session\" \
→ Terminal. If the target isn't obvious, run `ls /Applications | grep -i <hint>` first.\n\
\n\
After doing the action, reply (or for a plain question, just answer) in one or two short \
conversational sentences meant to be read aloud by text-to-speech — no markdown, no lists, \
no code blocks, no emoji.\n\
\n\
CRITICAL — VOICE STREAMING: Your response is streamed to text-to-speech the moment your \
first sentence completes. Lead with the answer or confirmation directly. Never start with \
filler like \"Let me check on that\", \"I'll help you with that\", \"Sure, I can do that\" \
— that wastes the user's first second. Examples:\n\
• Good: \"It's 72 degrees and clear.\" / \"Done, Instagram is open.\" / \"Three meetings today.\"\n\
• Bad: \"Let me check the weather for you.\" / \"Sure, I'll open that now.\" / \"Looking at your calendar…\"\n\
\n\
Do not explore the codebase or read documentation unless explicitly asked.";

/// The local Ollama model used for fast, knowledge-only replies.
const LOCAL_MODEL: &str = "llama3.2:3b";

/// Answer `prompt` with the local Ollama model — no network, no process
/// cold-start, ~1 s warm. Knowledge-only: it has no tools and no system
/// access, so the pipeline only routes pure questions here (see `needs_cli`).
pub fn run_local(prompt: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": LOCAL_MODEL,
        "messages": [
            { "role": "system", "content": VOICE_SYSTEM_PROMPT },
            { "role": "user", "content": prompt },
        ],
        "stream": false,
    });

    let resp = reqwest::blocking::Client::new()
        .post("http://localhost:11434/api/chat")
        .json(&body)
        .send()
        .map_err(|e| format!("local model request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(format!("local model returned {status}: {}", body.trim()));
    }

    let json: Value = resp
        .json()
        .map_err(|e| format!("could not parse local model response: {e}"))?;

    let answer = json["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if answer.is_empty() {
        return Err("local model returned an empty response".into());
    }
    Ok(answer)
}

/// A neutral scratch directory for the `claude` CLI to run in. Kept empty of
/// any `CLAUDE.md`, so the CLI skips project-context auto-discovery — walking
/// the project tree to load its big `CLAUDE.md` (plus the graphify rules) is a
/// real slice of the cold-start cost and irrelevant to a quick spoken command.
fn agent_cwd() -> PathBuf {
    let dir = std::env::temp_dir().join("jarvis-agent");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Write a minimal `settings.json` to a stable temp path and return it. The
/// file disables every globally-enabled plugin (claude-mem, explanatory-
/// output-style, etc.) and every hook for our spawned `claude` calls without
/// touching the user's real `~/.claude/settings.json`. Saves ~3 seconds per
/// invocation versus the default settings.
/// Two settings files, one per session mode. Both are rewritten on every
/// startup so they're guaranteed to exist after /tmp cleanups.
///
/// - **Fast**: empty `enabledPlugins` + empty `hooks` — minimal system prompt,
///   ~2 s per turn after the first. Used for general chatty commands.
/// - **Power**: `github`, `supabase`, `frontend-design` enabled — gives the
///   brain real tool surfaces (issues, PRs, SQL, edge functions, UI gen).
///   ~4-5 s per turn after the first. Used when the user's command mentions
///   any of those domains.
fn jarvis_settings_fast() -> PathBuf {
    let path = std::env::temp_dir().join("jarvis-claude-settings-fast.json");
    let _ = std::fs::write(&path, r#"{"enabledPlugins": {}, "hooks": {}}"#);
    path
}
fn jarvis_settings_power() -> PathBuf {
    let path = std::env::temp_dir().join("jarvis-claude-settings-power.json");
    let _ = std::fs::write(
        &path,
        r#"{"enabledPlugins":{"github@claude-plugins-official":true,"supabase@claude-plugins-official":true,"frontend-design@claude-plugins-official":true},"hooks":{}}"#,
    );
    path
}

/// Which CLI flag/settings regime to spawn with — see [`jarvis_settings_fast`]
/// vs [`jarvis_settings_power`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMode {
    Fast,
    Power,
}

/// `true` when `ANTHROPIC_API_KEY` is set to a non-empty value, meaning we can
/// use the much faster `--bare` mode. Without it we stack skip-flags instead.
fn has_api_key() -> bool {
    std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// One interesting event from a streaming claude run. The caller decides what
/// to do with each (emit to HUD, hand to TTS, etc.).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A complete chunk of user-facing assistant text. Claude Code emits one
    /// of these per content block (one for `thinking`, one for the final
    /// `text` the user sees) — *not* per token. So each `Text` payload is
    /// already a complete utterance, safe to hand to TTS as a unit.
    Text(String),
    /// Claude is about to invoke a tool. `summary` is a short, human-readable
    /// description (e.g. `"Bash: open -a Claude"`) meant for the HUD trace.
    /// `name` (the raw tool name like `"Bash"`) is exposed for callers that
    /// want to filter or color-code by tool type but isn't required.
    #[allow(dead_code)]
    ToolUse { name: String, summary: String },
    /// A tool finished. `ok = false` means it errored.
    ToolResult { ok: bool },
}

// ---------------------------------------------------------------------------
// ClaudeSession — the long-running CLI process that all voice commands share.
//
// Lifecycle:
//   - Spawned lazily on first use (or eagerly via `prewarm()`).
//   - Each `ask()` writes a user message to stdin, reads events from stdout
//     until the terminal `result`, and returns the final text.
//   - Respawned on (a) child death, (b) IO failure during a request, or
//     (c) every MAX_TURNS calls (to prevent context-window bloat).
// ---------------------------------------------------------------------------

/// Cap on session reuse — after this many successful turns we kill and
/// respawn so the conversation history can't grow unbounded. Tuned for
/// voice: each turn is ~50-100 input + ~50 output tokens, so 40 turns is
/// roughly 4-6 K tokens — well under Haiku's 200 K context but bounded.
const MAX_TURNS: u32 = 40;

/// Hard wall-clock cap on a single `ask()` call. If the claude CLI hasn't
/// produced the terminal `result` event within this window, a watchdog
/// thread SIGKILLs the child — its `stdout` closes, the blocked
/// `read_line()` returns 0 bytes, and `ask()` reports "session closed
/// unexpectedly". The singleton wrapper then respawns on the next call.
///
/// Chosen generously: a cold-start Power session with plugins can take ~10s
/// legitimately; we only want to catch *real* hangs (OAuth refresh stuck,
/// network black hole), not slow-but-progressing requests.
const ASK_TIMEOUT: Duration = Duration::from_secs(45);

struct ClaudeSession {
    mode: SessionMode,
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: Option<BufReader<ChildStdout>>,
    turns: u32,
}

impl ClaudeSession {
    const fn new(mode: SessionMode) -> Self {
        Self {
            mode,
            child: None,
            stdin: None,
            stdout: None,
            turns: 0,
        }
    }

    /// `true` when the underlying child is still alive and ready to accept
    /// another prompt. Cheap to call (uses `try_wait` which doesn't block).
    fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Tear the current session down (if any), reaping the child to avoid
    /// zombies. Safe to call on a fresh / already-dead session.
    fn kill(&mut self) {
        self.stdin = None;
        self.stdout = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.turns = 0;
    }

    /// Ensure a live `claude` child is ready. (Re)spawns if missing, dead, or
    /// past the turn cap.
    fn ensure_alive(&mut self) -> Result<(), String> {
        if self.alive() && self.turns < MAX_TURNS {
            return Ok(());
        }
        if self.turns >= MAX_TURNS {
            println!(
                "[agent] session turn cap reached ({}/{}); respawning to clear context",
                self.turns, MAX_TURNS
            );
        }
        self.kill();
        self.spawn()
    }

    /// Spawn a fresh `claude -p` process configured for persistent stream-json
    /// I/O. The same skip-flag logic as the old spawn-per-call path, just
    /// applied once instead of on every voice command.
    fn spawn(&mut self) -> Result<(), String> {
        let mut cmd = Command::new("claude");
        cmd.current_dir(agent_cwd())
            .env("PATH", augmented_path())
            .arg("-p")
            // stream-json on BOTH input and output: we feed user messages in
            // line by line, claude responds with events line by line.
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--model")
            .arg("haiku")
            .arg("--append-system-prompt")
            .arg(VOICE_SYSTEM_PROMPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if has_api_key() {
            // `--bare` skips hooks, plugins, MCPs, keychain reads, CLAUDE.md
            // discovery — the fastest path. Auth strictly via API key. Same
            // for both modes since the tool surface comes from --append-
            // system-prompt anyway.
            cmd.arg("--bare");
        } else {
            cmd.arg("--no-session-persistence");
            cmd.arg("--exclude-dynamic-system-prompt-sections");
            match self.mode {
                SessionMode::Fast => {
                    // Empty enabledPlugins + strict MCP → no plugins, no MCPs.
                    // ~2 s per turn after the first.
                    cmd.arg("--settings").arg(jarvis_settings_fast());
                    cmd.arg("--strict-mcp-config");
                    cmd.arg("--disable-slash-commands");
                }
                SessionMode::Power => {
                    // Selective plugins (github / supabase / frontend-design)
                    // + drop --strict-mcp-config so user-level MCPs load (your
                    // ~/.claude.json supabase MCP + anything else you've added
                    // via `claude mcp add`). ~4-5 s per turn after the first
                    // but with real tool surfaces.
                    cmd.arg("--settings").arg(jarvis_settings_power());
                }
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to launch `claude`: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "claude stdin pipe missing".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "claude stdout pipe missing".to_string())?;

        self.stdin = Some(BufWriter::new(stdin));
        self.stdout = Some(BufReader::new(stdout));
        self.child = Some(child);
        self.turns = 0;
        println!("[agent] claude session spawned ({:?})", self.mode);
        Ok(())
    }

    /// The hot path. Writes `prompt` as a user message, then reads events
    /// until the terminal `result`, dispatching each interesting one to
    /// `on_event`. Returns the final assembled text.
    ///
    /// Wrapped in a watchdog: a sibling thread holds the child's PID. If
    /// `read_loop` doesn't signal completion within [`ASK_TIMEOUT`], the
    /// watchdog `kill -9`s the child. That closes stdout, unblocks the read,
    /// and we surface a clear error instead of hanging the voice loop.
    fn ask<F>(&mut self, prompt: &str, on_event: F) -> Result<String, String>
    where
        F: FnMut(StreamEvent),
    {
        self.ensure_alive()?;

        // Snapshot the PID so the watchdog can kill the child without needing
        // a shared handle to `self`. Safe: a respawn between now and SIGKILL
        // would just kill an unrelated process — but `ensure_alive()` only
        // respawns inside this call, before we get here.
        let pid = self
            .child
            .as_ref()
            .map(|c| c.id())
            .ok_or("session child gone after ensure_alive")?;

        // Watchdog: armed before we write the prompt, disarmed (via `done_tx`)
        // when we return. On timeout, SIGKILL the claude process.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if done_rx.recv_timeout(ASK_TIMEOUT).is_err() {
                eprintln!(
                    "[agent] ask() timed out after {:?} — killing claude pid {}",
                    ASK_TIMEOUT, pid
                );
                // `/bin/kill -9` is the most portable way to send SIGKILL
                // without pulling in the `libc` crate.
                let _ = Command::new("/bin/kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .status();
            }
        });

        // Run the actual read loop. Helper struct lets us disarm the watchdog
        // even on early `?` returns.
        struct Disarm(Option<mpsc::Sender<()>>);
        impl Drop for Disarm {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let _disarm = Disarm(Some(done_tx));

        let result = self.ask_inner(prompt, on_event);
        // `_disarm` drops here → sends "done" → watchdog exits without killing.
        drop(_disarm);
        // Best-effort join so the watchdog thread isn't leaked.
        let _ = watchdog.join();
        result
    }

    /// Inner read loop, factored out of `ask()` so the watchdog can wrap it
    /// cleanly. All the original parser logic; nothing semantic changed.
    fn ask_inner<F>(&mut self, prompt: &str, mut on_event: F) -> Result<String, String>
    where
        F: FnMut(StreamEvent),
    {
        // Write the user message.
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        });
        {
            let stdin = self.stdin.as_mut().ok_or("session stdin gone")?;
            writeln!(stdin, "{msg}").map_err(|e| format!("session stdin write: {e}"))?;
            stdin
                .flush()
                .map_err(|e| format!("session stdin flush: {e}"))?;
        }

        // Read events until the terminal `result`. This is the SAME parser
        // logic the old spawn-per-call path used — only the transport changed.
        let stdout = self.stdout.as_mut().ok_or("session stdout gone")?;
        let mut emitted_texts: Vec<String> = Vec::new();
        let mut final_text: Option<String> = None;
        let mut terminal_error: Option<String> = None;

        loop {
            let mut line = String::new();
            let n = stdout
                .read_line(&mut line)
                .map_err(|e| format!("session stdout read: {e}"))?;
            if n == 0 {
                // EOF — child died mid-request. Surface as an error; the
                // singleton wrapper will respawn next call.
                return Err("claude session closed unexpectedly".to_string());
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            match value.get("type").and_then(Value::as_str).unwrap_or("") {
                "assistant" => {
                    let Some(content) = value.pointer("/message/content").and_then(Value::as_array)
                    else {
                        continue;
                    };
                    for block in content {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    let text = text.trim();
                                    if !text.is_empty() && !emitted_texts.iter().any(|t| t == text)
                                    {
                                        emitted_texts.push(text.to_string());
                                        on_event(StreamEvent::Text(text.to_string()));
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string();
                                let summary = brief_tool_summary(&name, block.get("input"));
                                on_event(StreamEvent::ToolUse { name, summary });
                            }
                            _ => {}
                        }
                    }
                }
                "user" => {
                    let Some(content) = value.pointer("/message/content").and_then(Value::as_array)
                    else {
                        continue;
                    };
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                            let ok = !block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            on_event(StreamEvent::ToolResult { ok });
                        }
                    }
                }
                "result" => {
                    if value.get("subtype").and_then(Value::as_str) == Some("success") {
                        if let Some(text) = value.get("result").and_then(Value::as_str) {
                            final_text = Some(text.trim().to_string());
                        }
                    } else if let Some(subtype) = value.get("subtype").and_then(Value::as_str) {
                        terminal_error = Some(format!("claude reported: {subtype}"));
                    }
                    // `result` is the per-turn terminator. Stop reading; leave
                    // the process alive for the next ask().
                    break;
                }
                _ => {}
            }
        }

        if let Some(err) = terminal_error {
            return Err(err);
        }
        self.turns += 1;
        final_text.ok_or_else(|| "no `result` event found in claude output".to_string())
    }
}

/// Two process-wide singletons, one per session mode. Wrapping each in a
/// Mutex serializes concurrent voice commands *within a mode* — claude is
/// single-turn-per-session anyway, and interleaved stdin writes would
/// corrupt the protocol. Across modes they can proceed in parallel (rare
/// because one user, one mic, but the architecture allows it).
static SESSION_FAST: OnceLock<Mutex<ClaudeSession>> = OnceLock::new();
static SESSION_POWER: OnceLock<Mutex<ClaudeSession>> = OnceLock::new();

fn session_for(mode: SessionMode) -> &'static Mutex<ClaudeSession> {
    match mode {
        SessionMode::Fast => {
            SESSION_FAST.get_or_init(|| Mutex::new(ClaudeSession::new(SessionMode::Fast)))
        }
        SessionMode::Power => {
            SESSION_POWER.get_or_init(|| Mutex::new(ClaudeSession::new(SessionMode::Power)))
        }
    }
}

/// Decide whether a command needs Power-mode tools (GitHub, Supabase,
/// frontend-design, plus any user-level MCPs). Deterministic keyword match
/// — no LLM round-trip burned on routing. Biased toward Fast so simple
/// chatty stuff stays snappy.
fn route_to_power(command: &str) -> bool {
    let lower = command.to_lowercase();
    // Keywords that imply needing the Power session's tool surface.
    const POWER_KEYWORDS: &[&str] = &[
        // GitHub-y
        "github",
        "git hub",
        "pull request",
        "merge request",
        "pr ",
        "issue",
        "issues",
        "commit",
        "branch",
        "fork",
        "release",
        "repo",
        // Supabase / databases
        "supabase",
        "database",
        "sql",
        "query my",
        "query the",
        "edge function",
        "row level security",
        "schema",
        // Vercel-y (uses MCP if configured, else `vercel` CLI via Bash)
        "vercel",
        "deploy",
        "deployment",
        // Project mgmt (if user adds Linear/Notion/Jira MCPs later)
        "linear",
        "notion",
        "jira",
        "ticket",
        "asana",
        "clickup",
        // Productivity (if user adds Google MCPs later)
        "calendar event",
        "schedule a meeting",
        "send an email",
        "draft an email",
        "google drive",
        "gmail",
        // Web / fetch (already in Power via default tools)
        "stack overflow",
        "stackoverflow",
        // Frontend-design (UI generation)
        "design a",
        "mockup",
        "wireframe",
    ];
    POWER_KEYWORDS.iter().any(|k| lower.contains(k))
}

/// Eagerly spawn BOTH sessions in parallel at app startup so the first call
/// to either skips its cold-start. Silent no-op on failure — the first real
/// `ask()` will retry and surface any error there.
pub fn prewarm() {
    // Spawn each in its own thread so they overlap (~5 s combined wall time
    // becomes ~3 s if they both take ~3 s).
    std::thread::spawn(|| {
        let mut g = session_for(SessionMode::Fast).lock_recover();
        if let Err(e) = g.ensure_alive() {
            eprintln!("[agent] fast session prewarm failed: {e}");
        }
    });
    std::thread::spawn(|| {
        let mut g = session_for(SessionMode::Power).lock_recover();
        if let Err(e) = g.ensure_alive() {
            eprintln!("[agent] power session prewarm failed: {e}");
        }
    });
}

/// Public entry point used by [`crate::pipeline`]. Routes between the Fast
/// and Power session based on keyword analysis of the prompt. On any error
/// (dead child, IO failure, malformed response) the chosen session is torn
/// down so the *next* call respawns from scratch — the failing call itself
/// still surfaces the error to the user.
pub fn run_claude_streaming<F>(prompt: &str, on_event: F) -> Result<String, String>
where
    F: FnMut(StreamEvent),
{
    let mode = if route_to_power(prompt) {
        SessionMode::Power
    } else {
        SessionMode::Fast
    };
    println!("[agent] routing to {mode:?} session");
    let s = session_for(mode);
    let mut guard = s.lock_recover();
    let result = guard.ask(prompt, on_event);
    if result.is_err() {
        guard.kill();
    }
    result
}

/// One-line label for a tool call, suitable for the HUD trace lane.
fn brief_tool_summary(name: &str, input: Option<&Value>) -> String {
    let detail = input.and_then(|i| match name {
        "Bash" => i.get("command").and_then(Value::as_str).map(truncate_50),
        "Read" | "Edit" | "Write" => i
            .get("file_path")
            .and_then(Value::as_str)
            .map(|s| s.rsplit('/').next().unwrap_or(s).to_string()),
        "WebFetch" => i.get("url").and_then(Value::as_str).map(truncate_50),
        _ => None,
    });
    match detail {
        Some(d) => format!("{name}: {d}"),
        None => name.to_string(),
    }
}

fn truncate_50(s: &str) -> String {
    if s.chars().count() <= 50 {
        s.to_string()
    } else {
        let cut: String = s.chars().take(47).collect();
        format!("{cut}…")
    }
}
