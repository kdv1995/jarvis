#!/usr/bin/env bash
#
# Build → sign → install Jarvis into /Applications.
#
# Run this after any code change. It will:
#   1. Build the release binary (`cargo build --release`)
#   2. Take a clean copy of the existing Tauri-bundled .app structure
#      (which already has the icon, Info.plist, Resources/portrait.png, etc.)
#   3. Swap in the freshly built binary
#   4. Re-sign with the "Jarvis Code Signing" cert (stable across rebuilds!)
#   5. Atomically replace /Applications/Jarvis.app
#   6. Re-launch via `open -a Jarvis`
#
# Because we always sign with the same cert, macOS TCC keeps every permission
# you've granted (Mic, Accessibility, Automation→Terminal, etc.) — you only
# grant them ONCE, the first time you launch the freshly-signed app.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEYCHAIN_PATH="$HOME/Library/Keychains/jarvis-codesign.keychain-db"
CERT_NAME="Jarvis Code Signing"
BUNDLE_ID="com.jarvis.hud"

BUILT_BUNDLE="$REPO_ROOT/src-tauri/target/release/bundle/macos/Jarvis.app"
BUILT_BINARY="$REPO_ROOT/src-tauri/target/release/jarvis"
INSTALLED_APP="/Applications/Jarvis.app"

cd "$REPO_ROOT/src-tauri"

# Pick a signing identity. If the self-signed cert is available, use it for
# full TCC stability across rebuilds. Otherwise fall back to ad-hoc + an
# explicit identifier — same `com.jarvis.hud` for every rebuild — which gives
# partial stability (Accessibility/Automation usually persist; Microphone
# sometimes re-prompts because TCC keys mic grants tighter to cdhash for
# ad-hoc signatures).
USE_CERT=0
if security find-identity -v -p codesigning "$KEYCHAIN_PATH" 2>/dev/null \
        | grep -q "$CERT_NAME"; then
    USE_CERT=1
    echo "→ Signing identity: $CERT_NAME (cert-based, full TCC stability)"
else
    echo "→ Signing identity: ad-hoc + stable identifier $BUNDLE_ID"
    echo "  (Run scripts/create-codesign-cert.sh once for full TCC stability across rebuilds.)"
fi

# Sanity: a previously-built .app bundle structure exists?
# (We need its Info.plist, Resources, icon — the Rust binary alone isn't an app.)
if [ ! -d "$BUILT_BUNDLE" ]; then
    echo "✗ No bundle skeleton at $BUILT_BUNDLE" >&2
    echo "  Need to do a one-time 'cargo tauri build' to produce the .app shell." >&2
    echo "  Install the CLI with: cargo install tauri-cli --version '^2.0'" >&2
    exit 1
fi

echo "→ Building release binary"
cargo build --release

if [ ! -f "$BUILT_BINARY" ]; then
    echo "✗ Build did not produce $BUILT_BINARY" >&2
    exit 1
fi

# Stop running Jarvis so we can replace the bundle (TCC dislikes hot-swaps).
echo "→ Stopping running Jarvis"
pkill -f "/Applications/Jarvis.app/Contents/MacOS/jarvis" 2>/dev/null || true
pkill -f "target/release/jarvis" 2>/dev/null || true
osascript -e 'tell application "Jarvis" to quit' 2>/dev/null || true
sleep 1

echo "→ Swapping in freshly built binary"
cp -f "$BUILT_BINARY" "$BUILT_BUNDLE/Contents/MacOS/jarvis"

echo "→ Signing the bundle"
# --force: replace any existing signature
# --deep:  sign all nested code (frameworks, helpers)
# --identifier: pin the signed identifier to com.jarvis.hud so TCC's match key
#               is identity-based, not random-linker-ad-hoc-based.
if [ "$USE_CERT" = "1" ]; then
    codesign --force --deep \
        --sign "$CERT_NAME" \
        --keychain "$KEYCHAIN_PATH" \
        --identifier "$BUNDLE_ID" \
        "$BUILT_BUNDLE"
else
    codesign --force --deep \
        --sign - \
        --identifier "$BUNDLE_ID" \
        "$BUILT_BUNDLE"
fi

# Verify the signature looks right
echo "→ Verifying signature"
codesign -dvv "$BUILT_BUNDLE" 2>&1 | grep -E "Identifier|Authority|TeamIdentifier|Signature"

echo "→ Installing to $INSTALLED_APP"
rm -rf "$INSTALLED_APP"
cp -R "$BUILT_BUNDLE" "$INSTALLED_APP"

echo "→ Launching"
open -a Jarvis

echo ""
echo "✓ Installed. Signature identifier=$BUNDLE_ID, anchored to $CERT_NAME."
echo "  All TCC permissions granted to /Applications/Jarvis.app persist"
echo "  across future runs of this script."
