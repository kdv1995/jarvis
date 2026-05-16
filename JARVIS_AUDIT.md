# Jarvis Audit

Date: 2026-05-15

## Confirmed Current State

- App shape: Tauri 2 + Vite + TypeScript frontend, Rust backend, local Python services for STT/TTS, and a separate Rust clap wake daemon.
- Primary brain path: `src-tauri/src/agent.rs` runs a persistent `claude -p --input-format stream-json --output-format stream-json` session.
- Local fast answer path: pure knowledge questions go to Ollama `llama3.2:3b` on `127.0.0.1:11434`.
- STT path: local faster-whisper on `127.0.0.1:11436`, falling back to ElevenLabs Scribe if configured.
- TTS path: local Kokoro on `127.0.0.1:11435`, played through `afplay`.
- Fast action path: simple actions such as open/close apps, search, time/date, media, and "ask claude/codex ..." bypass the LLM brain and use Rust + AppleScript/shell.
- Local services checked live:
  - Ollama `:11434`: listening, has `llama3.2:3b`.
  - Kokoro `:11435`: `/health` returned `ok`.
  - Whisper `:11436`: `/health` returned `ok`.
- CLI auth checked live:
  - `codex`: installed at `/opt/homebrew/bin/codex`, version `codex-cli 0.128.0`, logged in using ChatGPT.
  - `claude`: installed at `/Users/user/.local/bin/claude` and `/opt/homebrew/bin/claude`, version `2.1.142`, but `claude auth status --text` returned `Not logged in`.
  - Current shell env has no `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, or `OPENAI_API_KEY`.

## Important Findings

1. Claude is hardcoded as the real brain, but Claude auth is currently not active.
   - Result: `agent::prewarm()` and real `run_claude_streaming()` calls are likely to fail unless the bundled app receives credentials not visible in the shell.
   - Immediate fix: run `claude auth login`, or set the correct Claude auth/token path.

2. Codex subscription is available, but Jarvis only uses Codex by pasting into `Codex.app`.
   - Current code supports `"ask codex ..."` as a GUI handoff, not as a backend brain.
   - Codex CLI supports `codex exec --json`, so it can be added as a second backend provider.
   - Unlike Claude's current implementation, Codex CLI does not expose the same persistent stdin stream flow in the checked help output. Expect higher cold-start latency unless using a long-running service mode.

3. The code had stale STT/TTS configuration assumptions.
   - `.env.example`, `tts_voice()`, and comments still described ElevenLabs/macOS `say` TTS even though runtime uses Kokoro only.
   - Fixed in this audit pass.

4. ElevenLabs config unnecessarily required `ELEVENLABS_VOICE_ID`.
   - Runtime only needs ElevenLabs for STT fallback.
   - Fixed: `ElevenLabsConfig::from_env()` now requires only `ELEVENLABS_API_KEY`.

5. `README.md` is still the default Tauri template.
   - This is a real maintainability issue. There is no current runbook for starting Kokoro, Whisper, Ollama, Tauri, or the clap daemon.

6. Repository hygiene is weak.
   - This directory is not a git repository.
   - Build artifacts are checked into the working tree: `src-tauri/target` is about `9.1G`, root is about `11G`, plus `dist/`, `node_modules/`, and `clap-daemon/target`.
   - If this is meant to be developed as a project, initialize git and add a strict `.gitignore`.

7. Security posture is powerful but risky.
   - Claude is spawned with `--permission-mode bypassPermissions`.
   - The voice prompt tells the agent to run shell/AppleScript actions without refusal.
   - This matches the "Jarvis controls my Mac" goal, but it needs a command risk classifier before enabling a Codex backend with similar powers.

8. Speed work already has good foundations.
   - Persistent Claude sessions avoid per-command CLI cold start.
   - Fast-path router avoids LLM calls for common commands.
   - Local STT/TTS services are warm and local.
   - Main remaining latency is STT end-of-speech gating, cloud/CLI brain time, and TTS generation/playback.

## Recommended Provider Architecture

Add a small provider layer instead of wiring Codex directly into `pipeline.rs`.

Suggested env:

```sh
JARVIS_BRAIN_PROVIDER=auto      # auto | claude | codex | local
JARVIS_CLAUDE_MODEL=haiku
JARVIS_CODEX_MODEL=             # empty means Codex default
JARVIS_CODEX_MODE=exec          # exec first; server later if stable
JARVIS_DANGEROUS_ACTIONS=true   # required for OS-control commands
```

Routing:

- Fast-path Rust actions still run first.
- Knowledge-only questions still try local Ollama first.
- Tool/action commands route to:
  - Claude when Claude auth is healthy and low latency is important.
  - Codex when `JARVIS_BRAIN_PROVIDER=codex`, Claude auth is missing, or the user says "use codex".
  - Fail with a useful spoken error if neither provider is authenticated.

Implementation notes:

- Keep the current persistent Claude session because it is the lowest-latency CLI path.
- Add Codex as an adapter around `codex exec --json --skip-git-repo-check --ephemeral`.
- For safety, start Codex in read-only/sandboxed mode for question answering and require an explicit env flag before allowing full Mac control.
- Parse JSONL events to emit HUD trace lines, but do not expect Claude-compatible event names.

## Speed Recommendations

Highest impact:

1. Add a provider health preflight at app startup.
   - Check Claude auth, Codex login, Ollama health, Whisper health, Kokoro health.
   - Surface "Claude unavailable, using Codex" in logs/HUD instead of failing after speech.

2. Make VAD timings configurable.
   - Current hangover is 400 ms and min speech is 250 ms.
   - Expose `JARVIS_VAD_HANGOVER_MS`, `JARVIS_VAD_MIN_SPEECH_MS`, and maybe `JARVIS_AWAIT_TIMEOUT_MS`.
   - For short commands, 250-300 ms hangover may feel faster.

3. Add speculative TTS for confirmations.
   - For fast actions, speak a canned confirmation immediately after the command succeeds.
   - Already mostly done; extend it to more common actions.

4. Add a direct "dictate to Codex/Claude app" mode separately from "agent brain" mode.
   - Current GUI paste path is useful, but it is not the same as using a subscription as Jarvis's backend.
   - Keep both concepts separate in command wording and code.

5. Consider Codex `exec-server` only after checking stability.
   - `codex exec` works as the documented scripting entrypoint, but a long-running server would be better for latency if it exposes a stable local protocol.

## Validation Performed

- `npm run build`: passed.
- `cargo check --manifest-path src-tauri/Cargo.toml`: passed with an Xcode/macOS SDK warning only.
- `npm audit --omit=dev`: passed, 0 vulnerabilities.
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`: initially failed on a redundant closure; fixed.
- `cargo clippy --manifest-path clap-daemon/Cargo.toml -- -D warnings`: initially failed on manual range contains; fixed.

## External References Checked

- Anthropic Claude Code CLI reference: `claude -p`, `--input-format stream-json`, `--output-format stream-json`, `remote-control`, and `setup-token`.
  - https://code.claude.com/docs/en/cli-reference
- Anthropic Claude Code authentication precedence and subscription token behavior.
  - https://code.claude.com/docs/en/team
- OpenAI Codex CLI page: Codex included with ChatGPT Plus/Pro/Business/Edu/Enterprise and supports local CLI use.
  - https://developers.openai.com/codex/cli
- Codex exec JSONL scripting reference.
  - https://www.mintlify.com/openai/codex/cli/exec
