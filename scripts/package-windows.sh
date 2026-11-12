#!/usr/bin/env bash
# Builds dist/Tightwire-Windows.zip on the Mac: tightwire.exe (GUI subsystem;
# tray app without arguments, CLI with them) plus the installer scripts,
# README, licence and third-party notices. windows/build.ps1 assembles the
# same folder natively on a PC.
#   default            cross-link for x86_64-pc-windows-gnu via zig
#   TIGHTWIRE_EXE=...  package that exe instead
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
if [[ -n "${TIGHTWIRE_EXE:-}" ]]; then
  EXE="$TIGHTWIRE_EXE"
else
  cargo zigbuild --profile dist --manifest-path receiver/Cargo.toml --target x86_64-pc-windows-gnu --bin tightwire-recv
  TARGET_DIR="$(cargo metadata --format-version 1 --no-deps --manifest-path receiver/Cargo.toml | sed -E 's/.*"target_directory":"([^"]+)".*/\1/')"
  EXE="$TARGET_DIR/x86_64-pc-windows-gnu/dist/tightwire-recv.exe"
fi
STAGE="$ROOT/dist/Tightwire-Windows"
rm -rf "$STAGE" "$ROOT/dist/Tightwire-Windows.zip"
mkdir -p "$STAGE"
cp "$EXE" "$STAGE/tightwire.exe"
# The same files windows/build.ps1 assembles on a PC (not the build scripts).
cp "windows/Install Tightwire.cmd" "windows/Uninstall Tightwire.cmd" windows/install.ps1 windows/uninstall.ps1 windows/README.txt "$STAGE/"
cp LICENSE "$STAGE/LICENSE.txt"
bash scripts/notices.sh "$STAGE/THIRD-PARTY-NOTICES.txt" >/dev/null
for f in LICENSE.txt THIRD-PARTY-NOTICES.txt; do
  perl -pi -e 's/\r?\n/\r\n/' "$STAGE/$f"
done
(cd "$ROOT/dist" && zip -qr -X Tightwire-Windows.zip Tightwire-Windows)
file "$STAGE/tightwire.exe"
ls -l "$ROOT/dist/Tightwire-Windows.zip"
