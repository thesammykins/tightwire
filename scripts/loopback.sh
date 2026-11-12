#!/usr/bin/env bash
# SPEC §7.1a, §7.3, §7.4 on localhost, no microphone:
#   1. pairing handshake: Swift initiator (tightwire-send pair) ↔ Rust responder
#      (tightwire-recv pair), codes auto-accepted with the test-only flag;
#   2. stream a 1 kHz sine with the keys from that pairing into the file
#      backend and check the WAV is continuous at the right frequency;
#   3. same with a pre-shared key and a simulated +150 ppm device clock
#      (drift compensation);
#   4. 96 kHz stereo (resampled to the 48 kHz virtual device, both channels);
#   5. latency test: RTT probes, clock sync and end-to-end markers, plus a
#      buffer floor/cap set from the Mac (type 21) and applied by the receiver;
#   6. stop the receiver mid-stream and check the sender gates audio within ~3 s.
# Run via `mise run loopback`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="${LOOPBACK_OUT:-$ROOT/out/loopback}"
rm -rf "$OUT"
mkdir -p "$OUT"

cargo build --release --quiet --manifest-path receiver/Cargo.toml
swift build -c release --package-path mac 2>&1 | grep -E "error|warning: .*Tightwire" || true
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps --manifest-path receiver/Cargo.toml | sed -E 's/.*"target_directory":"([^"]+)".*/\1/')"
RECV="$TARGET_DIR/release/tightwire-recv"
CHECK="$TARGET_DIR/release/tightwire-wavcheck"
SEND="$(swift build -c release --package-path mac --show-bin-path)/tightwire-send"

# Below the macOS ephemeral range (49152+) so a random pick rarely collides.
PORT=$((40000 + RANDOM % 9000))
PAIR_PORT=$((PORT + 1))
FAILED=0
fail() { echo "FAIL: $*"; FAILED=1; }
field() { sed -nE "s/.*[ ^]$1=([^ ]+).*/\1/p" "$2" | head -1; }

echo "== 1. pairing handshake (Swift initiator <-> Rust responder) =="
"$RECV" pair --config "$OUT/recv-pairing.conf" --bind 127.0.0.1 --port "$PAIR_PORT" --no-beacon \
  --test-auto-accept --timeout 20 >"$OUT/pair-recv.txt" 2>"$OUT/pair-recv.log" &
RP=$!
sleep 0.3
if ! "$SEND" pair --to "127.0.0.1:$PAIR_PORT" --name "loopback-mac" --pairing-file "$OUT/send-pairing.json" \
  --test-auto-accept >"$OUT/pair-send.txt" 2>"$OUT/pair-send.log"; then
  fail "sender pairing exited non-zero"; cat "$OUT/pair-send.log"
fi
wait $RP || fail "receiver pairing exited non-zero"
cat "$OUT/pair-recv.txt" "$OUT/pair-send.txt"
RCODE=$(sed -nE 's/^code: ([0-9]{3} [0-9]{3}).*/\1/p' "$OUT/pair-recv.txt")
SCODE=$(sed -nE 's/^code: ([0-9]{3} [0-9]{3}).*/\1/p' "$OUT/pair-send.txt")
RFP=$(sed -nE 's/.*fingerprint ([0-9a-f]+).*/\1/p' "$OUT/pair-recv.txt")
SFP=$(sed -nE 's/.*fingerprint ([0-9a-f]+).*/\1/p' "$OUT/pair-send.txt")
[[ -n "$RCODE" && "$RCODE" == "$SCODE" ]] || fail "confirmation codes differ: '$RCODE' vs '$SCODE'"
[[ -n "$RFP" && "$RFP" == "$SFP" ]] || fail "derived keys differ: '$RFP' vs '$SFP'"
[[ $(stat -f %Lp "$OUT/recv-pairing.conf") == 600 ]] || fail "receiver pairing file is not 0600"
echo "codes match ($RCODE), key fingerprints match ($RFP)"

# Timing-sensitive checks (buffer depth, glitch-free audio) need a machine
# whose threads are scheduled on time. Shared CI runners stall for tens of
# milliseconds, so LOOPBACK_TIMING=relaxed reports those as warnings and
# keeps only the functional checks (pairing, auth, every packet delivered).
timing() { # message; returns 0 (ignored) when relaxed, 1 when strict
  if [[ "${LOOPBACK_TIMING:-strict}" == relaxed ]]; then echo "warning (timing relaxed): $1"; return 0; fi
  echo "$1"; return 1
}

