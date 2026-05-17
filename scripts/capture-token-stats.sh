#!/usr/bin/env bash
# Capture token-usage stats at the scheduled time.
#
# Opens the Anthropic console usage page in the default browser, waits for
# it to load, then takes a screenshot of the full screen and saves it to
# the user's Desktop with a timestamp. Triggered by the LaunchAgent at
# ~/Library/LaunchAgents/com.jarvis.tokenscreenshot.plist.
#
# Side effects:
#   - opens / focuses default browser to console.anthropic.com
#   - writes ~/Desktop/jarvis-tokens-YYYY-MM-DD_HHMM.png
#   - writes /tmp/jarvis-tokencap.log (stdout + stderr)
#
# If the user isn't logged in, the screenshot captures the login page —
# best effort, not silent failure.

set -uo pipefail

LOG=/tmp/jarvis-tokencap.log
exec > >(tee -a "$LOG") 2>&1

echo ""
echo "─── $(date) ─── token capture firing"

# Open Anthropic console usage page so the screenshot has useful content.
# Default browser only — we don't force Safari/Chrome.
/usr/bin/open "https://console.anthropic.com/settings/usage" || true

# Give the page 6 seconds to load + render (Anthropic console SPA cold-loads
# in ~3-4 s on broadband).
sleep 6

# Build dated filename — easy to find, ordered chronologically.
TS=$(date '+%Y-%m-%d_%H%M')
OUT="$HOME/Desktop/jarvis-tokens-$TS.png"

# Full-screen capture without UI sound and without window-shadow capture
# overhead. -x silences the shutter sound (CI-friendly).
/usr/sbin/screencapture -x "$OUT"

if [ -f "$OUT" ]; then
    SIZE=$(stat -f%z "$OUT" 2>/dev/null || stat -c%s "$OUT" 2>/dev/null || echo "?")
    echo "✓ saved: $OUT ($SIZE bytes)"
else
    echo "✗ screencapture did not produce a file"
    exit 1
fi
