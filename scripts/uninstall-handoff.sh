#!/usr/bin/env bash
# Removes Longcable's root service, settings and logs with Longcable's own
# uninstaller. Run it yourself: sudo scripts/uninstall-handoff.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [[ $EUID -ne 0 ]]; then
  echo "Run it yourself with sudo: sudo $ROOT/scripts/uninstall-handoff.sh" >&2
  exit 1
fi
exec "$ROOT/vendor/longcable/scripts/uninstall-mac.sh"
