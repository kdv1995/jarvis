//! The orchestrator — Jarvis's state machine.
//!
//! A background "listener loop" pulls 16 kHz frames from the continuous mic
//! [`Listener`], gates them through the [`SpeechGate`] into discrete
//! utterances, and submits each utterance to the [`Engine`]. The engine
//! transcribes it, applies wake-word logic, and (when addressed) runs the
//! STT → `claude` → TTS flow on a worker thread.
//!
//! Modes:
//!   * `Idle` — waiting for a "Jarvis" / "Hey Jarvis" utterance.
//!   * `AwaitingCommand` — woke on the bare wake word; the next utterance is
//!     taken as a command with no wake word required (times out).
//!   * `Busy` — transcribing / thinking / speaking; incoming audio is ignored.

use std::collections::VecDeque;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::audio::Listener;
use crate::state::{AppState, LockExt};
use crate::vad::{rms, ClapDetector, SpeechGate};
use crate::{agent, tts};

/// How long we stay in `AwaitingCommand` before giving up and going idle.
/// Used for two cases now:
///   1. After a bare "Jarvis" wake (waiting for the actual command), and
///   2. **Multi-turn**: after Jarvis answers, the next utterance within this
///      window is taken as a follow-up command *without* requiring the wake
///      word again. 5 s feels conversational — long enough to gather your
///      thoughts for "and also…", short enough that ambient chatter doesn't
///      keep accidentally triggering Jarvis.
const AWAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of recent turns to remember for context injection.
/// 8 covers a typical mid-length conversation (~16 utterances of back-and-
/// forth) without bloating brain prompts. Excess entries are evicted FIFO.
const CONVERSATION_MAX_TURNS: usize = 8;

/// Maximum age of a remembered turn — past this, the turn is filtered out
/// of context even if still in the deque. 5 minutes is the boundary between
/// "natural conversation pause" (kept) and "new conversation" (forgotten).
const CONVERSATION_MAX_AGE: Duration = Duration::from_secs(300);

/// Frames of sustained loud speech required to confirm barge-in (~32 ms each;
/// 7 ≈ 224 ms — enough to ignore one-off bumps and TTS click artefacts).
const BARGE_IN_FRAMES: u32 = 7;
/// Frame RMS threshold for barge-in. Tuned high enough that the user's own
/// TTS leaking from the speakers into the mic doesn't trigger interruption.
const BARGE_IN_RMS: f32 = 0.04;
/// Grace period at the start of TTS playback where barge-in is suppressed —
/// the user gets ~half a second to hear what Jarvis said before being able to
/// interrupt (avoids tiny accidental cancels on the first word).
const BARGE_IN_GRACE_FRAMES: u32 = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    AwaitingCommand,
    Busy,
}

/// Why an utterance was submitted to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// From the continuous listener while idle — must contain the wake word.
    WakeCheck,
    /// From the manual button, or a follow-up while `AwaitingCommand` — treated
    /// as a command directly, no wake word required.
    DirectCommand,
}

enum WakeMatch {
    /// Wake word found, followed by a command.
    Command(String),
    /// Just the wake word, nothing after it.
    WakeOnly,
    /// No wake word — not addressed to Jarvis.
    None,
}

pub struct Engine {
    app: AppHandle,
    state: Arc<AppState>,
    mode: Mutex<Mode>,
    /// When the current `AwaitingCommand` started (for the timeout).
    awaiting_since: Mutex<Option<Instant>>,
    /// Live `afplay` (or `say`) child during TTS playback — `Some` while
    /// Jarvis is speaking. Used for barge-in: the listener loop takes the
    /// child out and `.kill()`s it when it detects the user speaking over
    /// Jarvis. `tts::say` polls this slot to know when it's been cancelled.
    pub tts_child: Arc<Mutex<Option<Child>>>,
    /// Rolling history of recent conversation turns. Capped at
    /// [`CONVERSATION_MAX_TURNS`] entries and trimmed to entries within
    /// [`CONVERSATION_MAX_AGE`] when context is built. Used to inject
    /// recent context into EVERY brain prompt — fast-path turns, follow-up
    /// turns, AND fresh wake-word turns — so context survives across the
    /// 5-second AwaitingCommand window expiring, across switches between
    /// the Fast/Power claude sessions, and across fast-path commands that
    /// never touch the brain (so when brain IS invoked, it knows what the
    /// user has been doing in the meantime).
    conversation_history: Mutex<VecDeque<ConversationTurn>>,
    /// `true` if the user barged in on the last TTS — flips the framing of
    /// the next prompt's context preamble. Consumed (swap-to-false) on read.
    last_was_interrupted: std::sync::atomic::AtomicBool,
}

/// One completed user↔Jarvis exchange, stored in the rolling history.
///
/// `when` is [`SystemTime`] (not [`Instant`]) because we persist turns to
/// `~/.jarvis/journal.jsonl` between runs and need a wall-clock timestamp.
/// `.elapsed()` on a SystemTime can fail if the clock went backwards; we
/// treat that as "no time has passed" — benign for the 5-minute age window.
#[derive(Clone)]
struct ConversationTurn {
    user: String,
    jarvis: String,
    when: SystemTime,
}

impl Engine {
    fn mode(&self) -> Mode {
        *self.mode.lock_recover()
    }

    /// True while a TTS child process is currently registered (Jarvis is
    /// actively speaking). Used by the listener loop to gate barge-in.
    fn is_tts_playing(&self) -> bool {
        self.tts_child.lock_recover().is_some()
    }

