# Jarvis

Voice-driven holographic AI assistant for macOS. Wake it with a clap or a hot-word, speak naturally, and a transparent click-through HUD overlay shows what it's doing in real-time. Routes your prompts through `claude` CLI for reasoning, local Whisper for STT, and local Kokoro (or ElevenLabs) for TTS.

![status](https://img.shields.io/badge/platform-macOS%2013%2B-blue) ![arch](https://img.shields.io/badge/arch-Apple%20Silicon-success) ![status](https://img.shields.io/badge/status-personal%20project-orange)

---

## What it does

- **Always-on listening**: clap your hands or say a wake word — Jarvis activates
- **Speak naturally**: ask questions, give commands, dictate prompts directly into a running Terminal `claude` session
- **Holographic HUD**: transparent, click-through overlay across your whole screen; portrait-displacement Three.js avatar that reacts to listening / thinking / speaking state
- **Local-first**: STT (Whisper) and TTS (Kokoro) run on your machine. ElevenLabs is optional fallback for both.
- **Smart routing**: fast actions (open apps, web search, time/date) skip the LLM. Knowledge questions go to a local Ollama `llama3.2:3b`. Everything else goes through `claude` CLI with persistent context across restarts.

## Requirements

| | |
|---|---|
| **OS** | macOS 13 Ventura or later |
| **CPU** | Apple Silicon (M1 / M2 / M3 / M4) — Intel not tested |
| **RAM** | 8 GB minimum, 16 GB recommended (local models live in memory) |
| **Disk** | ~12 GB (Rust build cache + models + node_modules) |
| **Mic** | Any input device macOS recognises |

You'll also need accounts/installs for:
- [**Rust**](https://rustup.rs) (1.78+) — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- [**Node.js**](https://nodejs.org) 20+
- [**Python**](https://python.org) 3.11+
- [**Claude CLI**](https://docs.anthropic.com/en/docs/agents/claude-code/overview) — `npm install -g @anthropic-ai/claude-code` and run `claude auth login` once
- [**Ollama**](https://ollama.com) (optional, for fast local answers) — `brew install ollama && ollama pull llama3.2:3b`
- **ElevenLabs API key** (optional, for premium STT/TTS fallback) — get from [elevenlabs.io](https://elevenlabs.io/app/settings/api-keys)

## Install

> **Important:** the order below matters. The first build *must* go through `tauri build` (step 5) because `scripts/install.sh` expects an existing `.app` skeleton — it only swaps in fresh binaries on subsequent rebuilds.

### 1. Clone and configure

```bash
git clone https://github.com/kdv1995/jarvis.git
cd jarvis

cp .env.example .env
# Open .env in your editor. ELEVENLABS_API_KEY is optional — leave blank to use
# only the local Whisper + Kokoro stack.
```

### 2. Install frontend deps

```bash
npm install     # installs Vite, Three.js, Tauri CLI (~110 MB)
```

### 3. Build the clap-wake daemon

```bash
cd clap-daemon
cargo build --release
cd ..
```

This produces `clap-daemon/target/release/jarvis-clap-daemon`, a separate small Rust binary the main app launches in the background for low-latency wake detection.

### 4. Start the Python STT and TTS servers

These must be **running before** you launch Jarvis (Jarvis talks to them over `127.0.0.1:11435` and `:11436`). Open two separate terminal tabs and leave each running:

**Tab A — Whisper STT (`http://127.0.0.1:11436`)**
```bash
cd whisper-stt
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python server.py
```
First run downloads the Whisper model (~1.5 GB) into `~/.cache/huggingface/`.

**Tab B — Kokoro TTS (`http://127.0.0.1:11435`)**
```bash
cd kokoro-tts
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python server.py
```
First run downloads the Kokoro voice models (~300 MB).

Quick sanity check from a third terminal — both should reply `ok`:
```bash
curl -s http://127.0.0.1:11436/health
curl -s http://127.0.0.1:11435/health
```

### 5. Create the stable code-signing cert (one-time)

```bash
./scripts/create-codesign-cert.sh   # asks for your sudo password ONCE
```

Without this, macOS will re-prompt you for Microphone, Accessibility, and Automation permissions on every rebuild. The cert is locally generated, valid for 10 years, and only trusted by your own machine — it never leaves your laptop.

### 6. First build: produce the .app bundle

```bash
./node_modules/.bin/tauri build --bundles app
```

This compiles Rust, builds the frontend (Vite → `dist/`), and assembles `src-tauri/target/release/bundle/macos/Jarvis.app` with the frontend embedded into the binary. Takes ~3–5 minutes on a clean M1.

### 7. Install + launch

```bash
./scripts/install.sh
```

The script copies the bundle to `/Applications/Jarvis.app`, signs it with your cert, kills any running instance, and launches via `open -a Jarvis`. Use this same script for every future rebuild — it's much faster than step 6.

### 8. Grant macOS permissions (first launch only)

When Jarvis starts, macOS shows three prompts in sequence:

- **Microphone** — for speech-to-text
- **Accessibility** — for AppleScript window control (focusing Terminal, etc.)
- **Automation → Terminal** — for dictation passthrough into running `claude` sessions

Click **Allow** on all three. Because the app is signed with your stable cert, you'll only see these prompts once — every future install keeps the grants.

### Verifying it works

After launch you should see:
- A transparent silver-blue holographic portrait overlay across your whole screen
- HUD panels in the corners (clock, signal info, audio spectrum)
- A "PROJECTING" badge at top-center

Clap once — the badge should change to "SCANNING" and you'll hear a wake chime. Speak a question. The badge will transition through "ANALYZING" → "TRANSMITTING" as Jarvis responds via the TTS engine.

If you don't see the HUD, see [Troubleshooting](#troubleshooting) below.

## Usage

| Action | How |
|---|---|
| Wake Jarvis | Clap once (or say wake word) |
| Ask a question | Just speak after the wake chime |
| Dictate into Terminal | Wake, then say "claude code [your prompt]" — the prompt is typed into the frontmost Terminal window's running `claude` session |
| Stop everything | `./scripts/stop.sh` |
| Toggle widget mode | Press `⌃⌥J` (Ctrl-Option-J) |

## Architecture

```
                ┌─────────────────────────────┐
                │  HUD overlay (transparent)  │  ← Tauri 2 + Vite + Three.js
                │  Three.js hologram + panels │
                └──────────────┬──────────────┘
                               │ Tauri events
                               ▼
┌────────────┐   audio   ┌──────────────────────────┐   answer   ┌──────────────┐
│ Microphone │──────────▶│  Rust pipeline (Tauri)   │───────────▶│  TTS engine  │
└────────────┘           │  - VAD                   │            │ Kokoro/11Labs│
                         │  - State machine         │            └──────┬───────┘
                         │  - Wake / dictation      │                   │ audio
                         └─┬────────┬───────────┬───┘                   ▼
                           │        │           │                  ┌────────┐
                           ▼        ▼           ▼                  │ afplay │
                       ┌────────┐ ┌─────────┐ ┌────────────┐       └────────┘
                       │ STT    │ │ Ollama  │ │ claude CLI │
                       │Whisper │ │llama3.2 │ │ (session)  │
                       └────────┘ └─────────┘ └────────────┘
```

Key files:
- `src-tauri/src/pipeline.rs` — main state machine
- `src-tauri/src/agent.rs` — persistent `claude -p` session with watchdog
- `src-tauri/src/stt.rs`, `src-tauri/src/tts.rs` — local-first with ElevenLabs fallback
- `src/hud/graph.ts` — Three.js portrait-displacement hologram
- `clap-daemon/` — separate Rust binary for low-latency clap detection
- `whisper-stt/`, `kokoro-tts/` — Python HTTP servers

## Troubleshooting

**HUD invisible after install** → run `./node_modules/.bin/tauri build --bundles app` (NOT `cargo build`) so frontend assets get embedded into the binary. Tauri 2 compiles `dist/` into the Rust binary via `tauri-build` macros — `cargo build --release` alone produces a binary without any frontend.

**TCC permissions re-prompt every install** → you skipped `create-codesign-cert.sh`. Run it once, then re-install. The ad-hoc fallback signing in `install.sh` works but keys mic grants to the binary's `cdhash`, which changes every build.

**Mic captures background noise as gibberish** → check input volume in System Settings → Sound → Input. Below 30% mic level produces unstable STT.

**`claude` CLI hangs** → the pipeline has a 45-second watchdog that auto-respawns the session. Check `~/.jarvis/journal.jsonl` for conversation state.

**`install.sh` fails with "No bundle skeleton"** → you haven't run step 6 (`tauri build --bundles app`) yet. The script needs an existing `Jarvis.app` skeleton to copy from before it can swap in a fresh binary.

**`python server.py` errors with "Module not found: faster_whisper" or "kokoro"** → you forgot to activate the venv. Run `source .venv/bin/activate` first, or re-run the `pip install` step inside the venv.

**Jarvis launches but never wakes on a clap** → check that `clap-daemon/target/release/jarvis-clap-daemon` exists (step 3). The main app spawns it as a subprocess; without it, only voice wake words work.

**ElevenLabs not used even though key is set** → check `.env` has no quotes around the value and is at the project root (not inside `src-tauri/`). Restart Jarvis after editing `.env` — env vars are read once at startup.

## Privacy

Everything except optional ElevenLabs fallback runs locally. Conversation history is appended to `~/.jarvis/journal.jsonl` on your machine and never uploaded. `claude` CLI sends prompts to Anthropic per its own terms. No telemetry from this project.

## Status

Personal hobby project. No support guarantees. PRs welcome but expect slow review.

## License

MIT (or whatever you prefer to add — currently no LICENSE file).
