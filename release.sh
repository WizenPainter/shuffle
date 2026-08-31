#!/bin/bash
# LOCAL / emergency release path. The CANONICAL release is now the CI workflow
# (.github/workflows/release.yml): push a tag `vX.Y.Z` and GitHub builds,
# notarizes, and — crucially — records a build-provenance attestation that ties
# the DMG to the source commit. DMGs built here locally are signed + notarized
# but NOT attested, so users can't `gh attestation verify` them. Prefer tagging.
#
# Cut a new Shuffle release and publish it to GitHub as the "latest" release,
# so the website's fixed download link always serves the newest version.
#
# It builds a signed + notarized DMG, then creates/updates the GitHub release
# tagged v<version> (version comes from Cargo.toml) and uploads the DMG under
# BOTH a versioned name (Shuffle-<version>.dmg) and a STABLE name (Shuffle.dmg).
#
# The stable name is what makes the website link never change:
#   https://github.com/WizenPainter/shuffle/releases/latest/download/Shuffle.dmg
#
# Prerequisites (one-time): notarytool profile stored (see make_dmg.sh), and
# `gh auth login` done. BEFORE running: bump `version` in Cargo.toml, then
# commit and push your changes so the release tags the right commit.

set -e
cd "$(dirname "$0")"

REPO="WizenPainter/shuffle"
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
TAG="v$VERSION"
DMG="Shuffle-$VERSION.dmg"
STABLE="Shuffle.dmg"

# Warn if the current commit isn't pushed (the release would tag the wrong state).
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "WARNING: you have uncommitted changes. Commit + push before releasing so"
    echo "         the $TAG tag points at the code you're shipping."
fi

echo "==> Building signed + notarized $DMG (universal: arm64 + x86_64)"
# Bake the commit into the binary so Settings shows "Version X (sha)" —
# otherwise released builds read "(dev)" and can't be told apart.
export SHUFFLE_BUILD_SHA="$(git rev-parse --short HEAD)"
# Build both slices so the release runs on Apple Silicon and Intel Macs;
# make_app.sh lipos them into one universal binary.
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
SHUFFLE_SIGN_ID="__force_adhoc__" ./make_app.sh   # make_dmg re-signs with Developer ID
./make_dmg.sh

# A stable-named copy so /releases/latest/download/Shuffle.dmg always resolves.
cp "$DMG" "$STABLE"

echo "==> Publishing GitHub release $TAG"
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    echo "    Release $TAG already exists — updating its assets."
    gh release upload "$TAG" "$DMG" "$STABLE" --repo "$REPO" --clobber
    gh release edit "$TAG" --repo "$REPO" --latest
else
    gh release create "$TAG" "$DMG" "$STABLE" \
        --repo "$REPO" \
        --title "Shuffle $VERSION" \
        --notes "Shuffle $VERSION" \
        --latest
fi

echo
echo "Done. Website download link (never changes):"
echo "  https://github.com/$REPO/releases/latest/download/$STABLE"
echo "Release page:"
echo "  https://github.com/$REPO/releases/latest"