    /// Kill the active TTS playback (if any) and reap the child. Safe to call
    /// when nothing is playing — it's a no-op. Also flags that the *next*
    /// command was preceded by an interruption, so the brain can frame its
    /// response accordingly ("got it, switching to…" rather than starting
    /// fresh).
    fn cancel_tts(&self) {
        if let Some(mut child) = self.tts_child.lock_recover().take() {
            let _ = child.kill();
            let _ = child.wait(); // avoid zombies
            self.last_was_interrupted
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock_recover() = mode;
        match mode {
            Mode::Idle => {
                *self.awaiting_since.lock_recover() = None;
                self.emit_state("idle");
            }
            Mode::AwaitingCommand => {
                *self.awaiting_since.lock_recover() = Some(Instant::now());
                self.emit_state("listening");
            }
            Mode::Busy => {}
        }
    }

    /// Submit an utterance for processing. No-op if the engine is already busy.
    pub fn submit(self: &Arc<Self>, samples: Vec<f32>, trigger: Trigger) {
        {
            let mut mode = self.mode.lock_recover();
            if *mode == Mode::Busy {
                return; // already working — drop this utterance
            }
            *mode = Mode::Busy;
        }
        let engine = Arc::clone(self);
        std::thread::spawn(move || engine.process(samples, trigger));
    }

    /// Worker-thread body: transcribe, apply wake logic, run the command.
    fn process(self: Arc<Self>, samples: Vec<f32>, trigger: Trigger) {
        println!(
            "[pipeline] process() started — {} samples, trigger={:?}",
            samples.len(),
            trigger
        );
        // A direct command is known work — light up "thinking" right away. A
        // wake-check might just be ambient noise, so stay quiet until the wake
        // word actually matches (no "thinking" flash at every passing sound).
        if trigger == Trigger::DirectCommand {
            self.emit_state("thinking");
        }

        let t_stt = Instant::now();
        let transcript = match self.state.elevenlabs.as_ref() {
            Some(cfg) => crate::stt::transcribe(&samples, cfg),
            None => Err("ElevenLabs not configured — STT unavailable".into()),
        };
        let transcript = match transcript {
            Ok(t) => {
                println!(
                    "[pipeline] STT took {:?} — transcript: {:?}",
                    t_stt.elapsed(),
                    t
                );
                t
            }
            Err(e) => {
                println!("[pipeline] STT error: {}", e);
                return self.fail(&e);
            }
        };

        // Decide what (if anything) the command is.
        let command = match trigger {
            Trigger::DirectCommand => {
                if transcript.is_empty() {
                    return self.fail("I didn't catch that.");
                }
                transcript
            }
            Trigger::WakeCheck => match match_wake_word(&transcript) {
                WakeMatch::Command(cmd) => {
                    // Wake word confirmed — now we're genuinely working.
                    self.emit_state("thinking");
                    cmd
                }
                WakeMatch::WakeOnly => {
                    // Woke, but no command yet — give an audible ack and
                    // open the AwaitingCommand window for the follow-up.
                    // Same UX as the clap path so both wake gestures feel
                    // identical from the user's perspective.
                    self.acknowledge_wake();
                    return;
                }
                WakeMatch::None => {
                    // Not addressed to Jarvis — silently return to idle.
                    self.set_mode(Mode::Idle);
                    return;
                }
            },
        };

        println!("[pipeline] wake matched — command: {:?}", command);
        let _ = self
            .app
            .emit("hud://transcript", json!({ "text": command }));

        // --- Dictation passthrough: "claude <prompt>" → Terminal.app session
        // If the command starts with the "claude" keyword, type the rest into
        // the user's already-running Claude Code session in Terminal.app
        // instead of routing to Jarvis's own brain. The terminal's Claude
        // process keeps its own conversation context, so multi-turn dictation
        // just works — Jarvis is purely a voice→keystroke pipe here.
        //
        // We DELIBERATELY do not call `remember_turn` for dictation: nothing
        // came back from Jarvis's brain, so the brain's recent-conversation
        // log shouldn't be polluted with terminal prompts.
        if let Some(prompt) = strip_dictation_prefix(&command) {
            if prompt.is_empty() {
                return self.fail("Dictation prompt was empty.");
            }
            println!("[pipeline] DICTATE → Terminal: {:?}", prompt);
            match dictate_to_terminal(&prompt) {
                Ok(()) => {
                    let _ = self
                        .app
                        .emit("hud://caption", json!({ "text": format!("→ Terminal: {prompt}"), "final": true }));
                    play_chime("Pop");
                    // Keep the 5-second follow-up window open so the next
                    // "claude, ..." utterance lands without re-saying "jarvis".
                    self.set_mode(Mode::AwaitingCommand);
                    self.emit_state("listening");
                    return;
                }
                Err(e) => return self.fail(&format!("iTerm2 dictation failed: {e}")),
            }
        }

        // --- Conversational continuity ---------------------------------------
        // If this utterance is a follow-up (came in via the 5s AwaitingCommand
        // window after the last reply), build a context preamble so the brain
        // can interpret things like "actually Illustrator" or "no, in NYC".
        // Empty string when not a follow-up — preserves the existing prompt
        // exactly for first-turn commands.
        let follow_up_context = self.build_follow_up_context(trigger);

        // --- Morning routine: gather calendar context and ask the brain to ---
        // compose a natural briefing. Not a fast-path return because the
        // response is generative ("Good morning. You have a meeting at 10."),
        // but the *gather* phase is local AppleScript (~200 ms) so the brain
        // only has to do the wording, not the data fetch.
        if is_morning_greeting(&command) {
            println!("[pipeline] morning routine → fetching calendar context");
            let context = morning_context();
            let prompt = format!(
                "{follow_up_context}The user just said '{command}'. Greet them and brief \
                 them in one or two short sentences spoken aloud, using ONLY the context \
                 below. Do not invent events. Lead with the greeting.\n\
                 \n\
                 CONTEXT:\n{context}"
            );
            self.emit_state("thinking");
            let answer = self.run_streaming(&prompt);
            match answer {
                Ok(a) if !a.is_empty() => {
                    let _ = self
                        .app
                        .emit("hud://caption", json!({ "text": &a, "final": true }));
                    self.remember_turn(&command, &a);
                    self.set_mode(Mode::AwaitingCommand);
                    return;
                }
                Ok(_) => return self.fail("Empty briefing — try again."),
                Err(e) => return self.fail(&e),
            }
        }

        // --- System telemetry fast-path -------------------------------------
        // "what's my battery", "cpu usage", "memory", "wifi", "uptime" —
        // answered straight from the snapshot the HUD already polls. ~20ms.
        if let Some(answer) = crate::sysinfo::try_answer_system_query(&command) {
            println!("[pipeline] SYS-INFO FAST PATH — {}", answer);
            self.emit_state("speaking");
            let _ = self
                .app
                .emit("hud://caption", json!({ "text": &answer, "final": false }));
            if let Err(e) = tts::speak_sentence(&answer, &self.app, &self.tts_child) {
                eprintln!("[jarvis] sys-info tts error: {e}");
            }
            let _ = self
                .app
                .emit("hud://caption", json!({ "text": &answer, "final": true }));
            self.remember_turn(&command, &answer);
            self.set_mode(Mode::AwaitingCommand);
            return;
        }

        // --- Fast path: skip the brain entirely for the 80% case -------------
        // Common voice verbs ("open X", "close X", "what time") don't need a
        // 200B-parameter model to figure out. Pattern-match in Rust, run the
        // AppleScript / shell directly, speak a brief confirmation via Kokoro.
        // End-to-end ~1.5s instead of ~7s (the claude CLI cold-start alone
        // costs ~3s). Falls through to the regular brain routing for anything
        // not in the table.
        let t_fast = Instant::now();
        if let Some(answer) = try_fast_action(&command) {
            println!("[pipeline] FAST PATH ({:?}) — {}", t_fast.elapsed(), answer);
            self.emit_state("speaking");
            let _ = self
                .app
                .emit("hud://caption", json!({ "text": &answer, "final": false }));
            if let Err(e) = tts::speak_sentence(&answer, &self.app, &self.tts_child) {
                eprintln!("[jarvis] fast-path tts error: {e}");
            }
            let _ = self
                .app
                .emit("hud://caption", json!({ "text": &answer, "final": true }));
            // Remember this turn for follow-up context.
            self.remember_turn(&command, &answer);
            // Stay hot for ~5s so a follow-up doesn't need the wake word.
            self.set_mode(Mode::AwaitingCommand);
            return;
        }

        // --- Brain + voice ---------------------------------------------------
        // Two routes, both end up speaking the answer:
        //   • CLI route (streaming): claude emits each assistant text block as
        //     soon as it's complete, and we hand it to Kokoro immediately —
        //     first word out the door in ~1s instead of waiting ~5s for the
        //     full result. Tool calls light up the HUD trace lane live.
        //   • Local route (non-streaming): Ollama returns the whole answer at
        //     once, so we just speak it as one chunk.
        // Local→CLI fallback if the local model is unavailable.
        let t_brain = Instant::now();
        // Prepend follow-up context (empty string when this is a fresh turn).
        let enriched = if follow_up_context.is_empty() {
            command.clone()
        } else {
            format!("{follow_up_context}The user now says: {command}")
        };
        let answer = if needs_cli(&command) {
            println!("[pipeline] routing to claude CLI (streaming)…");
            self.run_streaming(&enriched)
        } else {
            println!("[pipeline] routing to local model…");
            match agent::run_local(&enriched) {
                Ok(a) => {
                    // Local model gave a full answer — speak it once.
                    self.emit_state("speaking");
                    let _ = self
                        .app
                        .emit("hud://caption", json!({ "text": &a, "final": false }));
                    if let Err(e) = tts::speak_sentence(&a, &self.app, &self.tts_child) {
                        eprintln!("[jarvis] tts error: {e}");
                    }
                    Ok(a)
                }
                Err(e) => {
                    println!(
                        "[pipeline] local model unavailable ({e}); falling back to claude CLI"
                    );
                    self.run_streaming(&enriched)
                }
            }
        };

        let answer = match answer {
            Ok(a) if !a.is_empty() => {
                println!(
                    "[pipeline] brain+voice took {:?} — answered ({} chars)",
                    t_brain.elapsed(),
                    a.len()
                );
                a
            }
            Ok(_) => return self.fail("The assistant returned an empty response."),
            Err(e) => return self.fail(&e),
        };

        // Finalize the caption (HUD can drop the live-typing cursor).
        let _ = self
            .app
            .emit("hud://caption", json!({ "text": &answer, "final": true }));
        // Remember this turn so the next utterance — if it arrives in the
        // AwaitingCommand window — can be interpreted in context.
        self.remember_turn(&command, &answer);
        // Stay hot for ~5s so a follow-up doesn't need the wake word.
        self.set_mode(Mode::AwaitingCommand);
    }

    /// Run the claude CLI in streaming mode. Each assistant text block is
    /// spoken via Kokoro the moment it arrives (so the first word comes out
    /// in ~1 s instead of waiting for the full response). Tool calls and
    /// their results are mirrored to the HUD `hud://trace` lane in real time.
    /// Returns the final assembled answer text once the CLI exits.
    fn run_streaming(&self, prompt: &str) -> Result<String, String> {
        use crate::agent::StreamEvent;
        use std::cell::Cell;

        let app = self.app.clone();
        let tts_child = Arc::clone(&self.tts_child);
        // Whether we've already emitted the "speaking" state transition. We
        // delay it until the *first* text block lands so the HUD shows
        // "thinking" while claude is still in tool-use phase.
        let speaking_emitted = Cell::new(false);

        let final_text = agent::run_claude_streaming(prompt, |event| match event {
            StreamEvent::Text(text) => {
                if !speaking_emitted.get() {
                    speaking_emitted.set(true);
                    let _ = app.emit("hud://state", json!({ "state": "speaking" }));
                }
                let _ = app.emit("hud://caption", json!({ "text": &text, "final": false }));
                if let Err(e) = tts::speak_sentence(&text, &app, &tts_child) {
                    eprintln!("[jarvis] tts error mid-stream: {e}");
                }
            }
            StreamEvent::ToolUse { summary, .. } => {
                println!("[pipeline] tool → {summary}");
                let _ = app.emit(
                    "hud://trace",
                    json!({ "action": format!("→ {summary}"), "kind": "use" }),
                );
            }
            StreamEvent::ToolResult { ok } => {
                let _ = app.emit(
                    "hud://trace",
                    json!({
                        "action": if ok { "✓" } else { "✗" },
                        "kind": if ok { "ok" } else { "error" }
                    }),
                );
            }
        })?;

        // Rare edge case: stream ended without any text block (e.g. claude ran
        // tools but emitted no user-facing answer). Still speak the terminal
        // `result` text so the user isn't left in silence.
        if !speaking_emitted.get() && !final_text.trim().is_empty() {
            self.emit_state("speaking");
            let _ = self.app.emit(
                "hud://caption",
                json!({ "text": &final_text, "final": false }),
            );
            if let Err(e) = tts::speak_sentence(&final_text, &self.app, &self.tts_child) {
                eprintln!("[jarvis] tts error (final fallback): {e}");
            }
        }

        Ok(final_text)
    }

    fn fail(&self, message: &str) {
        eprintln!("[jarvis] pipeline error: {message}");
        let _ = self.app.emit("hud://error", json!({ "message": message }));
        self.set_mode(Mode::Idle);
    }

    fn emit_state(&self, state: &str) {
        let _ = self.app.emit("hud://state", json!({ "state": state }));
    }

    /// Spoken acknowledgement when Jarvis is woken without a command payload —
    /// either by a clap or by the user saying just "Jarvis" alone.
    ///
    /// Flow: Busy + speaking-state → brief TTS ack (default: "Yes?") → drops
    /// into AwaitingCommand with a *fresh* 5-second timer. The user always
    /// gets the full window after hearing the ack — none of it is eaten by
    /// the ack itself, because we transition to AwaitingCommand only AFTER
    /// TTS playback finishes (`speak_sentence` blocks).
    ///
    /// The ack phrase comes from `JARVIS_WAKE_ACK` env var, defaulting to
    /// "Yes?". Set to an empty string to disable the audible ack entirely
    /// (state will still transition; HUD will still flash listening).
    pub fn acknowledge_wake(&self) {
        let phrase = std::env::var("JARVIS_WAKE_ACK")
            .ok()
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| "Yes?".to_string());
        self.speak_wake_phrase(&phrase);
    }

