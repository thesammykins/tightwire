#!/usr/bin/env bash
# Builds dist/Tightwire.app from the SwiftPM product and signs it.
#   SIGN_IDENTITY=-        ad-hoc (default)
#   SIGN_IDENTITY=auto     first "Developer ID Application" identity, else ad-hoc
#   SIGN_IDENTITY=<sha1>   that identity
#   BUNDLE_ID=<id>         bundle identifier (else mac/local.env, else the default below)
#   DIST=<dir>             output directory (default: dist/)
# Developer ID builds get the hardened runtime (+ audio-input entitlement) on
# every executable. Nothing is notarized or uploaded: Tightwire is built by
# whoever runs it, and an ad-hoc build is the normal case.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MAC="$ROOT/mac"
DIST="${DIST:-$ROOT/dist}"
APP="$DIST/Tightwire.app"
# One source of truth for the version: TightwireCore/BuildInfo.swift. No
# environment override: the app reports BuildInfo.version at run time, and
# Info.plist must say the same.
VERSION="$(sed -nE 's/.*static let version = "([^"]+)".*/\1/p' "$MAC/Sources/TightwireCore/BuildInfo.swift")"
[[ -n "$VERSION" ]] || { echo "error: no version in TightwireCore/BuildInfo.swift" >&2; exit 1; }
# Same commit stamp as the receiver's build.rs (+dirty with tracked changes).
COMMIT="$(git -C "$ROOT" rev-parse --short=10 HEAD 2>/dev/null || echo unknown)"
if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]]; then COMMIT="$COMMIT+dirty"; fi
# The bundle identifier is the app's identity for macOS: its defaults
# domain, privacy grants, Keychain access, login item and URL scheme owner all
# follow it. Upstream builds use the default; a builder who already runs a
# build under another identifier keeps it with BUNDLE_ID in the environment or
# in mac/local.env (gitignored, KEY=value lines), so an upgrade is not a new app.
LOCAL_ENV="$MAC/local.env"
if [[ -z "${BUNDLE_ID:-}" && -f "$LOCAL_ENV" ]]; then
  BUNDLE_ID="$(sed -nE 's/^[[:space:]]*BUNDLE_ID[[:space:]]*=[[:space:]]*"?([^"[:space:]]*)"?[[:space:]]*$/\1/p' "$LOCAL_ENV" | tail -1)"
fi
BUNDLE_ID="${BUNDLE_ID:-app.sammykins.tightwire}"
[[ "$BUNDLE_ID" =~ ^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$ ]] || { echo "error: BUNDLE_ID '$BUNDLE_ID' is not a reverse-DNS identifier" >&2; exit 1; }

swift build -c release --package-path "$MAC" --product Tightwire 2>&1 | grep -E "error|Compiling|Build complete" | tail -3
BIN="$(swift build -c release --package-path "$MAC" --show-bin-path)"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN/Tightwire" "$APP/Contents/MacOS/Tightwire"
cp "$ROOT/assets/icon/Tightwire.icns" "$APP/Contents/Resources/Tightwire.icns"
# Licence and third-party notices travel with every build.
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
bash "$ROOT/scripts/notices.sh" "$APP/Contents/Resources/THIRD-PARTY-NOTICES.txt" >/dev/null
# Extra payload (e.g. the handoff helper) is added by the caller via EXTRA_PAYLOAD_DIR.
if [[ -n "${EXTRA_PAYLOAD_DIR:-}" && -d "$EXTRA_PAYLOAD_DIR" ]]; then
  cp -R "$EXTRA_PAYLOAD_DIR"/. "$APP/Contents/Resources/"
fi

cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Tightwire</string>
  <key>CFBundleDisplayName</key><string>Tightwire</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>Tightwire</string>
  <key>CFBundleIconFile</key><string>Tightwire</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>TightwireCommit</key><string>$COMMIT</string>
  <key>NSHumanReadableCopyright</key><string>Copyright © Tightwire contributors. GPL-3.0-or-later.</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Tightwire sends the selected input (your microphone) to your paired PC.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Tightwire streams audio to the PC you paired with on your local network.</string>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>Tightwire control</string>
      <key>CFBundleURLSchemes</key><array><string>tightwire</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

IDENTITY="${SIGN_IDENTITY:--}"
if [[ "$IDENTITY" == auto ]]; then
  IDENTITY="$(security find-identity -v -p codesigning | sed -nE 's/^ *[0-9]+\) ([0-9A-F]{40}) "Developer ID Application:.*/\1/p' | head -1)"
  IDENTITY="${IDENTITY:--}"
fi
# Hardened runtime for every executable in Developer ID builds. Ad-hoc builds
# (what anyone building from source gets) are signed without it, as they
# always have been; nothing here is notarized, which is what needs it.
RUNTIME=()
[[ "$IDENTITY" != - ]] && RUNTIME=(--options runtime)
sign() { codesign --force --sign "$IDENTITY" --timestamp=none ${RUNTIME[@]+"${RUNTIME[@]}"} "$@"; }

# Nested code first, inside out. The embedded longcable-server is
# third-party code run by launchd as root; it is re-signed unmodified.
find "$APP/Contents/Resources" -type f -perm -u+x 2>/dev/null | while read -r f; do
  if file "$f" | grep -q Mach-O; then sign "$f"; fi
done

if [[ "$IDENTITY" != - ]]; then
  ENT="$(mktemp -t tightwire-ent).plist"
  cat >"$ENT" <<'ENT'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.device.audio-input</key><true/>
</dict></plist>
ENT
  sign --entitlements "$ENT" "$APP"
else
  sign "$APP"
fi
codesign --verify --deep --strict "$APP"
echo "built $APP $BUNDLE_ID $VERSION ($COMMIT) (signed: $([[ "$IDENTITY" == - ]] && echo ad-hoc || echo "$IDENTITY"))"
