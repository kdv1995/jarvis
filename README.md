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

```bash
git clone https://github.com/kdv1995/jarvis.git
cd jarvis

# 1. Configure
cp .env.example .env
# Edit .env: add ELEVENLABS_API_KEY if you want ElevenLabs fallback (optional)

# 2. Frontend deps
npm install

# 3. Python services (one terminal each, leave running)
cd whisper-stt && python -m venv .venv && source .venv/bin/activate && pip install -r requirements.txt && python server.py
cd ../kokoro-tts && python -m venv .venv && source .venv/bin/activate && pip install -r requirements.txt && python server.py

# 4. Stable code-signing cert (one-time, asks for sudo once — keeps TCC permissions across rebuilds)
./scripts/create-codesign-cert.sh

# 5. Build + install Jarvis.app to /Applications and launch
./scripts/install.sh
```

On first launch, macOS asks for:
- **Microphone** — needed for STT
- **Accessibility** — needed for AppleScript window control
- **Automation → Terminal** — needed for dictation passthrough

Grant all three. With the signing cert in place, you grant them **once** — every future `./scripts/install.sh` keeps them.

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

**HUD invisible after install** → run `./node_modules/.bin/tauri build --bundles app` (NOT `cargo build`) so frontend assets get embedded into the binary.

**TCC permissions re-prompt every install** → you skipped `create-codesign-cert.sh`. Run it once, then re-install.

**Mic captures background noise as gibberish** → check input volume in System Settings → Sound → Input. Below 30% mic level produces unstable STT.

**`claude` CLI hangs** → the pipeline has a 45-second watchdog that auto-respawns the session. Check `~/.jarvis/journal.jsonl` for conversation state.

## Privacy

Everything except optional ElevenLabs fallback runs locally. Conversation history is appended to `~/.jarvis/journal.jsonl` on your machine and never uploaded. `claude` CLI sends prompts to Anthropic per its own terms. No telemetry from this project.

## Status

Personal hobby project. No support guarantees. PRs welcome but expect slow review.

## License

MIT (or whatever you prefer to add — currently no LICENSE file).