    /// Spoken greeting for the first launch caused by the background clap
    /// daemon. Unlike the short in-app wake ack, this confirms Jarvis has fully
    /// started and is ready for the next utterance.
    pub fn greet_launch_wake(&self) {
        let phrase = std::env::var("JARVIS_LAUNCH_WAKE_GREETING")
            .ok()
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| "Jarvis online.".to_string());
        self.speak_wake_phrase(&phrase);
    }

    fn speak_wake_phrase(&self, phrase: &str) {
        if !phrase.is_empty() {
            // Mark Busy + speaking so the listener_loop's TTS-aware paths
            // (mic pause, barge-in suppression) kick in cleanly during the
            // ack. Without this, the mic would stay open and the ack itself
            // could be transcribed as an "utterance".
            self.set_mode(Mode::Busy);
            self.emit_state("speaking");
            if let Err(e) = tts::speak_sentence(phrase, &self.app, &self.tts_child) {
                eprintln!("[jarvis] wake-ack TTS failed: {e}");
            }
        }
        // Now open the actual command window. AwaitingCommand sets a fresh
        // `awaiting_since = now()` so the 5-second timeout starts here.
        self.set_mode(Mode::AwaitingCommand);
        self.emit_state("listening");
    }

    /// Build the recent-conversation preamble for a brain prompt. Pulls
    /// every turn from the rolling history that's still within
    /// [`CONVERSATION_MAX_AGE`] (5 min default) and formats them as a
    /// prior-conversation block.
    ///
    /// **Crucially, this is NOT gated on the trigger type** — it fires for
    /// both fresh wake-word commands AND in-window follow-ups. So you can
    /// say *"Jarvis, weather in NYC"* → wait 30 seconds → *"Jarvis, what
    /// about San Francisco"* and the brain still has the NYC context. The
    /// 5-second AwaitingCommand window is just for "no wake word needed";
    /// context persistence is decoupled and lasts much longer.
    ///
    /// If `last_was_interrupted` is set, the preamble adds a flag so the
    /// brain knows the user cut off the previous TTS mid-sentence.
    fn build_follow_up_context(&self, _trigger: Trigger) -> String {
        let was_interrupted = self
            .last_was_interrupted
            .swap(false, std::sync::atomic::Ordering::Relaxed);

        let recent: Vec<ConversationTurn> = {
            let history = self.conversation_history.lock_recover();
            history
                .iter()
                .filter(|t| t.when.elapsed().unwrap_or(Duration::ZERO) < CONVERSATION_MAX_AGE)
                .cloned()
                .collect()
        };

        if recent.is_empty() && !was_interrupted {
            return String::new();
        }

        let mut out = String::new();
        if was_interrupted {
            out.push_str(
                "NOTE: The user just interrupted you mid-sentence — treat the next \
                 message as a correction or redirect, not a fresh question. No apology.\n\n",
            );
        }
        if !recent.is_empty() {
            out.push_str("RECENT CONVERSATION (oldest → newest):\n");
            for turn in &recent {
                // Trim Jarvis's answer to a single line for context — full
                // replies can be long and we just need the gist for the
                // brain to follow continuity.
                let jarvis_brief = turn
                    .jarvis
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>();
                out.push_str(&format!(
                    "  User: \"{}\"\n  You:  \"{}\"\n",
                    turn.user.trim(),
                    jarvis_brief
                ));
            }
            out.push('\n');
        }
        out
    }

    /// Record a completed user↔Jarvis exchange into the rolling history.
    /// Trims oldest entries beyond [`CONVERSATION_MAX_TURNS`] so memory
    /// usage is bounded regardless of conversation length. Called from
    /// every successful exit path (fast-path, brain, morning routine).
    ///
    /// Also appends the turn to `~/.jarvis/journal.jsonl` so it survives
    /// app restarts and supplies continuity across days. The journal write
    /// is best-effort — failure logs to stderr but doesn't disturb the
    /// voice flow.
    fn remember_turn(&self, prompt: &str, answer: &str) {
        let now = SystemTime::now();
        let mut history = self.conversation_history.lock_recover();
        history.push_back(ConversationTurn {
            user: prompt.to_string(),
            jarvis: answer.to_string(),
            when: now,
        });
        while history.len() > CONVERSATION_MAX_TURNS {
            history.pop_front();
        }
        drop(history); // release before disk IO
        if let Err(e) = append_to_journal(prompt, answer, now) {
            eprintln!("[pipeline] journal append failed: {e}");
        }
        // Clear the interrupted flag now that a new turn has completed
        // successfully. The next interruption will set it again.
        self.last_was_interrupted
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Start the engine and its background listener loop. Hands-free capture is
/// best-effort: if the mic can't be opened, the engine is still returned so the
/// manual button keeps working.
pub fn start(app: AppHandle, state: Arc<AppState>) -> Arc<Engine> {
    // Eagerly spawn the persistent claude session in the background so the
    // FIRST voice command doesn't pay the ~3 s cold-start tax. Cheap and
    // failure-safe: if it fails the first real `ask()` will retry.
    std::thread::spawn(agent::prewarm);

    // Hydrate conversation memory from disk. Recent turns from prior runs
    // give Jarvis continuity across app restarts — if you said "Jarvis,
    // weather" 3 minutes ago, then restarted Jarvis, then asked "what about
    // Berlin", the brain still has the original question in its context.
    // Older turns past CONVERSATION_MAX_AGE remain in the deque but are
    // filtered out by `build_follow_up_context` — they're inert ballast,
    // dropped on the next eviction.
    let mut hydrated: VecDeque<ConversationTurn> =
        VecDeque::with_capacity(CONVERSATION_MAX_TURNS + 1);
    match load_recent_turns(CONVERSATION_MAX_TURNS) {
        Ok(turns) => {
            let n = turns.len();
            hydrated.extend(turns);
            if n > 0 {
                println!("[pipeline] hydrated {n} turn(s) from journal");
            }
        }
        Err(e) => eprintln!("[pipeline] journal hydrate failed: {e}"),
    }

    let engine = Arc::new(Engine {
        app,
        state,
        mode: Mutex::new(Mode::Idle),
        awaiting_since: Mutex::new(None),
        tts_child: Arc::new(Mutex::new(None)),
        conversation_history: Mutex::new(hydrated),
        last_was_interrupted: std::sync::atomic::AtomicBool::new(false),
    });

    let loop_engine = Arc::clone(&engine);
    std::thread::spawn(move || listener_loop(loop_engine));

    engine
}

/// Pulls frames from the continuous listener, gates them into utterances, and
/// submits them. Runs for the lifetime of the app.
fn listener_loop(engine: Arc<Engine>) {
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<f32>>();

    let listener = match Listener::start(frame_tx) {
        Ok(l) => {
            println!("[jarvis] hands-free listening active");
            l
        }
        Err(e) => {
            eprintln!("[jarvis] hands-free disabled ({e}); manual button still works");
            return;
        }
    };

    // Mic-during-TTS policy. Opt out with JARVIS_MIC_PAUSE_DURING_TTS=false to
    // restore the legacy barge-in-capable behaviour (mic always on, frames
    // filtered in software). Default: actually pause the cpal stream when
    // Jarvis is speaking — driver-level off, mic LED dark on hardware that
    // has one. The cost: barge-in (interrupting Jarvis by speaking over him)
    // stops working since no frames are captured during TTS. Use the
    // double-clap wake or wait for Jarvis to finish.
    let pause_during_tts = std::env::var("JARVIS_MIC_PAUSE_DURING_TTS")
        .map(|v| v.trim().to_lowercase() != "false")
        .unwrap_or(true);
    println!(
        "[jarvis] mic-pause-during-TTS: {} (set JARVIS_MIC_PAUSE_DURING_TTS=false to disable)",
        if pause_during_tts {
            "ON"
        } else {
            "off — barge-in available"
        }
    );
    let mut mic_paused = false;

    let mut gate = match SpeechGate::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[jarvis] VAD init failed ({e}); manual button still works");
            return;
        }
    };
    let mut clap = ClapDetector::new();

    // Tracks the gate's speech state across frames so "listening" is emitted
    // once, on the onset of speech — not once per frame.
    let mut was_speaking = false;

    // Barge-in tracking. Active only while the engine is `Busy` AND `tts_child`
    // is `Some` (i.e. Jarvis is actually speaking). `tts_grace_frames` counts
    // up at the start of each TTS, blocking barge-in for the first ~500 ms;
    // `barge_speech_frames` then counts consecutive loud-mic frames toward the
    // `BARGE_IN_FRAMES` threshold that triggers cancellation.
    let mut was_tts_playing = false;
    let mut tts_grace_frames: u32 = 0;
    let mut barge_speech_frames: u32 = 0;

    loop {
        // Driver-level mic toggle based on TTS state. Done OUTSIDE the frame
        // receive so transitions happen promptly even when the channel is
        // empty (which it will be while paused — no frames arriving).
        if pause_during_tts {
            let tts_now = engine.is_tts_playing();
            if tts_now && !mic_paused {
                listener.pause();
                mic_paused = true;
            } else if !tts_now && mic_paused {
                listener.resume();
                mic_paused = false;
            }
        }

        match frame_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(frame) => {
                // While Busy we still watch the mic — but only for barge-in,
                // not for full VAD-driven utterance capture. If Jarvis is
                // actively speaking and we hear sustained loud speech, kill
                // the TTS so the user can interrupt. Thinking-state Busy
                // (no TTS child yet) is left alone; killing the LLM mid-call
                // risks half-executed shell commands.
                if engine.mode() == Mode::Busy {
                    let tts_playing = engine.is_tts_playing();
                    if tts_playing && !was_tts_playing {
                        // TTS just started — start the grace window.
                        tts_grace_frames = 0;
                        barge_speech_frames = 0;
                    }
                    was_tts_playing = tts_playing;

                    if tts_playing {
                        if tts_grace_frames < BARGE_IN_GRACE_FRAMES {
                            tts_grace_frames += 1;
                        } else if rms(&frame) > BARGE_IN_RMS {
                            barge_speech_frames += 1;
                            if barge_speech_frames >= BARGE_IN_FRAMES {
                                println!("[barge-in] interrupting TTS");
                                engine.cancel_tts();
                                barge_speech_frames = 0;
                                // process() returns from tts::say with Err,
                                // sets Idle — next frame falls into normal
                                // VAD flow and captures the interrupt.
                            }
                        } else {
                            barge_speech_frames = 0;
                        }
                    }

                    gate.reset();
                    clap.reset();
                    was_speaking = false;
                    continue;
                }

                // Not Busy — clear barge state for the next TTS round.
                was_tts_playing = false;
                tts_grace_frames = 0;
                barge_speech_frames = 0;

                // A double hand-clap wakes Jarvis, exactly like the bare wake
                // word — it drops the engine into AwaitingCommand so the next
                // utterance is taken as a command. Also fires the audible
                // wake-ack ("Yes?") so the user hears confirmation he's
                // listening before they start speaking. The clap frame
                // itself is dropped (reset + continue) so it never becomes
                // "speech".
                if clap.push_frame(&frame) {
                    if engine.mode() == Mode::Idle {
                        println!("[clap] double-clap detected — waking");
                        // Worker thread: the ack TTS blocks for ~500 ms;
                        // running it on the listener thread would stall
                        // frame intake. Spawn it off and continue draining
                        // the mic queue. set_mode(Busy) inside the worker
                        // ensures subsequent frames see Busy state quickly.
                        let engine_ack = Arc::clone(&engine);
                        std::thread::spawn(move || {
                            engine_ack.acknowledge_wake();
                        });
                    }
                    gate.reset();
                    was_speaking = false;
                    continue;
                }

                let utterance = gate.push_frame(&frame);
                let speaking = gate.is_speaking();

                // Speech just started → light the HUD up as "listening" so the
                // user gets immediate feedback that Jarvis hears them.
                if speaking && !was_speaking {
                    engine.emit_state("listening");
                }
                // Speech ended but produced no utterance (too short / noise) →
                // drop back to the resting state so the HUD doesn't stay lit.
                if was_speaking && !speaking && utterance.is_none() {
                    match engine.mode() {
                        Mode::AwaitingCommand => engine.emit_state("listening"),
                        _ => engine.emit_state("idle"),
                    }
                }
                was_speaking = speaking;

                if let Some(utterance) = utterance {
                    let trigger = match engine.mode() {
                        Mode::AwaitingCommand => Trigger::DirectCommand,
                        _ => Trigger::WakeCheck,
                    };
                    engine.submit(utterance, trigger);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Expire a stale AwaitingCommand.
                if engine.mode() == Mode::AwaitingCommand {
                    let expired = engine
                        .awaiting_since
                        .lock_recover()
                        .map(|t| t.elapsed() > AWAIT_TIMEOUT)
                        .unwrap_or(true);
                    if expired {
                        gate.reset();
                        was_speaking = false;
                        engine.set_mode(Mode::Idle);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("[jarvis] listener channel closed; hands-free stopped");
                break;
            }
        }
    }
}

/// Decide whether a command needs the full `claude` CLI — which can run shell
/// commands, edit files, and read live system state — or whether the fast
/// local model can answer it from knowledge alone.
///
/// Deterministic by design: routing is plain code, never an LLM call. Biased
/// toward the CLI — a question wrongly sent to the CLI is merely slower, but an
/// action wrongly sent to the knowledge-only local model simply can't be done.
fn needs_cli(command: &str) -> bool {
    let lower = command.to_lowercase();

    // Multi-word phrases that imply action or live state.
    const PHRASES: &[&str] = &[
        "what's playing",
        "my screen",
        "right now",
        "turn on",
        "turn off",
        "search for",
        "look up",
    ];
    if PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Single words that imply *doing* something, or needing live system /
    // world state the local model can't know. Matched whole-word so e.g.
    // "running" doesn't trip "run".
    const WORDS: &[&str] = &[
        // action verbs
        "open",
        "close",
        "launch",
        "quit",
        "start",
        "stop",
        "run",
        "execute",
        "create",
        "delete",
        "remove",
        "move",
        "rename",
        "play",
        "pause",
        "skip",
        "install",
        "build",
        "commit",
        "push",
        "enable",
        "disable",
        "screenshot",
        "send",
        "email",
        "set",
        // delegate-to-another-AI verbs and target apps (need Bash + AppleScript)
        "ask",
        "tell",
        "paste",
        "type",
        "spawn",
        "delegate",
        "claude",
        "chatgpt",
        "codex",
        "openai",
        "terminal",
        "iterm",
        // live state the model can't know from training data
        "time",
        "date",
        "today",
        "weather",
        "battery",
        "currently",
    ];
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| WORDS.contains(&w))
}

