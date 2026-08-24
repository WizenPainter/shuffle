#!/bin/bash
# Assemble Shuffle.app from the release binary + AppIcon.icns.
# Run `cargo build --release` first.
set -e
cd "$(dirname "$0")"

APP="Shuffle.app"
# Version from Cargo.toml so the bundle matches the crate.
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Prefer a universal binary (arm64 + x86_64) so Shuffle runs on both Apple
# Silicon and Intel Macs. release.sh builds both per-target binaries; if only
# the default host build exists (e.g. a quick local dev build) fall back to it.
ARM_BIN="target/aarch64-apple-darwin/release/shuffle"
X86_BIN="target/x86_64-apple-darwin/release/shuffle"
if [ -f "$ARM_BIN" ] && [ -f "$X86_BIN" ]; then
    lipo -create "$ARM_BIN" "$X86_BIN" -output "$APP/Contents/MacOS/shuffle"
    echo "Universal binary: $(lipo -archs "$APP/Contents/MacOS/shuffle")"
else
    cp target/release/shuffle "$APP/Contents/MacOS/shuffle"
    echo "Single-arch binary: $(lipo -archs "$APP/Contents/MacOS/shuffle")"
fi
cp AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

# Compile the native "Remove Background" helper (Vision framework) next to the
# main binary, universal so it also runs on Intel. The Vision API it uses
# (VNGenerateForegroundInstanceMaskRequest) needs macOS 14, so both slices
# target macos14. Best-effort: if swiftc is missing or a slice fails, the
# feature just won't appear.
if command -v swiftc >/dev/null 2>&1; then
    if swiftc -O -target arm64-apple-macos14 removebg.swift -o "$APP/Contents/MacOS/removebg.arm64" 2>/dev/null \
        && swiftc -O -target x86_64-apple-macos14 removebg.swift -o "$APP/Contents/MacOS/removebg.x86" 2>/dev/null; then
        lipo -create "$APP/Contents/MacOS/removebg.arm64" "$APP/Contents/MacOS/removebg.x86" \
            -output "$APP/Contents/MacOS/removebg"
        rm -f "$APP/Contents/MacOS/removebg.arm64" "$APP/Contents/MacOS/removebg.x86"
        echo "Built removebg helper: $(lipo -archs "$APP/Contents/MacOS/removebg")"
    elif swiftc -O removebg.swift -o "$APP/Contents/MacOS/removebg" 2>/dev/null; then
        echo "Built removebg helper (host arch only)"
    else
        echo "WARNING: removebg helper failed to compile (Remove Background disabled)"
    fi

    # "cloudctl" helper: download / evict cloud files (iCloud + File Provider).
    # Universal too. Best-effort — if it fails, the cloud actions just hide.
    if swiftc -O -target arm64-apple-macos12 cloudctl.swift -o "$APP/Contents/MacOS/cloudctl.arm64" 2>/dev/null \
        && swiftc -O -target x86_64-apple-macos12 cloudctl.swift -o "$APP/Contents/MacOS/cloudctl.x86" 2>/dev/null; then
        lipo -create "$APP/Contents/MacOS/cloudctl.arm64" "$APP/Contents/MacOS/cloudctl.x86" \
            -output "$APP/Contents/MacOS/cloudctl"
        rm -f "$APP/Contents/MacOS/cloudctl.arm64" "$APP/Contents/MacOS/cloudctl.x86"
        echo "Built cloudctl helper: $(lipo -archs "$APP/Contents/MacOS/cloudctl")"
    elif swiftc -O cloudctl.swift -o "$APP/Contents/MacOS/cloudctl" 2>/dev/null; then
        echo "Built cloudctl helper (host arch only)"
    else
        echo "WARNING: cloudctl helper failed to compile (cloud download/evict disabled)"
    fi
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>Shuffle</string>
    <key>CFBundleDisplayName</key>     <string>Shuffle</string>
    <key>CFBundleExecutable</key>      <string>shuffle</string>
    <key>CFBundleIdentifier</key>      <string>com.shuffle.app</string>
    <key>CFBundleIconFile</key>        <string>AppIcon</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleShortVersionString</key> <string>$VERSION</string>
    <key>CFBundleVersion</key>         <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>  <string>12.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>LSApplicationCategoryType</key> <string>public.app-category.utilities</string>
</dict>
</plist>
PLIST

# Code-sign with a STABLE identity (must be the last step — signing seals the
# bundle). A stable signing identity + fixed bundle id is what lets macOS
# remember granted folder/privacy permissions across launches and rebuilds,
# instead of re-prompting every run (which ad-hoc signing causes).
SIGN_ID="${SHUFFLE_SIGN_ID:-Apple Development: Jaime Guzman (7UB4C2P6D6)}"
if security find-identity -v -p codesigning 2>/dev/null | grep -q "$SIGN_ID"; then
    codesign --force --sign "$SIGN_ID" --identifier com.shuffle.app "$APP"
    echo "Signed with: $SIGN_ID"
else
    echo "WARNING: signing identity not found; falling back to ad-hoc (permissions will re-prompt)."
    codesign --force --sign - --identifier com.shuffle.app "$APP"
fi
codesign -dv --verbose=2 "$APP" 2>&1 | grep -iE 'Identifier|Authority|Signature' | head -3
echo "Built $APP"
