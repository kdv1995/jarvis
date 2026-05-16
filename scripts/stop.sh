#!/usr/bin/env bash
#
# Stop every part of the Jarvis stack:
#   - The Jarvis app (mic listener + HUD)
#   - The clap daemon (background launchd service that wakes Jarvis on claps)
#   - The Whisper STT and Kokoro TTS local servers (if running as launchd)
#
# Idempotent — safe to run when nothing is running. Won't error.
#
# To start everything again later:
#   launchctl load ~/Library/LaunchAgents/com.jarvis.clap-daemon.plist
#   open -a Jarvis

set +e   # don't bail on pkill returning 1 when nothing matches

echo "→ Quitting Jarvis app"
osascript -e 'tell application "Jarvis" to quit' 2>/dev/null
pkill -f "/Applications/Jarvis.app/Contents/MacOS/jarvis" 2>/dev/null
pkill -f "target/release/jarvis" 2>/dev/null

echo "→ Stopping clap daemon (unloading launchd job so it can't auto-restart)"
launchctl unload ~/Library/LaunchAgents/com.jarvis.clap-daemon.plist 2>/dev/null
pkill -f "jarvis-clap-daemon" 2>/dev/null

# Optional: stop the local STT/TTS servers too. Comment out if you want
# them to keep running (they're cheap when idle, and warm startup is faster).
echo "→ Stopping Whisper STT + Kokoro TTS servers"
launchctl unload ~/Library/LaunchAgents/com.jarvis.whisper-stt.plist 2>/dev/null
launchctl unload ~/Library/LaunchAgents/com.jarvis.kokoro-tts.plist 2>/dev/null
pkill -f "whisper-stt/server.py" 2>/dev/null
pkill -f "kokoro-tts/server.py" 2>/dev/null

sleep 1

echo ""
echo "=== still running (should be empty) ==="
ps -ax -o pid,command \
  | grep -iE "jarvis|whisper-stt/server|kokoro-tts/server" \
  | grep -v grep \
  | grep -v "stop.sh" \
  || echo "(nothing — fully stopped)"