// ---------------------------------------------------------------------------
// Fast-path action router. Bypasses both Ollama and claude CLI for the most
// common voice verbs — pattern-match, run AppleScript / shell, return a
// canned response. End-to-end latency drops from ~7s (claude CLI cold-start)
// to ~1.5s (just STT + TTS).
//
// Scope is deliberately small — only handle the cases where the right action
// is *unambiguous*. Anything ambiguous (or any app not in the table) falls
// through to the claude CLI brain.
// ---------------------------------------------------------------------------

/// Try to handle `command` with a fast Rust+AppleScript path. Returns
/// `Some(spoken_response)` on a hit, `None` to fall through to the brain.
fn try_fast_action(command: &str) -> Option<String> {
    let cleaned = command
        .to_lowercase()
        .trim()
        .trim_end_matches(['.', '?', '!'])
        .to_string();
    let cleaned = cleaned.trim();

    // "open X" / "launch X" / "start X" / "fire up X"
    for verb in &["open up ", "open ", "launch ", "start ", "fire up "] {
        if let Some(rest) = cleaned.strip_prefix(verb) {
            return fast_open(rest.trim().trim_start_matches("the "));
        }
    }

    // "ask claude <prompt>" / "tell claude <prompt>" / "ask codex <prompt>" /
    // "tell codex <prompt>" — open a fresh chat in the target desktop app and
    // type the prompt for the user. Skips the brain entirely (~3 s saved).
    //
    // Case-preserving: we match on the lowercased `cleaned` string, but slice
    // the original `command` to extract the prompt so Claude gets it with
    // proper capitalization ("React" not "react", proper names, etc.).
    const ASK_VERBS: &[(&str, &str)] = &[
        ("ask claude to ", "Claude"),
        ("tell claude to ", "Claude"),
        ("ask claude ", "Claude"),
        ("tell claude ", "Claude"),
        ("ask codex to ", "Codex"),
        ("tell codex to ", "Codex"),
        ("ask codex ", "Codex"),
        ("tell codex ", "Codex"),
    ];
    for (verb, app) in ASK_VERBS {
        if cleaned.starts_with(verb) {
            // Slice from the ORIGINAL command (preserves case). The trimmed
            // command may be shorter than `command` by leading whitespace —
            // use the trim-aware index.
            let orig_trimmed = command.trim();
            let orig_lower = orig_trimmed.to_lowercase();
            if orig_lower.starts_with(verb) {
                let prompt_text = orig_trimmed[verb.len()..].trim();
                let prompt_text = prompt_text.trim_end_matches(['.', '?', '!']).trim();
                if !prompt_text.is_empty() {
                    return fast_send_to_app(app, prompt_text);
                }
            }
        }
    }

    // "close X" / "quit X"
    for verb in &["close ", "quit "] {
        if let Some(rest) = cleaned.strip_prefix(verb) {
            return fast_close(rest.trim().trim_start_matches("the "));
        }
    }

    // Web search — "search for X" / "google X" / "search youtube for X".
    if let Some(q) = cleaned
        .strip_prefix("search youtube for ")
        .or_else(|| cleaned.strip_prefix("youtube "))
    {
        return fast_url_search(
            "https://www.youtube.com/results?search_query=",
            q,
            "YouTube",
        );
    }
    for verb in &["search for ", "search ", "google ", "look up "] {
        if let Some(q) = cleaned.strip_prefix(verb) {
            return fast_url_search("https://www.google.com/search?q=", q, "Google");
        }
    }

    // Volume controls.
    if matches!(
        cleaned,
        "volume up" | "louder" | "turn it up" | "turn up the volume"
    ) {
        return fast_volume_delta(10);
    }
    if matches!(
        cleaned,
        "volume down" | "quieter" | "turn it down" | "turn down the volume"
    ) {
        return fast_volume_delta(-10);
    }
    if matches!(cleaned, "mute" | "mute the volume" | "be quiet" | "silence") {
        return fast_volume_mute(true);
    }
    if matches!(cleaned, "unmute" | "unmute the volume") {
        return fast_volume_mute(false);
    }

    // Media controls (Spotify-first, falls through to claude if Spotify isn't
    // installed). Apple Music speakers can say "play music in apple music".
    if matches!(cleaned, "play" | "resume" | "play music" | "play the music") {
        return fast_spotify("playpause", "Playing.");
    }
    if matches!(cleaned, "pause" | "pause the music" | "stop the music") {
        return fast_spotify("playpause", "Paused.");
    }
    if matches!(
        cleaned,
        "next" | "next song" | "next track" | "skip" | "skip song" | "skip this song"
    ) {
        return fast_spotify("next track", "Skipping ahead.");
    }
    if matches!(
        cleaned,
        "previous" | "previous song" | "previous track" | "go back" | "last song"
    ) {
        return fast_spotify("previous track", "Going back.");
    }

    // Lock / sleep.
    if matches!(
        cleaned,
        "lock the screen" | "lock screen" | "lock my mac" | "lock the mac" | "lock"
    ) {
        return fast_lock_screen();
    }
    if matches!(
        cleaned,
        "go to sleep" | "sleep" | "sleep my mac" | "sleep the mac" | "good night"
    ) {
        return fast_sleep();
    }

    // Time / date — pure local, no AppleScript.
    if matches!(
        cleaned,
        "what time is it"
            | "what's the time"
            | "what is the time"
            | "tell me the time"
            | "current time"
            | "the time"
    ) {
        return Some(fast_time());
    }
    if matches!(
        cleaned,
        "what date is it"
            | "what is the date"
            | "what's the date"
            | "what's today's date"
            | "what is today's date"
            | "today's date"
            | "what day is it"
            | "what's today"
    ) {
        return Some(fast_date());
    }

    // Screenshot — saves to ~/Desktop.
    if matches!(
        cleaned,
        "take a screenshot"
            | "screenshot"
            | "capture the screen"
            | "screen shot"
            | "take a screen shot"
    ) {
        return fast_screenshot();
    }

    // ── File operations (Pack 1) ────────────────────────────────────────
    // "find <name>" / "find file <name>" / "find document <name>" — Spotlight
    // mdfind, speak top hit (or count of hits).
    for verb in &["find file ", "find document ", "find "] {
        if let Some(name) = cleaned.strip_prefix(verb) {
            return fast_find_file(name.trim());
        }
    }
    // "open file <name>" / "open document <name>" — find via mdfind, `open` it.
    for verb in &["open file ", "open document ", "open the file "] {
        if let Some(name) = cleaned.strip_prefix(verb) {
            return fast_open_file(name.trim());
        }
    }
    // "create folder <name>" / "make folder <name>" / "new folder <name>" —
    // creates in ~/Desktop unless "in downloads" / "in documents" suffix.
    for verb in &[
        "create folder ",
        "create a folder ",
        "make folder ",
        "make a folder ",
        "new folder ",
        "new folder called ",
        "create a new folder ",
    ] {
        if let Some(rest) = cleaned.strip_prefix(verb) {
            return fast_create_folder(rest.trim());
        }
    }
    // "move <name> to <destination>" — destinations: downloads, desktop,
    // documents, trash. Source resolved via mdfind.
    if let Some(rest) = cleaned.strip_prefix("move ") {
        if let Some((name, dest)) = rest.rsplit_once(" to ") {
            return fast_move_file(name.trim(), dest.trim());
        }
    }

    // ── Window management (Pack 2) ──────────────────────────────────────
    // Snap the frontmost window to half of the screen.
    if matches!(
        cleaned,
        "snap left" | "snap to left" | "snap window left" | "window left" | "left half"
    ) {
        return fast_snap_window(SnapDir::Left);
    }
    if matches!(
        cleaned,
        "snap right" | "snap to right" | "snap window right" | "window right" | "right half"
    ) {
        return fast_snap_window(SnapDir::Right);
    }
    if matches!(
        cleaned,
        "snap top" | "snap to top" | "top half"
    ) {
        return fast_snap_window(SnapDir::Top);
    }
    if matches!(
        cleaned,
        "snap bottom" | "snap to bottom" | "bottom half"
    ) {
        return fast_snap_window(SnapDir::Bottom);
    }
    if matches!(
        cleaned,
        "maximize"
            | "maximize window"
            | "maximise"
            | "maximise window"
            | "full size"
            | "make it fullscreen"
            | "fill the screen"
    ) {
        return fast_snap_window(SnapDir::Full);
    }
    if matches!(cleaned, "center window" | "center the window" | "centre window") {
        return fast_snap_window(SnapDir::Center);
    }

    // Minimise / hide controls — apply to the frontmost app, not Jarvis itself.
    if matches!(
        cleaned,
        "minimize" | "minimise" | "minimize this" | "minimize the window" | "minimise the window"
    ) {
        return fast_minimize_frontmost();
    }
    if matches!(
        cleaned,
        "minimize all"
            | "minimise all"
            | "minimize everything"
            | "hide everything"
            | "minimize all but this"
    ) {
        return fast_hide_others();
    }
    if matches!(
        cleaned,
        "show desktop" | "show the desktop" | "go to desktop"
    ) {
        return fast_show_desktop();
    }
    if matches!(
        cleaned,
        "next window" | "switch window" | "cycle window"
    ) {
        return fast_cycle_window(false);
    }
    if matches!(
        cleaned,
        "previous window" | "previous app" | "last window" | "go back to last window"
    ) {
        return fast_cycle_window(true);
    }

    // ── Browser deep control (Pack 3) ──────────────────────────────────
    // URL opening — "open <thing>.com" or "go to <site>" (heuristic).
    for verb in &["go to ", "navigate to ", "open url ", "visit "] {
        if let Some(url) = cleaned.strip_prefix(verb) {
            if let Some(answer) = fast_open_url(url.trim()) {
                return Some(answer);
            }
        }
    }
    // Bare "open X.com" / "open X dot com" — only if it looks like a domain.
    if let Some(rest) = cleaned.strip_prefix("open ") {
        let rest = rest.trim().trim_start_matches("the ");
        if looks_like_url(rest) {
            if let Some(answer) = fast_open_url(rest) {
                return Some(answer);
            }
        }
    }

    // Tab management — keystroke into the frontmost browser.
    if matches!(cleaned, "new tab" | "open a new tab" | "open new tab") {
        return fast_browser_keystroke("t", "command", "New tab.");
    }
    if matches!(cleaned, "close tab" | "close this tab" | "close the tab") {
        return fast_browser_keystroke("w", "command", "Closed.");
    }
    if matches!(cleaned, "next tab" | "switch to next tab") {
        // Cmd-Option-Right
        return fast_browser_key_code(124, "command,option", "Next tab.");
    }
    if matches!(cleaned, "previous tab" | "switch to previous tab" | "last tab") {
        return fast_browser_key_code(123, "command,option", "Previous tab.");
    }
    if matches!(
        cleaned,
        "reopen tab" | "reopen the last tab" | "reopen closed tab" | "bring back the tab"
    ) {
        return fast_browser_keystroke("t", "command,shift", "Reopened.");
    }

    // Navigation — back / forward / reload.
    if matches!(cleaned, "go back" | "back" | "navigate back") {
        return fast_browser_keystroke("[", "command", "Going back.");
    }
    if matches!(cleaned, "go forward" | "forward" | "navigate forward") {
        return fast_browser_keystroke("]", "command", "Going forward.");
    }
    if matches!(
        cleaned,
        "reload" | "refresh" | "reload the page" | "refresh the page"
    ) {
        return fast_browser_keystroke("r", "command", "Reloading.");
    }
    if matches!(
        cleaned,
        "hard reload" | "force reload" | "hard refresh"
    ) {
        return fast_browser_keystroke("r", "command,shift", "Hard reload.");
    }
    if matches!(cleaned, "scroll to top" | "go to top") {
        return fast_browser_key_code(115, "", "Top.");
    }
    if matches!(cleaned, "scroll to bottom" | "go to bottom") {
        return fast_browser_key_code(119, "", "Bottom.");
    }

    None
}

