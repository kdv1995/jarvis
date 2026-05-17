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

## Voice commands (full vocabulary)

All commands fire after the wake chime. Most run in ~20 ms (no LLM), the rest go to claude CLI.

### Apps & focus
| Phrase | Action |
|---|---|
| "open Safari" / "launch Terminal" / "start VS Code" | Open the named app |
| "close Chrome" / "quit Spotify" | Quit the named app |
| "ask claude [prompt]" / "tell codex [prompt]" | Open Claude/Codex desktop app + paste prompt |

### Files
| Phrase | Action |
|---|---|
| "find report" / "find file resume.pdf" | Spotlight search, speak top hit |
| "open file my-presentation" | Spotlight + open the first match |
| "create folder Q4 reports" | Make new folder on ~/Desktop |
| "new folder Bills in downloads" | Make folder in ~/Downloads (or "documents") |
| "move resume.pdf to Downloads" | Finder-move to Downloads/Desktop/Documents/Trash |

### Windows (acts on frontmost app)
| Phrase | Action |
|---|---|
| "snap left" / "right half" / "top half" / "bottom half" | Snap to half-screen |
| "maximize" / "fill the screen" | Fill visible frame |
| "center window" | 70%×75% centered |
| "minimize" | Cmd-M the front window |
| "minimize all but this" / "hide everything" | Hide all other apps |
| "show desktop" | Mission Control: Show Desktop |
| "next window" / "previous window" | Cycle within the current app |

### Browser
| Phrase | Action |
|---|---|
| "go to github.com" / "open google.com" / "visit anthropic.ai" | Open URL in default browser |
| "github dot com" (after "go to") | STT-friendly — "dot" → "." |
| "new tab" / "close tab" / "next tab" / "previous tab" | Tab management |
| "reopen tab" | Cmd-Shift-T |
| "back" / "forward" / "reload" / "hard reload" | Navigation |
| "scroll to top" / "scroll to bottom" | Page-anchor jumps |

### System
| Phrase | Action |
|---|---|
| "volume up/down" / "mute" / "unmute" | Sound output (10%) |
| "brightness up/down" / "brighter" / "dimmer" | Display brightness (3 ticks) |
| "wifi on" / "wifi off" | Toggle Wi-Fi (via `networksetup`) |
| "bluetooth on/off" | Toggle Bluetooth (needs `brew install blueutil`) |
| "do not disturb on/off" / "focus on" | Toggle DND (needs Apple Shortcut named "Turn On/Off Do Not Disturb") |
| "screenshot" / "take a screenshot" | Save to ~/Desktop |
| "empty the trash" | Finder empty trash |
| "lock the screen" / "go to sleep" | Lock or sleep Mac |

### Media (Spotify-first)
| Phrase | Action |
|---|---|
| "play" / "pause" / "next" / "previous" | Spotify control |

### Search
| Phrase | Action |
|---|---|
| "search for X" / "google X" / "look up X" | Google search |
| "search youtube for X" / "youtube X" | YouTube search |

### Notes & Reminders
| Phrase | Action |
|---|---|
| "note that I need to call mom" | New Note in Notes.app |
| "make a note budget Q4 ideas" | Same |
| "remind me to call mom tomorrow" | New Reminder with natural-time clause |
| "remind me to check the oven in 30 minutes" | Same — Reminders.app parses the time |
| "add milk to shopping list" | Append to existing Reminders list (or default) |

### System info (voice queries)
| Phrase | Answer |
|---|---|
| "what's my battery?" / "battery status" | "Battery is at 87%, charging, 4h 22m remaining" |
| "cpu usage" / "memory" / "disk free" | Live metric from system snapshot |
| "wifi" / "network" | Connected SSID + signal strength |
| "uptime" / "how long has the Mac been on?" | Formatted uptime |

### Time & date
| Phrase | Answer |
|---|---|
| "what time is it" / "tell me the time" | Spoken time |
| "what's the date" / "what day is it" | Spoken date |

### Code dictation (special)
| Phrase | Action |
|---|---|
| "claude code create a Next.js site" | Types into frontmost Terminal claude session |
| "code refactor this component" | Same — shorter prefix |
| "claude add typescript types" | Same |

Anything not matched above falls through to the LLM brain (Ollama for knowledge, claude CLI for complex tasks).

## Skills (YAML workflows)

A *skill* is a named recipe with trigger phrases and an ordered list of steps. When you speak a trigger phrase, Jarvis runs the whole workflow.

### Shipped skills

These come pre-loaded with the app:

| Trigger phrases | What it does |
|---|---|
| "start work mode" / "work mode on" | DND on, open Cursor + Terminal, close Slack/Discord/Spotify |
| "end of day" / "wind down" / "wrap up" | Close all communication apps, DND off, lock screen |
| "meeting prep" / "i have a meeting" | DND on, mute, close noise, open Zoom |
| "deep focus" / "focus mode" | DND, hide everything but Cursor, brightness up |
| "break time" / "take a break" | Show desktop, dim screen, open Spotify, play |

### Meta commands

- "list skills" — speak the names of all loaded skills
- "reload skills" — re-scan `~/.jarvis/skills/` after editing

### Writing your own

Drop a `.yaml` file in `~/.jarvis/skills/`. Live-reload picks it up within a second.

```yaml
name: my-skill            # required, unique
triggers:                  # required, at least one
  - phrase one
  - phrase two
description: optional      # shown by "list skills"
on_error: continue         # or "stop" — default: continue
steps:
  - say: "Starting"        # speak via TTS
  - command: "open Cursor" # run any voice command from this README
  - wait: 500              # milliseconds
  - shell: "echo hi >> /tmp/jarvis.log"
  - applescript: |
      tell application "Finder" to activate
  - say: "Done"
```

Step types:
- `say:` — TTS announcement
- `command:` — re-route through Jarvis's fast-path table (any voice verb from the tables above)
- `wait:` — sleep N milliseconds (useful between launches)
- `shell:` — `/bin/sh -c <cmd>`, output not spoken
- `applescript:` — raw AppleScript snippet

User skills with the same `name:` as a bundled skill **override** the bundled version — copy a YAML out of `/Applications/Jarvis.app/Contents/Resources/skills/` and tweak it.

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
