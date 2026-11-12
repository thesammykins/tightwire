#!/usr/bin/env bash
# Stand-in for usbip-win2's usbip.exe in the handoff integration test.
# Prints the same shapes as usbip-win2 (userspace/usbip/*.cpp); keeps attached
# devices in $FAKE_USBIP_STATE ("port host busid" per line).
set -euo pipefail
state="${FAKE_USBIP_STATE:?}"
touch "$state"
echo "$*" >>"$state.calls"
case "${1:-}" in
  port)
    [[ -s "$state" ]] || exit 0
    printf 'Imported USB devices\n====================\n'
    while read -r port host bus; do
      printf 'Port %02d: device in use at High Speed(480Mbps)\n' "$port"
      printf '         Elgato Systems GmbH : unknown product (0fd9:0084)\n'
      printf '           -> usbip://%s:3240/%s\n' "$host" "$bus"
      printf '           -> remote bus/dev: 001/002\n           -> serial: A1\n           -> mode: default\n'
    done <"$state"
    ;;
  attach)
    shift
    host='' bus='' stop=false
    while (($#)); do case "$1" in -r) host=$2; shift 2 ;; -b) bus=$2; shift 2 ;; -x) stop=true; shift ;; *) shift ;; esac; done
    $stop && exit 0
    port=$(( $(wc -l <"$state") + 1 ))
    echo "$port $host $bus" >>"$state"
    echo "$port"
    ;;
  detach)
    port=$((10#$3))
    grep -v "^$port " "$state" >"$state.tmp" || true
    mv "$state.tmp" "$state"
    echo "port $port is successfully detached"
    ;;
  *) echo "usbip: unknown command" >&2; exit 1 ;;
esac