/// Canonical app name table. Spoken (lowercase) → either an AppleScript app
/// name OR a URL to open in the default browser. Kept tight on purpose —
/// only the apps users actually demo with regularly. Everything else falls
/// through to claude CLI which can `ls /Applications` to discover names.
fn resolve_target(spoken: &str) -> Option<FastTarget> {
    use FastTarget::*;
    Some(match spoken.trim() {
        // Native apps
        "claude" | "claude desktop" => App("Claude"),
        "codex" | "codex desktop" => App("Codex"),
        "terminal" => App("Terminal"),
        "iterm" | "iterm2" => App("iTerm"),
        "chrome" | "google chrome" => App("Google Chrome"),
        "safari" => App("Safari"),
        "firefox" => App("Firefox"),
        "arc" => App("Arc"),
        "spotify" => App("Spotify"),
        "finder" => App("Finder"),
        "messages" => App("Messages"),
        "mail" => App("Mail"),
        "calendar" => App("Calendar"),
        "notes" => App("Notes"),
        "music" | "apple music" => App("Music"),
        "preview" => App("Preview"),
        "system settings" | "settings" | "system preferences" => App("System Settings"),
        "xcode" => App("Xcode"),
        "vscode" | "vs code" | "visual studio code" => App("Visual Studio Code"),
        "cursor" => App("Cursor"),
        "slack" => App("Slack"),
        "discord" => App("Discord"),
        "telegram" => App("Telegram"),
        "whatsapp" => App("WhatsApp"),
        "zoom" => App("zoom.us"),
        "photoshop" | "ps" => App("Adobe Photoshop 2024"),
        "illustrator" => App("Adobe Illustrator 2024"),
        "figma" => App("Figma"),
        "sketch" => App("Sketch"),
        "notion" => App("Notion"),
        "obsidian" => App("Obsidian"),
        "1password" | "one password" => App("1Password"),
        "activity monitor" => App("Activity Monitor"),
        "reminders" => App("Reminders"),
        "photos" => App("Photos"),
        "podcasts" => App("Podcasts"),
        "books" => App("Books"),
        "facetime" | "face time" => App("FaceTime"),
        "photo booth" => App("Photo Booth"),
        "linear" => App("Linear"),
        "warp" => App("Warp"),
        "raycast" => App("Raycast"),
        "alfred" => App("Alfred"),
        "screen studio" => App("Screen Studio"),
        // Websites (open via default browser)
        "instagram" => Url("https://www.instagram.com"),
        "youtube" => Url("https://www.youtube.com"),
        "twitter" | "x" => Url("https://x.com"),
        "github" => Url("https://github.com"),
        "gmail" => Url("https://mail.google.com"),
        "google" => Url("https://www.google.com"),
        "reddit" => Url("https://www.reddit.com"),
        "linkedin" => Url("https://www.linkedin.com"),
        "chatgpt" | "chat gpt" => Url("https://chat.openai.com"),
        "claude web" | "claude ai" => Url("https://claude.ai"),
        "tiktok" | "tik tok" => Url("https://www.tiktok.com"),
        "facebook" => Url("https://www.facebook.com"),
        "amazon" => Url("https://www.amazon.com"),
        "netflix" => Url("https://www.netflix.com"),
        "twitch" => Url("https://www.twitch.tv"),
        "stack overflow" | "stackoverflow" => Url("https://stackoverflow.com"),
        "hacker news" | "hn" => Url("https://news.ycombinator.com"),
        "perplexity" => Url("https://www.perplexity.ai"),
        _ => return None,
    })
}

enum FastTarget {
    App(&'static str),
    Url(&'static str),
}

fn fast_open(spoken: &str) -> Option<String> {
    let target = resolve_target(spoken)?;
    match target {
        FastTarget::App(name) => {
            if run_osascript(&format!("tell application \"{name}\" to activate")).is_ok() {
                // Push the freshly-activated window onto the user's external
                // display (Dell 4K). No-op when there's only one screen.
                move_front_window_to_external(name);
                Some(format!("Opening {name}."))
            } else {
                None // fall through to brain
            }
        }
        FastTarget::Url(url) => {
            let ok = Command::new("/usr/bin/open")
                .arg(url)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                // Send the default browser (now frontmost) to the external
                // display so the URL is visible where the user is working.
                move_front_window_to_external(spoken);
                Some(format!("Opening {spoken}."))
            } else {
                None
            }
        }
    }
}

/// Send `prompt_text` as a NEW chat in the desktop app named `app_name`
/// (e.g. "Claude", "Codex"). The recipe is the same one taught to the brain
/// via `VOICE_SYSTEM_PROMPT`, but implemented directly here so the user's
/// voice command doesn't pay the brain's ~3 s thinking tax — total flow
/// ~1.5-2 s end-to-end.
///
/// Steps: `pbcopy` the prompt → activate app → Cmd+N (new chat) → Cmd+V
/// (paste) → Return. We use `pbcopy` instead of `keystroke` for the prompt
/// text because long text via AppleScript keystroke is slow and mangles
/// quotes / special chars; clipboard paste is instant and lossless.
fn fast_send_to_app(app_name: &str, prompt_text: &str) -> Option<String> {
    // 1. Copy the prompt to the system clipboard via `pbcopy` with the text
    //    piped in over stdin — bypasses shell quoting entirely so the
    //    prompt can contain any characters (quotes, $, backslashes, etc.).
    let mut pbcopy = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .ok()?;
    {
        let stdin = pbcopy.stdin.as_mut()?;
        stdin.write_all(prompt_text.as_bytes()).ok()?;
    }
    let status = pbcopy.wait().ok()?;
    if !status.success() {
        return None;
    }

    // 2. Drive the target app: activate, new chat, paste, send.
    //    The `delay`s give the app time to come forward and create its
    //    new-chat input field before we paste. 0.4s + 0.2s + 0.1s = 0.7s
    //    total UI latency, which is unavoidable for chat-app GUIs.
    let script = format!(
        r#"tell application "{app_name}" to activate
delay 0.4
tell application "System Events"
    keystroke "n" using command down
    delay 0.2
    keystroke "v" using command down
    delay 0.1
    key code 36
end tell"#
    );

    if run_osascript(&script).is_ok() {
        // Push the chat window onto the Dell so the user sees the response
        // where they're actually working.
        move_front_window_to_external(app_name);
        Some(format!("Sent to {app_name}."))
    } else {
        None // fall through to brain — maybe the app isn't installed
    }
}

fn fast_close(spoken: &str) -> Option<String> {
    let name = match resolve_target(spoken)? {
        FastTarget::App(n) => n,
        FastTarget::Url(_) => return None, // can't "close" a URL
    };
    if run_osascript(&format!("tell application \"{name}\" to quit")).is_ok() {
        Some(format!("Closing {name}."))
    } else {
        None
    }
}

fn fast_time() -> String {
    // Shell out to `date` for nice locale-aware formatting without a dep.
    Command::new("/bin/date")
        .arg("+It's %-l:%M %p.")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "I can't read the clock right now.".into())
}

fn fast_date() -> String {
    Command::new("/bin/date")
        .arg("+Today is %A, %B %-d.")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "I can't read the date right now.".into())
}

fn fast_screenshot() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = format!("{home}/Desktop/jarvis-screenshot.png");
    let ok = Command::new("/usr/sbin/screencapture")
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then(|| "Screenshot saved to your Desktop.".into())
}

fn fast_volume_delta(delta: i32) -> Option<String> {
    // `output volume` is 0-100. Clamp via AppleScript.
    let script = format!(
        "set v to output volume of (get volume settings)\n\
         set v to v + ({delta})\n\
         if v < 0 then set v to 0\n\
         if v > 100 then set v to 100\n\
         set volume output volume v\n\
         set volume without output muted"
    );
    if run_osascript(&script).is_ok() {
        Some(if delta > 0 {
            "Louder.".into()
        } else {
            "Quieter.".into()
        })
    } else {
        None
    }
}

fn fast_volume_mute(mute: bool) -> Option<String> {
    let script = if mute {
        "set volume with output muted"
    } else {
        "set volume without output muted"
    };
    run_osascript(script).ok().map(|_| {
        if mute {
            "Muted.".into()
        } else {
            "Unmuted.".into()
        }
    })
}

fn fast_spotify(verb: &str, response: &str) -> Option<String> {
    let script = format!("tell application \"Spotify\" to {verb}");
    run_osascript(&script).ok().map(|_| response.into())
}

fn fast_lock_screen() -> Option<String> {
    // Cmd+Ctrl+Q is macOS's lock keystroke. Works on every macOS version
    // since Mojave without needing screensaver settings.
    let script =
        "tell application \"System Events\" to keystroke \"q\" using {command down, control down}";
    run_osascript(script).ok().map(|_| "Locking.".into())
}

fn fast_sleep() -> Option<String> {
    let ok = Command::new("/usr/bin/pmset")
        .arg("sleepnow")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then(|| "Sleeping.".into())
}

fn fast_url_search(prefix: &str, query: &str, label: &str) -> Option<String> {
    let q = query.trim().trim_end_matches(['.', '?', '!']);
    if q.is_empty() {
        return None;
    }
    // Naive URL-encoding — `open` itself accepts spaces but encode to be safe.
    let encoded = q
        .chars()
        .map(|c| match c {
            ' ' => "+".to_string(),
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect::<String>();
    let url = format!("{prefix}{encoded}");
    let ok = Command::new("/usr/bin/open")
        .arg(&url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then(|| format!("Searching {label} for {q}."))
}

// ── File operations (Pack 1) ────────────────────────────────────────────

/// Spotlight search via `mdfind -name`. Returns up to 5 absolute paths.
/// Skips Library/ and other system noise to surface user content.
fn mdfind_user_files(name: &str) -> Vec<String> {
    let out = match Command::new("/usr/bin/mdfind")
        .arg("-name")
        .arg(name)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let home = std::env::var("HOME").unwrap_or_default();
    stdout
        .lines()
        .filter(|p| {
            // Filter out system-internal hits; keep things under ~/ that
            // aren't deep in Library or in Application Support.
            p.starts_with(&home)
                && !p.contains("/Library/Caches/")
                && !p.contains("/Library/Containers/")
                && !p.contains("/Library/Application Support/")
                && !p.contains("/.Trash/")
        })
        .take(5)
        .map(|s| s.to_string())
        .collect()
}

/// Display name of a path: just the basename, without the directory.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn fast_find_file(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Tell me what to find.".into());
    }
    let hits = mdfind_user_files(name);
    if hits.is_empty() {
        return Some(format!("Nothing matching {name}."));
    }
    if hits.len() == 1 {
        return Some(format!("Found {}.", basename(&hits[0])));
    }
    let names: Vec<&str> = hits.iter().take(3).map(|p| basename(p)).collect();
    Some(format!(
        "Found {} matches. Top: {}.",
        hits.len(),
        names.join(", ")
    ))
}

fn fast_open_file(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Tell me which file to open.".into());
    }
    let hits = mdfind_user_files(name);
    let path = hits.first()?;
    let ok = Command::new("/usr/bin/open")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Some(format!("Opening {}.", basename(path)))
    } else {
        Some(format!("Couldn't open {}.", basename(path)))
    }
}

