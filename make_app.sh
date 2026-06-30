#!/bin/bash
# Assemble Shuffle.app from the release binary + AppIcon.icns.
# Run `cargo build --release` first.
set -e
cd "$(dirname "$0")"

APP="Shuffle.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/shuffle "$APP/Contents/MacOS/shuffle"
cp AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

# Compile the native "Remove Background" helper (Vision framework) next to the
# main binary. Best-effort: if swiftc is missing the feature just won't appear.
if command -v swiftc >/dev/null 2>&1; then
    swiftc -O removebg.swift -o "$APP/Contents/MacOS/removebg" 2>/dev/null \
        && echo "Built removebg helper" \
        || echo "WARNING: removebg helper failed to compile (Remove Background disabled)"
fi

cat > "$APP/Contents/Info.plist" <<'PLIST'
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
    <key>CFBundleShortVersionString</key> <string>0.1.0</string>
    <key>CFBundleVersion</key>         <string>1</string>
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
