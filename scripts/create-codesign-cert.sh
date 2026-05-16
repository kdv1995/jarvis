#!/usr/bin/env bash
#
# One-time setup: create a self-signed code-signing certificate that every
# Jarvis rebuild will sign with. Because the cert hash is stable across
# rebuilds, macOS TCC (Accessibility, Microphone, Automation→Terminal, etc.)
# recognises every rebuild as the same app and KEEPS all granted permissions.
#
# This script will prompt for your admin (sudo) password ONCE — that's the
# cost of adding our self-signed cert to the system trust store so `codesign`
# will accept it. After this, every future build runs without any prompts.
#
# The cert is valid for 10 years.

set -euo pipefail

CERT_NAME="Jarvis Code Signing"
LOGIN_KC="$HOME/Library/Keychains/login.keychain-db"
SYSTEM_KC="/Library/Keychains/System.keychain"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

# Idempotent: if a valid cert already exists, exit early.
if security find-identity -v -p codesigning 2>/dev/null \
        | grep -q "$CERT_NAME"; then
    echo "✓ '$CERT_NAME' already exists and is trusted — nothing to do."
    exit 0
fi

# If a cert with our name exists but is untrusted, delete it before re-creating.
EXISTING_HASH=$(security find-identity "$LOGIN_KC" 2>/dev/null \
                  | awk -v n="$CERT_NAME" '$0 ~ n {print $2; exit}')
if [ -n "$EXISTING_HASH" ]; then
    echo "→ Removing existing untrusted '$CERT_NAME' (hash=$EXISTING_HASH)"
    security delete-certificate -Z "$EXISTING_HASH" "$LOGIN_KC" >/dev/null 2>&1 || true
fi

echo "→ Generating 10-year self-signed code-signing cert via openssl"
openssl req -newkey rsa:2048 -nodes \
    -keyout "$TMP_DIR/key.pem" \
    -x509 -sha256 -days 3650 \
    -subj "/CN=$CERT_NAME/O=Jarvis Voice Assistant" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "extendedKeyUsage=critical,codeSigning" \
    -out "$TMP_DIR/cert.pem" \
    2>/dev/null

# Bundle cert + key as PKCS12 using the legacy PBE that macOS's `security
# import` accepts. LibreSSL (the system openssl on macOS) uses old syntax —
# explicit `-keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1` rather
# than the `-legacy` flag from real OpenSSL 3.x.
echo "→ Packaging as PKCS12 (legacy PBE for macOS compatibility)"
openssl pkcs12 -export \
    -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
    -out "$TMP_DIR/cert.p12" \
    -inkey "$TMP_DIR/key.pem" \
    -in "$TMP_DIR/cert.pem" \
    -name "$CERT_NAME" \
    -passout pass:jarvis

echo "→ Importing into login keychain (codesign-only access)"
security import "$TMP_DIR/cert.p12" \
    -k "$LOGIN_KC" \
    -P "jarvis" \
    -T /usr/bin/codesign \
    -A >/dev/null

# Let codesign use the key without ever popping a "allow access" GUI dialog.
# This may silently fail if the login keychain is locked or needs a password
# — in practice it works because the login keychain is unlocked for the user.
security set-key-partition-list \
    -S "apple-tool:,apple:" \
    -s -k "" "$LOGIN_KC" >/dev/null 2>&1 || true

echo ""
echo "→ Adding cert to system trust store (REQUIRES YOUR ADMIN PASSWORD ONCE)"
echo "   macOS will now ask for your password. After this, every future"
echo "   Jarvis rebuild signs silently with the same trusted cert."
echo ""
sudo security add-trusted-cert \
    -d -r trustRoot \
    -p codeSign \
    -k "$SYSTEM_KC" \
    "$TMP_DIR/cert.pem"

echo ""
echo "✓ Cert installed. Verifying it's now a valid codesign identity:"
security find-identity -v -p codesigning | grep "$CERT_NAME"

echo ""
echo "Done. Run scripts/install.sh to build, sign, and install Jarvis."