/// Parse "<folder name>" or "<folder name> on desktop" / "in downloads".
/// Returns (name, parent_dir).
fn parse_folder_target(input: &str) -> (String, String) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let desktop = format!("{home}/Desktop");
    let downloads = format!("{home}/Downloads");
    let documents = format!("{home}/Documents");
    let lc = input.to_lowercase();
    // Look for "on/in <folder>" suffix
    let split_words = [" on desktop", " in desktop", " in downloads", " on downloads", " in documents", " on documents"];
    for w in &split_words {
        if let Some(idx) = lc.rfind(w) {
            let name = input[..idx].trim().to_string();
            let dest = if w.contains("desktop") {
                desktop
            } else if w.contains("downloads") {
                downloads
            } else {
                documents
            };
            return (name, dest);
        }
    }
    // Default: ~/Desktop
    (input.trim().to_string(), desktop)
}

fn fast_create_folder(input: &str) -> Option<String> {
    if input.is_empty() {
        return Some("What should I name the folder?".into());
    }
    let (name, parent) = parse_folder_target(input);
    if name.is_empty() {
        return Some("What should I name the folder?".into());
    }
    // Reject path traversal — folder name must not contain '/'.
    if name.contains('/') {
        return Some("Folder name can't contain a slash.".into());
    }
    let path = format!("{parent}/{name}");
    let ok = Command::new("/bin/mkdir")
        .arg("-p")
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Some(format!("Created folder {name}."))
    } else {
        Some(format!("Couldn't create folder {name}."))
    }
}

/// Resolve a spoken destination ("downloads" / "trash" / "desktop") to an
/// absolute path. Returns None for unknown destinations.
fn resolve_move_dest(dest: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let lc = dest.to_lowercase();
    let lc = lc.trim_start_matches("the ").trim();
    match lc {
        "downloads" | "the downloads folder" | "downloads folder" => {
            Some(format!("{home}/Downloads"))
        }
        "desktop" | "the desktop" => Some(format!("{home}/Desktop")),
        "documents" | "the documents folder" => Some(format!("{home}/Documents")),
        "trash" | "the trash" | "bin" | "the bin" => Some(format!("{home}/.Trash")),
        _ => None,
    }
}

fn fast_move_file(name: &str, dest: &str) -> Option<String> {
    if name.is_empty() || dest.is_empty() {
        return Some("Tell me what to move and where.".into());
    }
    let dest_path = resolve_move_dest(dest)?;
    let hits = mdfind_user_files(name);
    let src = hits.first()?;
    // Use AppleScript "Finder move" rather than /bin/mv so the destination
    // gains a proper file animation, and "Trash" works without extra logic.
    let dest_name = if dest_path.ends_with("/.Trash") {
        "trash".to_string()
    } else {
        format!("folder \"{}\" of (path to home folder)", basename(&dest_path))
    };
    let escaped_src = src.replace('"', "\\\"");
    let script = format!(
        "tell application \"Finder\" to move (POSIX file \"{escaped_src}\") to {dest_name}"
    );
    if run_osascript(&script).is_ok() {
        Some(format!("Moved {} to {}.", basename(src), dest.trim()))
    } else {
        Some(format!("Couldn't move {}.", basename(src)))
    }
}

// ── Window management (Pack 2) ──────────────────────────────────────────

#[derive(Clone, Copy)]
enum SnapDir {
    Left,
    Right,
    Top,
    Bottom,
    Full,
    Center,
}

impl SnapDir {
    fn label(self) -> &'static str {
        match self {
            SnapDir::Left => "left half",
            SnapDir::Right => "right half",
            SnapDir::Top => "top half",
            SnapDir::Bottom => "bottom half",
            SnapDir::Full => "fullscreen",
            SnapDir::Center => "centered",
        }
    }
}

/// AppleScript expression that returns (x, y, w, h) of the visible frame of
/// the screen the frontmost window is on. macOS calls this NSScreen.visibleFrame.
const FRONTMOST_BOUNDS: &str = "tell application \"Finder\" to get bounds of window of desktop";

fn fast_snap_window(dir: SnapDir) -> Option<String> {
    // Visible frame = screen minus menu bar and dock. Reading via Finder's
    // `bounds of window of desktop` returns (left, top, right, bottom).
    let bounds_text = match read_osascript(FRONTMOST_BOUNDS) {
        Ok(s) => s,
        Err(_) => return Some("Couldn't read screen bounds.".into()),
    };
    let parts: Vec<i64> = bounds_text
        .trim()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if parts.len() != 4 {
        return Some("Couldn't parse screen bounds.".into());
    }
    let (left, top, right, bottom) = (parts[0], parts[1], parts[2], parts[3]);
    let w = right - left;
    let h = bottom - top;
    let (x, y, ww, hh) = match dir {
        SnapDir::Left => (left, top, w / 2, h),
        SnapDir::Right => (left + w / 2, top, w / 2, h),
        SnapDir::Top => (left, top, w, h / 2),
        SnapDir::Bottom => (left, top + h / 2, w, h / 2),
        SnapDir::Full => (left, top, w, h),
        SnapDir::Center => {
            let cw = w * 70 / 100;
            let ch = h * 75 / 100;
            (left + (w - cw) / 2, top + (h - ch) / 2, cw, ch)
        }
    };
    // Set bounds of the frontmost window via System Events. Apps that
    // disallow window resizing (Finder pop-ups, System Settings) will fail
    // silently — we still return success since the speech is committed.
    let script = format!(
        "tell application \"System Events\" to tell (first process whose frontmost is true) \
         to tell front window to set {{position, size}} to {{{{{x}, {y}}}, {{{ww}, {hh}}}}}"
    );
    if run_osascript(&script).is_ok() {
        Some(format!("Snapped to {}.", dir.label()))
    } else {
        Some(format!("Couldn't snap window to {}.", dir.label()))
    }
}

fn fast_minimize_frontmost() -> Option<String> {
    // Cmd-M is the system-wide minimise shortcut. Works regardless of which
    // app has focus (as long as it has a window with a minimise button).
    let script = "tell application \"System Events\" to keystroke \"m\" using {command down}";
    if run_osascript(script).is_ok() {
        Some("Minimised.".into())
    } else {
        Some("Couldn't minimise.".into())
    }
}

fn fast_hide_others() -> Option<String> {
    // Cmd-Opt-H hides all apps except the frontmost.
    let script = "tell application \"System Events\" to keystroke \"h\" \
         using {command down, option down}";
    if run_osascript(script).is_ok() {
        Some("Hiding others.".into())
    } else {
        Some("Couldn't hide other windows.".into())
    }
}

fn fast_show_desktop() -> Option<String> {
    // F11 (Mission Control: Show Desktop) — also reachable as fn-F11. We use
    // the System Events `key code 103` which is the dedicated Show Desktop key.
    let script = "tell application \"System Events\" to key code 103";
    if run_osascript(script).is_ok() {
        Some("Showing desktop.".into())
    } else {
        Some("Couldn't show desktop.".into())
    }
}

fn fast_cycle_window(backward: bool) -> Option<String> {
    // Cmd-` cycles between windows of the *current* app (forward).
    // Cmd-Shift-` cycles backward.
    let script = if backward {
        "tell application \"System Events\" to keystroke \"`\" \
         using {command down, shift down}"
    } else {
        "tell application \"System Events\" to keystroke \"`\" using {command down}"
    };
    if run_osascript(script).is_ok() {
        Some(if backward {
            "Previous window.".into()
        } else {
            "Next window.".into()
        })
    } else {
        Some("Couldn't switch window.".into())
    }
}

/// Like `run_osascript` but captures stdout instead of just status. Used when
/// we need the AppleScript's return value (e.g. window bounds).
fn read_osascript(script: &str) -> Result<String, String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("osascript failed to launch: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("osascript: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ── Browser deep control (Pack 3) ───────────────────────────────────────

/// Heuristic: does this spoken text look like a URL or domain?
/// Accepts: "google.com", "google dot com", "https://...", "github.com/foo".
fn looks_like_url(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.starts_with("http://") || t.starts_with("https://") {
        return true;
    }
    // "X dot Y" pattern from STT (no actual dot)
    if t.contains(" dot ") {
        return true;
    }
    // Explicit "." with a recognised TLD-ish tail
    if t.contains('.') {
        let tld_tail = t.rsplit('.').next().unwrap_or("");
        let common_tlds = [
            "com", "org", "net", "io", "co", "ai", "dev", "app", "edu", "gov", "uk", "us", "ua",
            "eu", "de", "fr", "es", "it", "pl", "ca",
        ];
        return common_tlds.iter().any(|t| tld_tail.starts_with(t));
    }
    false
}

/// Normalise a spoken URL: replace " dot " → ".", strip leading "www.",
/// add https:// scheme if missing.
fn normalise_spoken_url(text: &str) -> String {
    let mut t = text.trim().to_lowercase();
    t = t.replace(" dot ", ".");
    t = t.replace(" slash ", "/");
    if !t.starts_with("http://") && !t.starts_with("https://") {
        t = format!("https://{t}");
    }
    t
}

fn fast_open_url(text: &str) -> Option<String> {
    if !looks_like_url(text) {
        return None;
    }
    let url = normalise_spoken_url(text);
    let ok = Command::new("/usr/bin/open")
        .arg(&url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Some(format!("Opening {url}."))
    } else {
        Some(format!("Couldn't open {url}."))
    }
}

/// Send a keystroke to the frontmost app (assumed to be a browser). Modifiers
/// is a comma-separated string like "command" or "command,shift".
fn fast_browser_keystroke(key: &str, modifiers: &str, response: &'static str) -> Option<String> {
    let mods = format_modifiers(modifiers);
    let using = if mods.is_empty() {
        String::new()
    } else {
        format!(" using {{{mods}}}")
    };
    // Escape only the special characters AppleScript treats literally inside
    // the double-quoted keystroke argument. Our keys are single ascii chars
    // ([ ] r t w m c v etc.), so the escape table is small.
    let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"System Events\" to keystroke \"{escaped}\"{using}"
    );
    if run_osascript(&script).is_ok() {
        Some(response.into())
    } else {
        None
    }
}

/// Like `fast_browser_keystroke` but uses `key code N` (numeric scancode) for
/// keys that can't be typed as text (arrows, function keys, Home, End).
fn fast_browser_key_code(code: u32, modifiers: &str, response: &'static str) -> Option<String> {
    let mods = format_modifiers(modifiers);
    let using = if mods.is_empty() {
        String::new()
    } else {
        format!(" using {{{mods}}}")
    };
    let script = format!("tell application \"System Events\" to key code {code}{using}");
    if run_osascript(&script).is_ok() {
        Some(response.into())
    } else {
        None
    }
}

