#!/usr/bin/env bash
# Installs or updates Longcable's root launchd service (org.sapd.longcable)
# from the vendored, pinned source, using Longcable's own installer.
# Run it yourself: sudo scripts/install-handoff.sh [PC_IPV4]
#   PC_IPV4 given: USB/IP (TCP 3240) is allowed from that PC only.
#   omitted:       Longcable's default (tailnet if Tailscale runs, else the LAN subnet).
# What it does as root (vendor/longcable/scripts/install-mac.sh):
#   /Library/Application Support/Longcable/longcable-server  (root:wheel 755)
#   /Library/LaunchDaemons/org.sapd.longcable.plist          (RunAtLoad, KeepAlive)
#   /Library/Logs/Longcable/, then launchctl bootstrap system; the service
#   listens on TCP 3240 and creates /var/run/longcable/control.sock.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LC="$ROOT/vendor/longcable"
if [[ $EUID -ne 0 ]]; then
  echo "This installs a root service. Build first, then run it yourself with sudo:" >&2
  echo "  mise run build-longcable && sudo $ROOT/scripts/install-handoff.sh [PC_IPV4]" >&2
  exit 1
fi
[[ -x "$LC/.build/release/longcable-server" ]] || { echo "Run 'mise run build-longcable' first (as your user)." >&2; exit 1; }
if [[ $# -ge 1 ]]; then
  exec "$LC/scripts/install-mac.sh" --skip-build 0.0.0.0 "$1"
else
  exec "$LC/scripts/install-mac.sh" --skip-build
fi
