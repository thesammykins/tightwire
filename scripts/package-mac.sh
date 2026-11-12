#!/usr/bin/env bash
# Builds dist/Tightwire.dmg: Tightwire.app with the pinned Longcable helper
# (vendor/longcable, built from source) embedded under
# Contents/Resources/longcable, signed with a Developer ID Application
# identity when one exists (SIGN_IDENTITY=auto), else ad-hoc. Never notarized,
# nothing uploaded. DIST, BUNDLE_ID and SIGN_IDENTITY: see mac/scripts/bundle.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
DIST="${DIST:-$ROOT/dist}"
export DIST
git submodule update --init vendor/longcable
swift build -c release --package-path vendor/longcable --product longcable-server 2>&1 | tail -1
LC_BIN="$(swift build -c release --package-path vendor/longcable --show-bin-path)/longcable-server"

PAYLOAD="$DIST/.payload"
rm -rf "$PAYLOAD"
mkdir -p "$PAYLOAD/longcable/bin" "$PAYLOAD/longcable/scripts"
cp "$LC_BIN" "$PAYLOAD/longcable/bin/"
cp vendor/longcable/scripts/install-mac.sh vendor/longcable/scripts/uninstall-mac.sh "$PAYLOAD/longcable/scripts/"
cp vendor/longcable/LICENSE "$PAYLOAD/longcable/LICENSE"
LC_REV="$(git -C vendor/longcable rev-parse HEAD)"
printf 'Longcable (GPL-3.0-or-later), https://github.com/Sapd/longcable\nBuilt unmodified from %s\nSource: vendor/longcable in https://github.com/thesammykins/tightwire\n' "$LC_REV" >"$PAYLOAD/longcable/README.txt"

EXTRA_PAYLOAD_DIR="$PAYLOAD" SIGN_IDENTITY="${SIGN_IDENTITY:-auto}" bash mac/scripts/bundle.sh
rm -rf "$PAYLOAD"

STAGE="$DIST/.dmg"
rm -rf "$STAGE" "$DIST/Tightwire.dmg"
mkdir -p "$STAGE"
cp -R "$DIST/Tightwire.app" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname Tightwire -srcfolder "$STAGE" -fs HFS+ -format UDZO "$DIST/Tightwire.dmg"
rm -rf "$STAGE"
codesign -dv "$DIST/Tightwire.app" 2>&1 | grep -E "^(Authority|Signature|Identifier|TeamIdentifier)=" | head -4
ls -l "$DIST/Tightwire.dmg"