/// "command,shift" → "command down, shift down"
fn format_modifiers(modifiers: &str) -> String {
    modifiers
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| format!("{} down", s.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Match common ways the user can ask for the morning briefing. Kept tight —
/// just the phrases that are unambiguously about starting the day.
fn is_morning_greeting(command: &str) -> bool {
    let c = command
        .to_lowercase()
        .trim()
        .trim_end_matches(['.', '?', '!'])
        .to_string();
    let c = c.trim();
    matches!(
        c,
        "good morning"
            | "good morning jarvis"
            | "morning jarvis"
            | "morning"
            | "start my day"
            | "what's on my plate today"
            | "brief me"
            | "my morning briefing"
    )
}

/// Gather the live context the brain needs to compose a morning briefing:
/// time, date, and the next ~12 hours of calendar events. Pure local
/// AppleScript / shell — no network, no LLM. Returns a plain-text block
/// ready to drop into a prompt.
fn morning_context() -> String {
    let mut out = String::new();

    // Time + date
    let time = Command::new("/bin/date")
        .arg("+%-l:%M %p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let date = Command::new("/bin/date")
        .arg("+%A, %B %-d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    out.push_str(&format!("- Current time: {time}\n"));
    out.push_str(&format!("- Today is: {date}\n"));

    // Calendar — events from now through 12 hours out. AppleScript talks to
    // the Calendar.app database, so the user must have at least started
    // Calendar.app once. Pretty-print as "HH:MM — Title".
    //
    // Note: this script intentionally returns a single-line result with `||`
    // separators between events because AppleScript's text-of-list is hard
    // to control across locales.
    let cal_script = r#"
set out to ""
try
    tell application "Calendar"
        set now to (current date)
        set later to now + (12 * hours)
        set found to {}
        repeat with cal in calendars
            try
                set evs to (every event of cal whose start date ≥ now and start date ≤ later)
                set found to found & evs
            end try
        end repeat
        repeat with ev in found
            set t to ((start date of ev) as string)
            set out to out & t & " — " & (summary of ev) & "||"
        end repeat
    end tell
on error e
    set out to "CALENDAR_ERROR: " & e
end try
return out
"#;

    let cal_output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(cal_script)
        .output();
    match cal_output {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if raw.is_empty() {
                out.push_str("- Calendar: no events in the next 12 hours.\n");
            } else if raw.starts_with("CALENDAR_ERROR") {
                out.push_str(&format!("- Calendar: unavailable ({raw}).\n"));
            } else {
                out.push_str("- Upcoming events (next 12 hours):\n");
                for ev in raw.split("||").filter(|s| !s.trim().is_empty()) {
                    out.push_str(&format!("    {}\n", ev.trim()));
                }
            }
        }
        _ => {
            out.push_str("- Calendar: unavailable (AppleScript failed).\n");
        }
    }

    out
}

/// Multi-monitor: send freshly-opened windows to the user's external display
/// (Dell U2725QE in this user's case) instead of the laptop's built-in screen.
/// Returns the frame of the first non-main NSScreen, converted from Cocoa
/// bottom-left coords into screen top-left coords (which `System Events → set
/// position of window` expects). `None` when there's only one display.
fn external_display_frame() -> Option<(i32, i32, i32, i32)> {
    let script = r#"
        ObjC.import("AppKit");
        const screens = $.NSScreen.screens;
        const main = $.NSScreen.mainScreen;
        const mainH = main.frame.size.height;
        let answer = "";
        for (let i = 0; i < screens.count; i++) {
            const s = screens.objectAtIndex(i);
            if (!s.isEqual(main)) {
                const f = s.frame;
                const x = Math.round(f.origin.x);
                // Convert Cocoa bottom-left origin to screen top-left.
                const y = Math.round(mainH - (f.origin.y + f.size.height));
                const w = Math.round(f.size.width);
                const h = Math.round(f.size.height);
                answer = `${x},${y},${w},${h}`;
                break;
            }
        }
        answer
    "#;
    let output = Command::new("/usr/bin/osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<i32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    if parts.len() != 4 {
        return None;
    }
    Some((parts[0], parts[1], parts[2], parts[3]))
}

/// After a fast-path `open` activates an app, wait briefly for the window to
/// appear, then move the *frontmost* app's front window onto the external
/// display. Sized to ~90% of the display with a small margin so it looks
/// purposefully placed, not just shoved into a corner.
///
/// `app_name` is only used for the trace log — the actual move targets the
/// frontmost process, which correctly handles both `App` targets (the just-
/// activated app) and `Url` targets (the default browser that came forward).
fn move_front_window_to_external(app_name: &str) {
    let Some((x, y, w, h)) = external_display_frame() else {
        // Single display — nothing to do.
        return;
    };

    // Wait for the window to actually exist. Most apps create their first
    // window within 200-300 ms of `activate`; 400 ms is a safe upper bound
    // without feeling sluggish. (For already-running apps this is wasted
    // time — they're activated instantly — but the wait is harmless.)
    std::thread::sleep(Duration::from_millis(400));

    // Inset by 5% on each side so the window doesn't hug the display edges.
    let pad_x = (w / 20).max(20);
    let pad_y = (h / 20).max(20);
    let win_x = x + pad_x;
    let win_y = y + pad_y;
    let win_w = (w - 2 * pad_x).max(400);
    let win_h = (h - 2 * pad_y).max(300);

    // `first application process whose frontmost is true` handles both App
    // targets (the one we just activated) and Url targets (whichever browser
    // handled the `open`). Errors are NO LONGER swallowed by `try`/`end try`
    // — a silent failure means the window quietly stayed on the wrong
    // display, which is the exact bug we were chasing. We let osascript's
    // exit code propagate and log it explicitly.
    let script = format!(
        r#"tell application "System Events"
    set frontApp to first application process whose frontmost is true
    set frontName to name of frontApp
    tell frontApp
        if (count of windows) > 0 then
            set position of window 1 to {{{win_x}, {win_y}}}
            set size of window 1 to {{{win_w}, {win_h}}}
            return "moved " & frontName
        else
            return "no-window:" & frontName
        end if
    end tell
end tell"#,
    );

    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let msg = String::from_utf8_lossy(&o.stdout).trim().to_string();
            println!(
                "[pipeline] window-move ({app_name}) → {msg} | target rect = ({win_x},{win_y},{win_w},{win_h})"
            );
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!(
                "[pipeline] window-move FAILED ({app_name}): osascript exit={:?} stderr={}",
                o.status.code(),
                stderr.trim()
            );
            eprintln!("[pipeline]   → most common cause: Jarvis.app lacks Accessibility permission. Check System Settings → Privacy & Security → Accessibility.");
        }
        Err(e) => {
            eprintln!("[pipeline] window-move COULD NOT RUN ({app_name}): {e}");
        }
    }
}

fn run_osascript(script: &str) -> Result<(), String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("osascript failed to launch: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("osascript: {}", stderr.trim()));
    }
    Ok(())
}

/// Dictation prefixes — longest first so "claude code" wins over "claude".
/// "code" is included as a shorter alias for fluency ("code, refactor this").
const DICTATION_PREFIXES: &[&str] = &["claude code", "claude", "code"];

/// If `command` starts with one of [`DICTATION_PREFIXES`], return the prompt
/// with the prefix and any trailing comma/colon/whitespace stripped. Matches
/// "claude foo", "claude, foo", "claude: foo", and "claude. foo".
fn strip_dictation_prefix(command: &str) -> Option<String> {
    let lower = command.to_lowercase();
    for &prefix in DICTATION_PREFIXES {
        if !lower.starts_with(prefix) {
            continue;
        }
        // The next char (if any) must be whitespace or punctuation — otherwise
        // we'd swallow words like "clauderise" or "codebase".
        let rest_start = prefix.len();
        let next = lower.as_bytes().get(rest_start).copied();
        let boundary_ok = matches!(
            next,
            None | Some(b' ' | b'\t' | b',' | b':' | b'.' | b';' | b'-')
        );
        if !boundary_ok {
            continue;
        }
        let trimmed = command[rest_start..]
            .trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, ',' | ':' | '.' | ';' | '-')
            })
            .trim_end()
            .to_string();
        return Some(trimmed);
    }
    None
}

/// Type a prompt into the user's frontmost Terminal.app window. `do script
/// "<text>" in window 1` injects the text into the currently selected tab's
/// foreground process (with an implicit Return), so a running Claude Code TUI
/// receives it the same way you'd type it manually.
///
/// Important: `do script` WITHOUT the `in window 1` clause would open a fresh
/// window and run the text as a shell command — that would land in a new
/// shell instead of the active Claude session. We always target window 1.
///
/// The prompt is passed as an AppleScript `argv` item so quotes / backslashes
/// / smart quotes from STT never need escaping — the value crosses the FFI
/// boundary as one opaque string.
fn dictate_to_terminal(prompt: &str) -> Result<(), String> {
    let script = r#"on run argv
    if (count of argv) = 0 then error "no prompt"
    tell application "Terminal"
        if not running then error "Terminal is not running"
        activate
        if (count of windows) = 0 then error "Terminal has no open windows"
        do script (item 1 of argv) in window 1
    end tell
end run"#;
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .arg("--")
        .arg(prompt)
        .output()
        .map_err(|e| format!("osascript failed to launch: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }
    Ok(())
}

/// Fire-and-forget chime via macOS's built-in system sounds. We spawn `afplay`
/// without awaiting it so the pipeline returns to idle immediately — the user
/// hears the cue while we're already listening for the next utterance.
fn play_chime(name: &str) {
    let path = format!("/System/Library/Sounds/{name}.aiff");
    let _ = Command::new("/usr/bin/afplay").arg(&path).spawn();
}

// ---------------------------------------------------------------------------
// Persistent conversation journal
//
// Stored at `~/.jarvis/journal.jsonl` — one JSON object per line, append-only.
// We append on every successful turn and hydrate the last N on startup so
// Jarvis has continuity across app restarts (not just within one process).
//
// Format:
//   {"ts":1747449801,"user":"...","jarvis":"..."}
//
// Append-only with no rotation for now. At ~200 bytes/turn and ~50 turns/day,
// that's ~3.5 MB/year — manageable. Rotation/summarization is a Wave-3 task.
// ---------------------------------------------------------------------------

/// Returns `~/.jarvis/journal.jsonl`, creating the parent directory if needed.
fn journal_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME unset".to_string())?;
    let dir = std::path::PathBuf::from(home).join(".jarvis");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir.join("journal.jsonl"))
}