stream_once() { # name, recv args..., -- , send args... ; returns non-zero on failure
  local name=$1; shift
  local rargs=() sargs=() bad=0
  while [[ $1 != -- ]]; do rargs+=("$1"); shift; done; shift
  sargs=("$@")
  "$RECV" --backend file --out "$OUT/$name.wav" --bind 127.0.0.1 --port "$PORT" --duration 12 "${rargs[@]}" \
    >"$OUT/$name.recv.txt" 2>"$OUT/$name.recv.log" &
  local rp=$!
  sleep 0.5
  "$SEND" --sine 1000 --frames 48 --to "127.0.0.1:$PORT" --duration 10 --quiet "${sargs[@]}" \
    >"$OUT/$name.send.txt" 2>"$OUT/$name.send.log" || { echo "$name: sender exited non-zero"; bad=1; }
  wait $rp || { echo "$name: receiver exited non-zero"; bad=1; }
  cat "$OUT/$name.recv.txt" "$OUT/$name.send.txt"
  "$CHECK" "$OUT/$name.wav" --freq 1000 --skip-ms 500 || timing "$name: WAV check failed" || bad=1
  local depth lost late under auth
  depth=$(field depth_avg_ms "$OUT/$name.recv.txt")
  lost=$(field lost "$OUT/$name.recv.txt"); late=$(field late "$OUT/$name.recv.txt")
  under=$(field underruns "$OUT/$name.recv.txt"); auth=$(field auth_dropped "$OUT/$name.recv.txt")
  awk -v d="$depth" 'BEGIN { exit !(d > 0 && d <= 4.0) }' || timing "$name: steady-state depth $depth ms (need <= 4)" || bad=1
  local rx; rx=$(field received "$OUT/$name.recv.txt")
  [[ "${rx:-0}" -ge 9000 && "$lost" == 0 ]] || { echo "$name: received ${rx:-0} packets, lost $lost (want >= 9000, 0)"; bad=1; }
  [[ "$auth" == 0 ]] || { echo "$name: $auth packets failed authentication"; bad=1; }
  echo "$name: depth ${depth} ms, lost $lost, late $late, underruns $under"
  return $bad
}

# The file backend's virtual device runs on this (shared, often busy) Mac. If
# its own thread was descheduled for > 3 ms the run measured the host, not
# Tightwire: say so and retry once. Real devices are clocked by hardware.
stream() {
  local name=$1
  if stream_once "$@"; then return; fi
  local late
  late=$(field output_late_max_ms "$OUT/$name.recv.txt")
  if awk -v l="${late:-0}" 'BEGIN { exit !(l > 3) }'; then
    echo "$name: host stalled the virtual output device by ${late} ms; retrying once"
    mv "$OUT/$name.recv.txt" "$OUT/$name.attempt1.recv.txt"
    stream_once "$@" || fail "$name (after retry)"
  else
    fail "$name"
  fi
}

echo "== 2. stream with the paired keys =="
stream paired --config "$OUT/recv-pairing.conf" -- --pairing-file "$OUT/send-pairing.json"

echo "== 3. pre-shared key, device clock +150 ppm =="
PSK=$(head -c 32 /dev/urandom | xxd -p -c 64)
stream drift150 --psk-hex "$PSK" --clock-ppm 150 -- --psk-hex "$PSK"

echo "== 4. 96 kHz stereo source (all channels) =="
stream hires96 --psk-hex "$PSK" -- --psk-hex "$PSK" --sine-rate 96000 --sine-channels 2 --all-channels
SR=$(field stream_rate "$OUT/hires96.recv.txt"); SC=$(field stream_channels "$OUT/hires96.recv.txt")
[[ "$SR" == 96000 && "$SC" == 2 ]] || fail "hires96: receiver saw ${SR} Hz ${SC} ch (want 96000 Hz 2 ch)"
echo "hires96: receiver played ${SR} Hz, ${SC} ch"

echo "== 5. latency test (RTT, clock sync, markers) and buffer limits from the Mac =="
"$RECV" --backend file --out "$OUT/latency.wav" --bind 127.0.0.1 --port "$PORT" --psk-hex "$PSK" --duration 38 \
  >"$OUT/latency.recv.txt" 2>"$OUT/latency.recv.log" &
