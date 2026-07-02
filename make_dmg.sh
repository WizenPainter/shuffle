#!/bin/bash
# Build a distributable Shuffle.dmg that non-technical users can install by
# dragging Shuffle.app into Applications. No Rust or build tools needed by them.
#
# Usage:
#   ./make_app.sh          # build + assemble Shuffle.app first
#   ./make_dmg.sh          # then wrap it in Shuffle.dmg
#
# For a SEAMLESS install (no Gatekeeper warning) the app is signed with a
# Developer ID certificate and notarized by Apple. The identity + notary profile
# default to Jaime Guzman's below; override with env vars if they change.
#
# One-time notary profile setup (do this ONCE before the first notarized build):
#   xcrun notarytool store-credentials shuffle-notary \
#       --apple-id you@example.com --team-id Z69U4AQSH3 --password <app-specific-pw>
#
# NOTE: team id Z69U4AQSH3 is the Developer ID team (matches the cert below), NOT
# the Apple Development team 7UB4C2P6D6.
#
# To force an UNSIGNED build (e.g. quick local test), run:  SHUFFLE_DEVID="" ./make_dmg.sh

set -e
cd "$(dirname "$0")"

APP="Shuffle.app"
VOL="Shuffle"
STAGING="dmg-staging"
# Version comes from Cargo.toml so each release DMG is uniquely named.
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
DMG="Shuffle-${VERSION}.dmg"

# Signing identity + notary profile (override via env; set SHUFFLE_DEVID="" to skip).
: "${SHUFFLE_DEVID=Developer ID Application: Jaime Guzman (Z69U4AQSH3)}"
: "${SHUFFLE_NOTARY_PROFILE=shuffle-notary}"

if [ ! -d "$APP" ]; then
    echo "ERROR: $APP not found. Run ./make_app.sh first."
    exit 1
fi

# --- Optional: sign with Developer ID + hardened runtime (needed to notarize) --
if [ -n "$SHUFFLE_DEVID" ]; then
    echo "Signing with: $SHUFFLE_DEVID"
    # Sign nested helpers first, then the app, with the hardened runtime.
    if [ -f "$APP/Contents/MacOS/removebg" ]; then
        codesign --force --options runtime --timestamp \
            --sign "$SHUFFLE_DEVID" "$APP/Contents/MacOS/removebg"
    fi
    codesign --force --options runtime --timestamp \
        --sign "$SHUFFLE_DEVID" --identifier com.shuffle.app "$APP"
    codesign --verify --deep --strict --verbose=2 "$APP"
else
    echo "NOTE: SHUFFLE_DEVID not set — building an unsigned/ad-hoc DMG."
    echo "      Users will see a Gatekeeper warning (WELCOME.txt explains the fix)."
fi

# --- Stage the drag-to-install layout ----------------------------------------
rm -rf "$STAGING" "$DMG"
mkdir -p "$STAGING"
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"

# A short note for users who hit Gatekeeper on an un-notarized build.
cat > "$STAGING/WELCOME.txt" <<'TXT'
Installing Shuffle
==================

1. Drag "Shuffle" onto the "Applications" folder in this window.
2. Open Applications and launch Shuffle.

If macOS says Shuffle "cannot be opened because Apple cannot check it":
  - Right-click (or Control-click) Shuffle in Applications.
  - Choose "Open", then click "Open" again in the dialog.
  You only need to do this once.
TXT

# --- Build the compressed DMG -------------------------------------------------
hdiutil create -volname "$VOL" -srcfolder "$STAGING" -ov -format UDZO "$DMG"
rm -rf "$STAGING"

# --- Optional: notarize + staple so there's NO Gatekeeper warning ------------
if [ -n "$SHUFFLE_DEVID" ] && [ -n "$SHUFFLE_NOTARY_PROFILE" ]; then
    echo "Submitting $DMG to Apple for notarization (this can take a few minutes)…"
    xcrun notarytool submit "$DMG" --keychain-profile "$SHUFFLE_NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
    echo "Notarized + stapled. This DMG installs with no warnings."
elif [ -n "$SHUFFLE_DEVID" ]; then
    echo "Signed but NOT notarized (set SHUFFLE_NOTARY_PROFILE to notarize)."
fi

echo "Built $DMG"