/// Append one user↔jarvis turn to the journal. Each line is a self-contained
/// JSON object so the file is robust to torn writes — a corrupt line just
/// gets skipped on hydrate rather than poisoning everything after it.
fn append_to_journal(user: &str, jarvis: &str, when: SystemTime) -> Result<(), String> {
    let path = journal_path()?;
    let ts = when
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = json!({
        "ts": ts,
        "user": user,
        "jarvis": jarvis,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    writeln!(file, "{}", entry)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Read the last `n` valid turns from the journal. Returns them in
/// chronological order (oldest → newest), ready to push into the deque.
///
/// We read the whole file and take the tail — at expected sizes (single-
/// digit MB), this is fast and simple. If the file ever grows enough to
/// matter, swap to a reverse line iterator.
fn load_recent_turns(n: usize) -> Result<Vec<ConversationTurn>, String> {
    let path = journal_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;

    // Parse last 4x N lines and keep the N most recent that parse cleanly
    // (so a few corrupt tail lines can't starve the hydrate of valid ones).
    let want = n.saturating_mul(4).max(n);
    let mut parsed: Vec<ConversationTurn> = contents
        .lines()
        .rev()
        .take(want)
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            let ts = v.get("ts")?.as_u64()?;
            let user = v.get("user")?.as_str()?.to_string();
            let jarvis = v.get("jarvis")?.as_str()?.to_string();
            let when = UNIX_EPOCH.checked_add(Duration::from_secs(ts))?;
            Some(ConversationTurn { user, jarvis, when })
        })
        .collect();

    // Trim to N most-recent and reverse back to chronological order.
    parsed.truncate(n);
    parsed.reverse();
    Ok(parsed)
}

/// Detect a "Jarvis" / "Hey Jarvis" wake word at the start of a transcript and
/// split off the command that follows it.
fn match_wake_word(transcript: &str) -> WakeMatch {
    // Wake variants, longest first so "hey jarvis" wins over "jarvis".
    // Includes common STT mis-transcriptions of "Jarvis".
    const PREFIXES: &[&str] = &[
        "hey jarvis",
        "hi jarvis",
        "ok jarvis",
        "okay jarvis",
        "hey service",
        "jarvis",
        "service",
        "jarvas",
        "jarvus",
        "jarves",
        "jervis",
    ];

    let lower = transcript.to_lowercase();
    let Some(start) = lower.find(|c: char| c.is_alphanumeric()) else {
        return WakeMatch::None;
    };
    let head = &lower[start..];

    for prefix in PREFIXES {
        if let Some(after) = head.strip_prefix(prefix) {
            // The prefix must end on a word boundary ("jarvisx" is not a match).
            if after.chars().next().is_some_and(|c| c.is_alphanumeric()) {
                continue;
            }
            // Slice the command from the *original* transcript to keep its case.
            let command = transcript[start + prefix.len()..]
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .trim();
            return if command.is_empty() {
                WakeMatch::WakeOnly
            } else {
                WakeMatch::Command(command.to_string())
            };
        }
    }
    WakeMatch::None
}

// ---------------------------------------------------------------------------
// Unit tests for pure functions.
//
// These cover the deterministic helpers: wake-word matching, dictation-prefix
// stripping, and morning-greeting detection. The fast-path verb table is
// NOT tested here because its handlers have side effects (run osascript /
// `open -a` / `pmset` / etc.) — testing those would actually launch apps.
// If we ever factor the *parsing* out from the *executing*, those would be
// easy targets for unit tests too.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn wake_command(input: &str) -> Option<String> {
        match match_wake_word(input) {
            WakeMatch::Command(cmd) => Some(cmd),
            _ => None,
        }
    }

    #[test]
    fn wake_word_basic_split() {
        assert_eq!(
            wake_command("jarvis what time is it"),
            Some("what time is it".to_string())
        );
    }

    #[test]
    fn wake_word_prefers_longest_prefix() {
        // "hey jarvis" must beat the shorter "jarvis" prefix so the command
        // is "open chrome", not "jarvis open chrome".
        assert_eq!(
            wake_command("hey jarvis open chrome"),
            Some("open chrome".to_string())
        );
    }

    #[test]
    fn wake_word_preserves_command_case() {
        // We need the brain to see proper casing for names like "Chrome".
        assert_eq!(
            wake_command("Jarvis open Chrome"),
            Some("open Chrome".to_string())
        );
    }

    #[test]
    fn wake_word_handles_punctuation_after_wake() {
        // "Jarvis, what's the time?" — comma between wake and command.
        assert_eq!(
            wake_command("Jarvis, what's the time?"),
            Some("what's the time?".to_string())
        );
    }

    #[test]
    fn wake_word_bare_returns_wake_only() {
        // Bare wake word with no command → caller opens the AwaitingCommand
        // window and gives a verbal "Yes?" ack.
        assert!(matches!(
            match_wake_word("Jarvis."),
            WakeMatch::WakeOnly
        ));
    }

    #[test]
    fn wake_word_no_wake_returns_none() {
        assert!(matches!(
            match_wake_word("what time is it"),
            WakeMatch::None
        ));
    }

    #[test]
    fn wake_word_empty_returns_none() {
        assert!(matches!(match_wake_word(""), WakeMatch::None));
    }

    #[test]
    fn wake_word_rejects_substring_match() {
        // "jarvisx" must not match "jarvis" — word boundary required.
        assert!(matches!(
            match_wake_word("jarvisx hello"),
            WakeMatch::None
        ));
    }

    #[test]
    fn wake_word_accepts_misheard_variants() {
        // Whisper sometimes mishears "Jarvis" as "service" or "jervis".
        assert_eq!(
            wake_command("service open chrome"),
            Some("open chrome".to_string())
        );
        assert_eq!(
            wake_command("jervis what time"),
            Some("what time".to_string())
        );
    }

    // ----- strip_dictation_prefix -----

    #[test]
    fn dictation_simple() {
        assert_eq!(
            strip_dictation_prefix("claude refactor this function"),
            Some("refactor this function".to_string())
        );
    }

    #[test]
    fn dictation_punctuation() {
        assert_eq!(
            strip_dictation_prefix("claude, fix the bug"),
            Some("fix the bug".to_string())
        );
        assert_eq!(
            strip_dictation_prefix("claude: list files"),
            Some("list files".to_string())
        );
    }

    #[test]
    fn dictation_longer_prefix_wins() {
        // "claude code" comes before "claude" in DICTATION_PREFIXES so the
        // stripped prompt is "review my diff", not "code review my diff".
        assert_eq!(
            strip_dictation_prefix("claude code review my diff"),
            Some("review my diff".to_string())
        );
    }

    #[test]
    fn dictation_rejects_substring() {
        // "clauderise" must NOT match "claude" — no word boundary after.
        assert_eq!(strip_dictation_prefix("clauderise this"), None);
        // "codebase" must NOT match "code".
        assert_eq!(strip_dictation_prefix("codebase audit"), None);
    }

    #[test]
    fn dictation_returns_none_when_no_prefix() {
        assert_eq!(strip_dictation_prefix("what time is it"), None);
    }

    // ----- is_morning_greeting -----

    #[test]
    fn morning_greeting_variants() {
        assert!(is_morning_greeting("good morning"));
        assert!(is_morning_greeting("Good Morning."));
        assert!(is_morning_greeting("morning jarvis"));
        assert!(is_morning_greeting("brief me"));
    }

    #[test]
    fn morning_greeting_rejects_unrelated() {
        assert!(!is_morning_greeting("good evening"));
        assert!(!is_morning_greeting("hello"));
        assert!(!is_morning_greeting("what time is it"));
    }

    // ----- journal round-trip -----

    #[test]
    fn journal_entry_parses_back() {
        // Verify our serialization format round-trips. We construct an entry,
        // serialize it the way append_to_journal does, then re-parse it the
        // way load_recent_turns does.
        let user = "what's the weather";
        let jarvis = "Sunny and 72.";
        let ts = 1_747_449_801_u64;
        let line = serde_json::json!({
            "ts": ts,
            "user": user,
            "jarvis": jarvis,
        })
        .to_string();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("ts").unwrap().as_u64(), Some(ts));
        assert_eq!(v.get("user").unwrap().as_str(), Some(user));
        assert_eq!(v.get("jarvis").unwrap().as_str(), Some(jarvis));
    }

    // ----- File operations (Pack 1) -----

    #[test]
    fn basename_strips_directories() {
        assert_eq!(super::basename("/Users/foo/bar/baz.txt"), "baz.txt");
        assert_eq!(super::basename("/single"), "single");
        assert_eq!(super::basename("trailing/"), "");
        assert_eq!(super::basename("noslash"), "noslash");
    }

    #[test]
    fn parse_folder_target_defaults_to_desktop() {
        let (name, dest) = super::parse_folder_target("Project Alpha");
        assert_eq!(name, "Project Alpha");
        assert!(dest.ends_with("/Desktop"));
    }

    #[test]
    fn parse_folder_target_honors_in_downloads() {
        let (name, dest) = super::parse_folder_target("Bills in downloads");
        assert_eq!(name, "Bills");
        assert!(dest.ends_with("/Downloads"));
    }

    #[test]
    fn parse_folder_target_honors_on_desktop_suffix() {
        let (name, dest) = super::parse_folder_target("Q4 on desktop");
        assert_eq!(name, "Q4");
        assert!(dest.ends_with("/Desktop"));
    }

    #[test]
    fn parse_folder_target_in_documents() {
        let (name, dest) = super::parse_folder_target("Receipts in documents");
        assert_eq!(name, "Receipts");
        assert!(dest.ends_with("/Documents"));
    }

    #[test]
    fn resolve_move_dest_known_destinations() {
        // We can't assert paths because $HOME varies, but we can check
        // each destination resolves to Some.
        assert!(super::resolve_move_dest("downloads").is_some());
        assert!(super::resolve_move_dest("the downloads folder").is_some());
        assert!(super::resolve_move_dest("desktop").is_some());
        assert!(super::resolve_move_dest("trash").is_some());
        assert!(super::resolve_move_dest("the bin").is_some());
        assert!(super::resolve_move_dest("documents").is_some());
    }

    #[test]
    fn resolve_move_dest_unknown_returns_none() {
        assert!(super::resolve_move_dest("a random folder").is_none());
        assert!(super::resolve_move_dest("").is_none());
    }

    // ----- Browser deep (Pack 3) -----

    #[test]
    fn looks_like_url_positive_cases() {
        assert!(super::looks_like_url("google.com"));
        assert!(super::looks_like_url("google dot com"));
        assert!(super::looks_like_url("https://example.org"));
        assert!(super::looks_like_url("http://localhost:1420"));
        assert!(super::looks_like_url("github.com/user/repo"));
        assert!(super::looks_like_url("anthropic.ai"));
    }

    #[test]
    fn looks_like_url_negative_cases() {
        assert!(!super::looks_like_url("the weather today"));
        assert!(!super::looks_like_url("hello world"));
        assert!(!super::looks_like_url(""));
        assert!(!super::looks_like_url("file.txt")); // not a known TLD
    }

    #[test]
    fn normalise_spoken_url_adds_scheme() {
        assert_eq!(super::normalise_spoken_url("google.com"), "https://google.com");
        assert_eq!(
            super::normalise_spoken_url("https://example.org"),
            "https://example.org"
        );
    }

    #[test]
    fn normalise_spoken_url_substitutes_dot() {
        assert_eq!(
            super::normalise_spoken_url("github dot com"),
            "https://github.com"
        );
    }

    #[test]
    fn normalise_spoken_url_substitutes_slash() {
        assert_eq!(
            super::normalise_spoken_url("github dot com slash anthropic"),
            "https://github.com/anthropic"
        );
    }

    #[test]
    fn format_modifiers_single() {
        assert_eq!(super::format_modifiers("command"), "command down");
    }

    #[test]
    fn format_modifiers_multiple() {
        assert_eq!(
            super::format_modifiers("command,shift"),
            "command down, shift down"
        );
    }

    #[test]
    fn format_modifiers_empty() {
        assert_eq!(super::format_modifiers(""), "");
    }
}