RP=$!
sleep 0.5
"$SEND" --sine 1000 --to "127.0.0.1:$PORT" --psk-hex "$PSK" --duration 36 --latency-test --apply-tuning 3,15 --quiet \
  >"$OUT/latency.send.txt" 2>"$OUT/latency.send.log" || fail "latency: sender exited non-zero"
wait $RP || fail "latency: receiver exited non-zero"
cat "$OUT/latency.send.txt"
MARKERS=$(field markers "$OUT/latency.send.txt"); MMED=$(field marker_median_ms "$OUT/latency.send.txt")
MUNC=$(field marker_unc_ms "$OUT/latency.send.txt"); REV=$(field receiver_revision "$OUT/latency.send.txt")
DEPTH=$(field depth_avg_ms "$OUT/latency.recv.txt"); FLOOR=$(field floor_ms "$OUT/latency.recv.txt"); CAP=$(field cap_ms "$OUT/latency.recv.txt")
[[ "$REV" == 1 ]] || fail "latency: receiver revision $REV (want 1)"
[[ "${MARKERS:-0}" -ge 20 ]] || fail "latency: only ${MARKERS:-0} markers measured"
# Send → output must be about the buffer depth (plus <= 2 ms: half a packet,
# the 0.5 ms resampler filter and scheduling), never below it by more than the clock uncertainty.
awk -v m="$MMED" -v d="$DEPTH" -v u="$MUNC" 'BEGIN { exit !(m >= d - 1 - u && m <= d + 2 + u) }' \
  || timing "latency: marker median $MMED ms vs buffer $DEPTH ms (± $MUNC)" || FAILED=1
[[ "$FLOOR" == 3.000 && "$CAP" == 15.000 ]] || fail "latency: receiver floor/cap $FLOOR/$CAP ms (want 3/15)"
grep -q "tuning result: applied" "$OUT/latency.send.txt" || fail "latency: tuning not acknowledged"
echo "latency: $MARKERS markers, send→output median $MMED ms (± $MUNC), buffer $DEPTH ms, floor/cap $FLOOR/$CAP ms"

echo "== 6. receiver goes away: sender must gate within ~3 s =="
"$RECV" --backend file --out "$OUT/gate.wav" --bind 127.0.0.1 --port "$PORT" --psk-hex "$PSK" --duration 3 \
  >"$OUT/gate.recv.txt" 2>"$OUT/gate.recv.log" &
RP=$!
sleep 0.3
T0=$(python3 -c 'import time; print(time.time())')
"$SEND" --sine 1000 --to "127.0.0.1:$PORT" --psk-hex "$PSK" --duration 8 --quiet >"$OUT/gate.send.txt" 2>"$OUT/gate.send.log" &
SP=$!
wait $RP
T_RECV_END=$(python3 -c "import time; print(time.time() - $T0)")
wait $SP || fail "gate: sender exited non-zero"
cat "$OUT/gate.send.txt"
LAST_LIVE=$(field last_live_s "$OUT/gate.send.txt")
GATED=$(field gated "$OUT/gate.send.txt")
GAP=$(awk -v l="$LAST_LIVE" -v e="$T_RECV_END" 'BEGIN { printf "%.2f", l - e }')
echo "receiver stopped at ${T_RECV_END}s; sender last live at ${LAST_LIVE}s (gap ${GAP}s); gated packets $GATED"
awk -v g="$GAP" 'BEGIN { exit !(g <= 3.5) }' || fail "sender kept streaming ${GAP}s after the receiver left"
[[ "${GATED:-0}" -gt 0 ]] || fail "sender never gated"

echo "== 7. device handoff: Swift controller <-> Rust receiver (fake Longcable, fake usbip.exe) =="
if TIGHTWIRE_RECV="$RECV" swift test --package-path mac --filter HandoffIntegration >"$OUT/handoff.log" 2>&1; then
  grep -E "passed after" "$OUT/handoff.log" | tail -1
else
  fail "handoff integration"; tail -20 "$OUT/handoff.log"
fi

if [[ $FAILED -ne 0 ]]; then echo "LOOPBACK FAILED (artifacts in $OUT)"; exit 1; fi
echo "LOOPBACK PASSED (artifacts in $OUT)"
