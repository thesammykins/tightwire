#!/usr/bin/env bash
# Renders the app icon from assets/icon/*.svg:
#   assets/icon/Tightwire.icns  (macOS bundle, via iconutil)
#   assets/icon/tightwire.ico   (Windows exe resource and tray icon)
# tightwire-small.svg is the simplified artwork for 16-32 px; everything
# larger uses tightwire.svg. The outputs are committed so normal builds need
# neither resvg nor iconutil.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ICON="$ROOT/assets/icon"
WORK="$(mktemp -d -t tightwire-icon)"
trap 'rm -rf "$WORK"' EXIT

render() { # svg size out
  resvg -w "$2" -h "$2" "$ICON/$1" "$3"
}
art_for() { if (($1 <= 32)); then echo tightwire-small.svg; else echo tightwire.svg; fi; }

SET="$WORK/Tightwire.iconset"
mkdir -p "$SET"
for base in 16 32 128 256 512; do
  render "$(art_for "$base")" "$base" "$SET/icon_${base}x${base}.png"
  render "$(art_for $((base * 2)))" $((base * 2)) "$SET/icon_${base}x${base}@2x.png"
done
iconutil -c icns -o "$ICON/Tightwire.icns" "$SET"

ICO_PNGS=()
for s in 16 20 24 32 40 48 64 256; do
  render "$(art_for "$s")" "$s" "$WORK/ico-$s.png"
  ICO_PNGS+=("$WORK/ico-$s.png")
done
python3 - "$ICON/tightwire.ico" "${ICO_PNGS[@]}" <<'PY'
# ICO with PNG-compressed entries (supported since Windows Vista).
import struct, sys
from pathlib import Path
out, paths = sys.argv[1], sys.argv[2:]
images = [Path(p).read_bytes() for p in paths]
header = struct.pack("<HHH", 0, 1, len(images))
offset = len(header) + 16 * len(images)
entries = b""
for data in images:
    w, h = struct.unpack(">II", data[16:24])  # PNG IHDR
    entries += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
Path(out).write_bytes(header + entries + b"".join(images))
PY
ls -l "$ICON"
