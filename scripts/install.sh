#!/usr/bin/env bash
#
# Build → sign → install Jarvis into /Applications.
#
# This is the single command a new user runs after `git clone`. It:
#   1. Builds the FULL Tauri bundle (`tauri build --bundles app`) — runs
#      `vite build` + Rust release build + bundle assembly in one shot,
#      so frontend changes (HTML/CSS/TS) are always embedded fresh.
#   2. Stops any running Jarvis (TCC dislikes hot-swaps).
#   3. Installs the bundle to /Applications/Jarvis.app.
#   4. Re-signs with the stable "Jarvis Code Signing" cert (if present) or
#      ad-hoc with a fixed identifier (com.jarvis.hud).
#   5. Launches via `open -a Jarvis`.
#
# Because the bundle identifier is stable across rebuilds, macOS TCC keeps
# every permission you've granted (Mic, Accessibility, Automation→Terminal).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEYCHAIN_PATH="$HOME/Library/Keychains/jarvis-codesign.keychain-db"
CERT_NAME="Jarvis Code Signing"
BUNDLE_ID="com.jarvis.hud"

BUILT_BUNDLE="$REPO_ROOT/src-tauri/target/release/bundle/macos/Jarvis.app"
INSTALLED_APP="/Applications/Jarvis.app"
TAURI_CLI="$REPO_ROOT/node_modules/.bin/tauri"

cd "$REPO_ROOT"

# Pick a signing identity. Prefer the self-signed cert for stable TCC; fall
# back to ad-hoc + fixed identifier (partial stability — mic may re-prompt
# because TCC keys mic grants to cdhash for ad-hoc signatures).
USE_CERT=0
if security find-identity -v -p codesigning "$KEYCHAIN_PATH" 2>/dev/null \
        | grep -q "$CERT_NAME"; then
    USE_CERT=1
    echo "→ Signing identity: $CERT_NAME (cert-based, full TCC stability)"
else
    echo "→ Signing identity: ad-hoc + stable identifier $BUNDLE_ID"
    echo "  (Run scripts/create-codesign-cert.sh once for full TCC stability across rebuilds.)"
fi

# Make sure deps are present so a fresh clone works without a separate step.
if [ ! -f "$TAURI_CLI" ]; then
    echo "→ Installing npm deps (first-time setup)"
    npm install
fi

echo "→ Building Tauri bundle (Vite + Rust + .app)"
"$TAURI_CLI" build --bundles app

if [ ! -d "$BUILT_BUNDLE" ]; then
    echo "✗ Build did not produce $BUILT_BUNDLE" >&2
    exit 1
fi

# Stop running Jarvis so we can replace the bundle cleanly.
echo "→ Stopping running Jarvis"
pkill -f "/Applications/Jarvis.app/Contents/MacOS/jarvis" 2>/dev/null || true
pkill -f "target/release/jarvis" 2>/dev/null || true
osascript -e 'tell application "Jarvis" to quit' 2>/dev/null || true
sleep 1

# Clear WebKit cache so the webview picks up the new frontend assets on
# first launch (stale cache was the cause of multiple "I don't see the
# button" bug reports during development).
rm -rf ~/Library/Caches/com.jarvis.hud ~/Library/WebKit/com.jarvis.hud 2>/dev/null || true

echo "→ Installing to $INSTALLED_APP"
rm -rf "$INSTALLED_APP"
cp -R "$BUILT_BUNDLE" "$INSTALLED_APP"

echo "→ Signing the installed bundle"
# --force: replace any existing signature
# --deep:  sign all nested code (frameworks, helpers)
# --identifier: pin to com.jarvis.hud so TCC keys by identity, not random cdhash.
if [ "$USE_CERT" = "1" ]; then
    codesign --force --deep \
        --sign "$CERT_NAME" \
        --keychain "$KEYCHAIN_PATH" \
        --identifier "$BUNDLE_ID" \
        "$INSTALLED_APP"
else
    codesign --force --deep \
        --sign - \
        --identifier "$BUNDLE_ID" \
        "$INSTALLED_APP"
fi

echo "→ Verifying signature"
codesign -dvv "$INSTALLED_APP" 2>&1 | grep -E "Identifier|Authority|TeamIdentifier|Signature"

echo "→ Launching"
open -a Jarvis

echo ""
echo "✓ Installed. Signature identifier=$BUNDLE_ID."
echo "  All TCC permissions granted to /Applications/Jarvis.app persist"
echo "  across future runs of this script."
