# Follow-up findings from the 12-agent research session

This is the unfinished work from the autonomous 12-agent research sweep on 2026-05-17. All CRITICAL security + HIGH UX safety findings were landed in the same session. The items below are intentionally deferred — either too big for a single session, or they need design decisions you (the owner) should make.

## CRITICAL — still open

### claude CLI runs with `--permission-mode bypassPermissions`
File: `src-tauri/src/agent.rs` (around line 350)

Every transcribed utterance is piped to `claude -p` with permission prompts disabled. Any audio reaching the mic (TV ad, voice over speakers, prompt injection via a webpage Claude reads) can trigger arbitrary `Bash` / `Write` / `Edit` tool calls under your UID.

**Fix:** drop `bypassPermissions`. Either:
- Replace with `--allowed-tools "Read,Glob,Grep,WebSearch"` (read-only) and require explicit user opt-in for write tools via voice ("yes, let claude write code")
- Or proxy through a confirmation gate similar to the destructive-verb gate just added

This is the single biggest security risk in Jarvis. Not landed this session because it changes core agent behavior and needs your design call on how voice→write should work.

## HIGH — quick wins for next session (< 1 hour each)

### Skills YAML allows arbitrary `shell:` from `~/.jarvis/skills/`
File: `src-tauri/src/skills.rs`

Any process running as the user (browser download, npm postinstall script) can drop a YAML there; `notify` watcher hot-loads it, next matching trigger fires it. No signature, no allowlist.

**Fix:** add `JARVIS_ALLOW_USER_SHELL=1` env gate. With it unset, `shell:` and `applescript:` steps in **user** YAML are rejected (bundled skills still work). Document the gate in the README skill section.

### `.env` file permission check
File: `src-tauri/src/config.rs` (or wherever ELEVENLABS_API_KEY is read)

`~/.jarvis/.env` is world-readable by default. Add a startup check that refuses to load if mode != 0600 and prints a remediation (`chmod 600 ~/.jarvis/.env`).

### Voice → keystroke needs frontmost-app allowlist
File: `src-tauri/src/pipeline.rs` — `fast_send_to_app`, `fast_browser_keystroke`

A mis-transcribed phrase while Terminal, 1Password, or Keychain Access is frontmost can paste attacker-influenced text. Combined with `bypassPermissions` above, that's a full RCE chain from mic.

**Fix:** read frontmost bundle ID via System Events, refuse to send keys unless it's in `{Claude, Codex, Safari, Chrome, Arc, Firefox, iTerm, Terminal}` (the last two only with explicit opt-in).

## HIGH — bigger items (2-4 hours each)

### Barge-in during TTS (UX agent — Critical)
Keep VAD hot during TTS, use macOS `AVAudioEngine` `voiceProcessing` for AEC, fade TTS in 150ms on speech detection. Without this, Jarvis can't be interrupted mid-sentence — 2026 standard from ChatGPT Voice / Siri.

### Cross-turn context resolver
Keep a 5-turn entity stack (last app, file, URL, contact mentioned). "open it" / "the one I just mentioned" should resolve to the latest mentioned entity. Today every turn is stateless.

### Discoverability — "what can you do" / "help me"
70+ verbs, no voice-discoverable index. Speak top-5 verbs by category on "help me"; on unmatched utterance, suggest nearest verb via Levenshtein over the verb table.

### Mode panic-guard sentinel
Backend agent #1. Wrap `process()` worker in a `scopeguard` so a panic mid-processing resets `Mode::Busy` → `Mode::Idle`. Without this, one panic leaves Jarvis "thinking forever".

## MEDIUM — backlog

| Finding | Agent | File | Effort |
|---|---|---|---|
| Text input fallback for mute users | A11y #2 | `src/main.ts` | M |
| Persistent transcript panel (vs auto-fade trace) | A11y #3 | `src/hud/panels.ts` | M |
| Three.js plane 280×280 → 160×160 + ghost as post-pass | Frontend #1 | `src/hud/graph.ts` | S |
| `setPixelRatio` cap at 1.5 fullscreen | Frontend #2 | `src/hud/graph.ts` | S |
| sysinfo via Tauri events, not 1.5s polling | Frontend #3 | `src/hud/sysinfo.ts` + Rust | M |
| Three.js named imports (drop ~150-200 KB) | Frontend #4 | `src/hud/graph.ts` | S |
| `aho-corasick` for `try_fast_action` dispatch | Backend #3 / Code #1 | `src/pipeline.rs` | M |
| Extract pipeline.rs into `fast_path`, `brain`, `conversation` modules | Backend #5 | `src/pipeline.rs` | L |
| Real `JarvisError` enum instead of `Result<(), String>` | Code #3 | new `error.rs` | M |
| Magic numbers → named constants | Code #4 | various | S |
| Integration test for try_fast_action routing | Code #5 / API tester | `src-tauri/tests/` | M |
| `ioreg` instead of `system_profiler` for WiFi (80ms → 10ms) | Perf #1 | `src/sysinfo.rs` | S |
| Persistent `osascript -i` instead of cold-spawn per call | Perf #2 | `src/pipeline.rs` | M |
| Idle-state Three.js render throttle (60fps → 10fps) | Perf #3 | `src/hud/graph.ts` | S |
| Skills parallel-step block | Perf #4 | `src/skills.rs` | M |
| Auto-update via `tauri-plugin-updater` | DevOps #4 | `tauri.conf.json` | M |
| Sentry crash reporting (opt-in via `JARVIS_TELEMETRY=1`) | DevOps #5 | `src-tauri/src/lib.rs` | M |

## Competitive features to consider (Trend Researcher agent)

Ranked by impact:

1. **Screen vision context** (screenshot → Claude with "what's on screen") — closes the biggest gap vs Raycast / Claude Mac app
2. **AI Extensions / MCP tool-calling** — let skills be tools Claude picks autonomously
3. **Post-STT cleanup pass** — Wispr-style polish before fast-path match
4. **Persistent memory + per-app profiles** — tone, vocab, shortcuts per active app
5. **Offline STT mode** — bundle Whisper Turbo / Parakeet, user-swappable
6. **Global quick-entry hotkey** — Caps Lock dictate-anywhere, paste at cursor (Claude Desktop pattern)
7. **Meeting / transcription mode** — Granola gap
8. **Live translation verb** — Apple Intelligence parity
9. **BYOK TTS** — ElevenLabs / OpenAI / Apple voices selectable
10. **ChatGPT voice refugees** — Jan 15 2026 ChatGPT macOS voice retires; marketing opportunity

## Roadmap synthesis (Product Manager agent)

Top priority for the next month: **"Crash-free wake loop + first-run installer"** (Polish P0 pair). Justification: until install + uptime are boring, no expansion feature retains users from GitHub clones. Polish compounds.

See the Product Manager agent's full Q3-Q4 2026 table in the original research transcript.

## Sources

All findings come from 12 parallel research agents run on 2026-05-17 against `kdv1995/jarvis` main branch. The agents covered: Backend Architect, Frontend Developer, UX Researcher, UI Designer, Accessibility Auditor, Security Engineer, Code Reviewer, DevOps Automator, API Tester, Performance Benchmarker, Trend Researcher, Product Manager.
