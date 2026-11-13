# Tightwire — system design and spec

Send one macOS input device (a microphone) to a Windows PC over wired LAN as
uncompressed PCM, with end-to-end latency low enough to monitor live.

## 1. Why custom instead of VBAN

VBAN is already raw PCM over UDP, so a custom protocol cannot beat it on *audio
quality per packet*. Where VBAN setups lose is at the receiver:

| Area | Typical VBAN (Voicemeeter) | Tightwire |
|---|---|---|
| Packet size | 256 samples typical (5.3 ms) | 48 frames (1 ms) default, down to 32 |
| Receive buffer | fixed "network quality" presets, 10–50 ms | adaptive jitter buffer, 2–4 ms on wired LAN |
| Clock drift (Mac vs PC crystal) | drop/duplicate samples → periodic clicks | continuous resampling ±0.1 %, inaudible |
| Output | through Voicemeeter's mixing engine | straight to the device, WASAPI exclusive, event-driven |
| Sample format | s16/s24 common | 32-bit float end to end (bit-exact from CoreAudio) |

Latency budget (48 kHz, target, to the output device's input):

| Stage | ms |
|---|---|
| CoreAudio capture buffer (32–64 frames) | 0.7–1.3 |
| Packetization (1 packet = 1 capture callback) | 0 |
| Wired LAN | < 0.5 |
| Jitter buffer target | 2–4 |
| WASAPI exclusive period | ~3 |
| **Total** | **~6–9** |

The receiver needs a virtual audio endpoint on Windows so apps see the stream
as a microphone. See §1a: Wave Link on Windows provides one, so no extra driver
is needed. That endpoint's own latency is outside our control and is on top of
the figure above.

## 1a. Elgato Wave Link / Wave Cast

Findings (Wave Link 3.5.0 on macOS, Elgato docs, Nov 2026):

- Wave Cast is Windows 11 only on both ends, a paid Marketplace add-on, and its
  wire protocol, ports and licence handshake are undocumented. The macOS Wave
  Link binary contains no Wave Cast code. Emulating a Wave Cast peer would mean
  reverse-engineering a licensed proprietary protocol with no traffic to study
  and would break on any Wave Link update. **Not pursued.**
- Wave Link's local API (TCP 127.0.0.1:1884) is control-only; it carries no
  audio.

Instead, Tightwire plugs into Wave Link at both edges, using only public audio
devices:

- **Mac:** Wave Link's virtual devices are ordinary CoreAudio inputs. Since
  0.2.0 the source is the user's saved choice, else the system default
  input; no product name is preferred (`Wave Link MicrophoneFX`, the 0.1
  default, produced nothing useful in testing). Devices with 2+ channels default
  to sending all channels; the channel choice is stored per device.
- **Windows:** the receiver renders into one of Wave Link's virtual playback
  endpoints (an input channel in the Windows Wave Link mixer). Wave Link then
  mixes it and exposes its own virtual microphone to games, Discord and OBS —
  the same place Wave Cast audio would land. VB-Cable is only a fallback for a
  PC without Wave Link. Virtual endpoints usually refuse WASAPI exclusive mode,
  so shared mode with the smallest period (IAudioClient3 where available) must
  be a first-class path, not just a fallback.
- In the first real run (0.1.0) Wave Link's Windows endpoints are
  named `<channel> (Elgato Virtual Audio)`, not "Wave Link …", and accepted
  WASAPI exclusive mode. The receiver never chooses an endpoint by name
  pattern: the tray saves the endpoint the user picks (§10), the CLI matches
  `--device` by case-insensitive substring, and `--list-devices` shows what
  exists.

## 2. Components

```
macOS                                             Windows
┌────────────────────────────┐   UDP :4810    ┌─────────────────────────────┐
│ Tightwire.app (menu bar)     │ ─────────────▶ │ tightwire-recv.exe            │
│  CoreAudio HAL input       │   audio        │  socket thread              │
│  → packetizer → UDP socket │ ◀───────────── │  → jitter buffer            │
│  menu + settings window    │   stats/beacon │  → drift resampler          │
└────────────────────────────┘                │  → WASAPI out (Wave Link)   │
                                              └─────────────────────────────┘
```

Repository layout:

```
SPEC.md
mise.toml                 # rust pin + tasks (build, test, loopback)
protocol/vectors/         # golden packet hex + JSON, used by both test suites
mac/                      # SwiftPM package
  Sources/TightwireCore/    # protocol, capture, sender (no UI, testable)
  Sources/Tightwire/        # SwiftUI menu bar app and settings window
  Sources/tightwire-send/   # headless CLI (device or synthetic sine source)
  Tests/TightwireCoreTests/
  scripts/bundle.sh       # builds Tightwire.app, Info.plist, ad-hoc codesign
receiver/                 # Rust crate, builds on macOS and Windows
  src/ (protocol, jitter, resample, output/{cpal,wasapi,file}, main)
  tests/
```

## 3. Wire protocol (v2)

UDP, default port **4810**, all integers little-endian. Stateless for audio:
every audio packet carries its own format, so the receiver needs no handshake
beyond §3a and survives either side restarting. v2 = v1 plus the
authentication of §3a; a v2 peer rejects v1 packets (version byte).

Common header (8 bytes):

| Off | Size | Field |
|---|---|---|
| 0 | 4 | magic `"TWIR"` |
| 4 | 1 | version = 2 |
| 5 | 1 | type (table below) |
| 6 | 2 | stream id (random per sender start; 0 where unused) |

| Type | Name | Direction | Tagged |
|---|---|---|---|
| 1 | audio | Mac → receiver | yes (with session nonce) |
| 2 | stats | receiver → Mac | yes |
| 3 | beacon | receiver → broadcast :4811, pairing mode only | no |
| 4 | hello | receiver → Mac | yes |
| 5 | probe | Mac → receiver | yes |
| 6–7 | reserved | | |
| 8–12 | pairing: commit, response, reveal, confirm, abort | both | no (see §3a) |
| 13–15 | reserved | | |
| 16–20 | device handoff (§9) | both | yes |
| 21 | buffer tuning set | Mac → receiver | yes (with session nonce) |
| 22 | buffer tuning ack | receiver → Mac | yes |
| 23–31 | reserved for authenticated control messages | both | yes |
| 32–255 | reserved | | |

"Tagged" packets end with the 8-byte authentication tag of §3a; lengths
below exclude it.

Audio packet (type 1), header + 16 bytes + payload + tag:

| Off | Size | Field |
|---|---|---|
| 8 | 4 | sequence number (wraps) |
| 12 | 8 | timestamp: index of first frame since stream start |
| 20 | 1 | sample-rate code (0=44100, 1=48000, 2=88200, 3=96000) |
| 21 | 1 | channels (1–8) |
| 22 | 1 | sample format (0 = f32, 1 = s24 packed, 2 = s16) |
| 23 | 1 | flags (bit 0 = discontinuity, bit 1 = latency marker, 0.2.0) |
| 24 | … | interleaved PCM; frame count = payload length / (channels × bytes) |

Senders emit f32 only; receivers must accept all three formats. Packets must
fit one Ethernet frame (payload ≤ 1400 bytes); no fragmentation, no
retransmission, no FEC.

Stats packet (type 2), receiver → the paired sender's address, once per second
while audio arrives; header stream id = the stream being played. Body (40
bytes): jitter buffer depth (µs, u32), packets received / lost / late (u32
each), drift (ppm, i32), output underruns (u32), packets dropped by
authentication (u32), highest audio sequence received (u32, freshness echo),
receiver session nonce (u64).

Hello (type 4), receiver → sender: receiver session nonce (u64), echoed probe
nonce (u64, 0 when unsolicited).

**Protocol revision 1 (0.2.0).** The version byte stays 2: every 0.2.0
change is appended to an existing body (old peers ignore trailing bytes; both
decoders always allowed them) or is a new type (old receivers drop types 21
and 22). Each side detects the other's revision and tells the user to update
the other side instead of failing silently: a 0.2.0 Mac shows "Update
Tightwire on the PC" when hellos carry no extension; a 0.2.0 receiver answers
a handoff offer without a signed host (0.1.x Mac) with a failed ack saying
"update Tightwire on the Mac". Audio between mixed versions keeps working.

- **Hello extension** (after the 16-byte body): revision u8 (1), the
  receiver's own IPv4 toward the Mac (4 bytes, 0.0.0.0 = unknown), receiver
  monotonic clock µs (u64, stamped when the hello is built).
- **Stats extension** (after the 40-byte body, 88 bytes): revision u8 (1;
  2 when the health block follows),
  output mode u8 (0 none, 1 WASAPI exclusive, 2 WASAPI shared via
  IAudioClient3, 3 WASAPI shared default period, 4 cpal, 5 file), stream
  channels u8, output channels u8, then u32 each: target µs, arrival jitter
  mean µs, jitter peak µs, configured floor µs (0 = adaptive), cap µs, output
  period frames, output rate, stream rate, drift ppb (i32), duplicates,
  concealed frames, resets; ring fill u16, ring capacity u16, ring-full drops
  u32; last latency marker played: stream timestamp u64 (0 = none) and the
  receiver-clock µs its first frame was handed to the output device (u64);
  receiver clock µs now (u64); the receiver's own IPv4 (4 bytes).
- **Stats health block** (protocol revision 2, 0.2.1; 25 bytes after the
  88-byte extension, present when its revision byte is 2): flags u8 (bit 0
  an output device is open and rendered in the last second; bit 1 drift
  loop saturated; bit 2 more than one underrun in the last 10 s; bit 3
  exclusive mode given up for shared), then u32 each: audio queued in the
  output endpoint µs, device rate ppb (i32, 0 = unknown), device lead µs,
  trims, skips, inserts. A Mac shows "PC is not playing" only on an explicit
  bit 0 = 0 from a revision-2 PC, never from zero counters.
- **Latency marker** (audio flag bit 1): set by the Mac on one packet a
  second during a latency test. The receiver records when the marked
  packet's first frame comes out of the resampler (read position ≥ marker
  timestamp + 24 frames of filter delay), at callback time + its offset in
  the device buffer, and reports it in the next stats.
- **Tuning set** (type 21, m2r + session nonce): request id u32, adaptive u8
  (1 = no floor), floor µs u32, cap µs u32. The receiver refuses (not clamps)
  a cap outside 2–100 ms or a floor above the cap, applies accepted values to
  the audio thread lock-free, saves them to `tuning.conf` next to the pairing
  file and answers **tuning ack** (type 22, r2m): request id u32, result u8
  (0 applied, 1 rejected), floor µs, cap µs, message (u8 len + UTF-8). The
  Mac resends every 0.5 s, three times, until acked.

Probe (type 5), sender → receiver: probe nonce (u64, random per probe).

Beacon (type 3), receiver → UDP broadcast on port 4811, once per second, **only
in pairing mode** (§3a): pairing port (u16), receiver id (16 bytes), UTF-8 host
name (u8 length + bytes). The Mac app lists beacons seen in the last 3 s.

## 3a. Pairing and authentication

The sender is locked to one receiver and never transmits audio without fresh,
authenticated evidence that this receiver is listening.

**Identities and storage.** Each side has a persistent random 16-byte id. A
pairing is (peer name, peer id, 32-byte root key, last peer address). One
pairing per side; pairing again replaces it; both sides can forget it. Mac:
macOS Keychain (generic password, service `Tightwire pairing`). Receiver: a
user-only (0600 on Unix) text file, `%APPDATA%\Tightwire\pairing.conf` on
Windows, `~/Library/Application Support/Tightwire/pairing.conf` on macOS,
`$XDG_CONFIG_HOME/tightwire/pairing.conf` (default `~/.config/…`) elsewhere;
`--config` overrides. The running receiver re-reads the file when it changes.

**Pairing (one-time).** `tightwire-recv pair` binds a pairing port (default
ephemeral, `--port` to fix) and broadcasts beacons for 120 s. The Mac lists
receivers in pairing mode and the user picks one. Messages (unauthenticated,
each a full datagram with the common header, stream id 0):

| Type | Message | Body |
|---|---|---|
| 8 | commit (Mac → R) | Mac id (16), SHA-256(Mac X25519 public key) (32), name (u8 len + UTF-8) |
| 9 | response (R → Mac) | receiver id (16), receiver X25519 public key (32), name (u8 len + UTF-8) |
| 10 | reveal (Mac → R) | Mac X25519 public key (32) |
| 11 | confirm (both) | role (0 = Mac, 1 = receiver), HMAC-SHA256(k_confirm, "tightwire confirm" ‖ role ‖ TH) truncated to 16 |
| 12 | abort (both) | reason (u8: 1 = user rejected, 2 = commit mismatch, 3 = timeout) |

The Mac commits to its ephemeral public key before it sees the receiver's, and
the receiver sends its key before seeing the Mac's, so neither side can grind
keys to steer the confirmation code. The receiver aborts if the revealed key
does not match the commitment. With `S` = X25519(shared) and
`TH` = SHA-256(commit ‖ response ‖ reveal) (whole datagrams):

- `HKDF-SHA256(ikm=S, salt=TH, info="tightwire v2 pairing", L=64)` → root key
  (bytes 0–31, stored) ‖ k_confirm (bytes 32–63, used once).
- Confirmation code: `HKDF-SHA256(ikm=S, salt=TH, info="tightwire v2 sas", L=4)`
  as a big-endian u32, mod 1 000 000, shown as six digits `123 456`.

Both sides show the code. The user confirms on each (Mac: button; receiver:
y/N prompt). A side sends confirm only after its user accepts, resends it every
0.5 s until it holds the peer's valid confirm, and answers each peer confirm
with its own. A side stores the pairing only when its user accepted *and* the
peer's confirm verified. Messages are resent every 0.5 s until answered; the
whole exchange times out after 120 s.

**Session keys.** From the root key (from pairing, or `--psk-hex` on either
CLI for tests): `k_m2r` = `HKDF-SHA256(ikm=root, salt="", info="tightwire v2
m2r", L=16)`, `k_r2m` likewise with `"tightwire v2 r2m"`. Separate directions
stop reflection of a peer's own packets.

**Tags.** Every tagged packet ends with SipHash-2-4 (64-bit output, written
little-endian) keyed with the direction's key over:

- audio and Mac → receiver control (16–31): receiver session nonce (u64 LE) ‖
  packet without tag. The nonce is random per receiver run and learned from
  hello/stats, so packets captured in an earlier run never verify.
- everything else: the packet without tag.

SipHash is implemented allocation-free in Rust and in C (used from Swift and
from the Core Audio callback) and tested against the reference vectors.

**Receiver acceptance.** Drops silently and counts (`auth` in stats and status
line): bad or missing tag, wrong version, any tagged type when unpaired.
Replay: within a stream, sequences at or behind the played position or already
seen are dropped; a stream id once replaced is retired for the rest of the run
so an old stream cannot be replayed to preempt the current one.

**Peer-supplied text (0.2.0).** Beacon host names and pairing names (commit,
response) are refused at decode when longer than 40 characters or containing
Unicode Cc/Cf characters (controls, bidi overrides/isolates, zero-width
characters) or line/paragraph separators; each side sends its own name
cleaned and cut to 40. Free-text ack messages are cleaned the same way and
cut to 200 characters. Pairing prompts (tray dialog, both CLIs, Mac window)
show the code first and alone, the peer's name after it, quoted. The Mac
lists a beacon per (receiver id, address): a second address announcing the
same id is shown next to the first and flagged, never swapped in; the list
is capped at 32 entries. The stored pairing uses the authenticated name from
the handshake, not the beacon's.

**Addresses (0.2.0).** The receiver updates the Mac's address (saved for
start-up hellos and deck requests, and shown by `doctor` as the last
authenticated packet) only from session-nonce-bound packets (audio, handoff
offer/release, tuning); a probe — which carries no nonce and so can be
replayed — gets its hello answer but changes nothing. The Mac changes the
PC's address (where audio goes, and what Longcable's allow-list admits) only
when an authenticated, fresh hello or stats carries the receiver's own IPv4
equal to the datagram's source. When gated it may try the PC's name
(`<name>.local`) for up to 3 s, adopting it only on such a verified hello.

**Liveness gating (sender).** The sender is *live* while it holds fresh
authenticated evidence from the paired receiver less than 3 s old: a hello
echoing one of its probe nonces, or a stats packet whose sequence echo is
within the last 3 s of sent packets. Only while live does it send audio.
Otherwise (and when evidence is older than 1.5 s) it sends one probe per second
to the last known address, re-resolving the receiver's host name (`<name>.local`,
then `<name>`) when set. An unsolicited hello (receiver start-up) only triggers
an immediate probe. The receiver answers probes with hello, sends an unsolicited
hello to the Mac's last known address on start-up, and sends no broadcast
outside pairing mode. The Mac sender binds local UDP port 4812 (ephemeral if
taken) so start-up hellos can find it. The menu bar shows unpaired / searching
/ streaming.

## 4. macOS sender

- macOS 14+, Swift 6, SwiftPM only (no .xcodeproj).
- **Capture:** CoreAudio HAL directly (AUHAL or `AudioDeviceCreateIOProcID`),
  not AVAudioEngine. Set `kAudioDevicePropertyBufferFrameSize` to the requested
  packet size, clamped to the device's supported range. Use the device's nominal
  sample rate; no resampling on the sender.
- **Real-time path:** the IO callback copies samples into a preallocated packet
  and calls `sendto` on a non-blocking, connected UDP socket. No allocation,
  locks, Swift runtime calls that can allocate, or logging in the callback.
  Counters are atomics read by the UI.
- Socket: `SO_NET_SERVICE_TYPE = NET_SERVICE_TYPE_AV` / DSCP EF.
- Device removal or format change stops the stream cleanly and surfaces an
  error; restart sets the discontinuity flag and a new stream id.
- **Latency (0.2.0).** The control thread probes once a second while live
  (5/s during a test), keeps the last 30 round-trip times and fits the
  receiver clock against the Mac's (NTP-style midpoint per exchange; the
  lowest-RTT exchange of each 2 s slot; least-squares line for crystal
  drift; uncertainty = half that RTT + fit residual). The Stream pane shows
  each term of the mic → PC output estimate labelled measured / reported /
  fixed / estimate (README "Latency"). The latency test (30 s) records RTT,
  stats and markers, measures Core Audio capture time (the IOProc's
  inInputTime) → PC output device per marker, recommends a packet size and
  floor/cap, logs one `[latency]` line, and **Apply** sends type 21 then
  restarts with the new packet size. One `[stats]` log line per 10 s while
  streaming.
- **Keep awake (0.2.0).** While a session runs or a device is on the PC the
  app holds a `PreventUserIdleSystemSleep` IOPM assertion (display sleep
  allowed), logged on acquire/release and shown in Stream and Setup.
- **Version.** `TightwireCore/BuildInfo.swift` holds the version;
  `bundle.sh` stamps the git commit into Info.plist (`TightwireCommit`); the
  log's start line and Setup › Diagnostics show `Tightwire 0.2.1 (<commit>)`.
- **UI (dark-leaning, native controls).** The menu bar item is a native menu
  (`MenuBarExtra` `.menu` style) and nothing else: one status line (not paired
  / paired / looking for / streaming / stopped with the error), Start/Stop
  Streaming, "Stream Deck: on the Mac|PC" with Send to PC or Back to Mac (only
  when handoff is configured), Open Tightwire… (⌘,), Open Log, Quit. All
  configuration is in one resizable `Window` scene (minimum 680×480, frame
  remembered, Dock icon while open so it activates like an app): a sidebar
  with **Stream** (input device, channel, packet size 32/48/64/128/256, level
  meter, live receiver stats: buffer, drift, lost, late, underruns, rejected),
  **Receiver** (pairing flow, paired PC, address override, forget),
  **Stream Deck** (§9: helper, allowed peers, owner, device list, URLs,
  recent activity) and **Setup** (§10). The window opens at launch while setup
  is incomplete, and comes to the front with the Receiver pane when a pairing
  code is waiting. Settings persist in UserDefaults. Menu bar icon: waveform (idle,
  template), waveform + hollow ring (searching, template), waveform + red
  dot (streaming), waveform + orange "!" (error), number.circle (code
  waiting). The coloured variants are not templates; their waveform is drawn
  in the label colour of the status item's appearance so it works on light
  and dark menu bars while the dot keeps its colour.
- **Log:** `~/Library/Logs/Tightwire/tightwire.log`, rotated at 1 MB with 3
  kept: start-up, input device, stream start/stop with reason, liveness
  changes, pairing, every handoff step and every Longcable request with its
  reply, errors.
- **UI snapshots:** `Tightwire --snapshot DIR` renders every pane state at the
  minimum and a large size from a model with no side effects.
- `LSUIElement`, `NSMicrophoneUsageDescription`, ad-hoc signed bundle.
- `tightwire-send` CLI shares TightwireCore: `--list-devices`, `--device`,
  `--to host:port`, `--frames N`, and `--sine 440 --duration S` (synthetic
  source paced by a real-time clock, needs no mic permission — used for tests),
  plus `pair`, `forget` and `--psk-hex` (§3a).

## 5. Receiver (Rust)

- One crate that builds for macOS (development and loopback testing) and
  `x86_64-pc-windows-msvc`.
- Threads: socket thread (blocking recv, raised priority) → lock-free SPSC ring
  → audio callback thread. Nothing blocking or allocating in the callback.
- **Jitter buffer:** ordered by timestamp, tolerates reordering and duplicates.
  Target depth adapts to measured inter-arrival jitter (EWMA + recent peak),
  floor = one packet + one output period, default cap 20 ms. Late packets are
  dropped and counted. On a gap: short fade-out, silence, fade-in (no clicks).
  A changed stream id, format, or discontinuity flag flushes and resyncs.
- **Drift compensation:** PI controller on smoothed buffer depth drives a
  resampling ratio within ±1000 ppm around a tracked device rate of up to
  ±1 % (Deviation 25). Interpolation quality must keep a 1 kHz
  sine's THD+N below −90 dB (verify with a test); windowed-sinc preferred.
- **Output backends** behind one trait:
  - `wasapi` (Windows): shared mode, event-driven; exclusive mode
    (smallest period the device accepts, automatic fallback to shared with a
    logged warning) only when asked for (0.2.1, Deviation 24). Shared mode
    keeps one engine period plus max(period/2, 1 ms) queued (Microsoft:
    render latency is the write-to-read distance, not the buffer size),
    growing it when the engine finds the buffer empty, and reports the
    queued audio. Each open activates a fresh IAudioClient, sets the event
    handle right after Initialize and times out after 8 s; exclusive passes
    take the device buffer before rendering and skip an event whose buffer
    is not free.
    Shared mode asks IAudioClient3::GetSharedModeEnginePeriod for the
    driver's range, logs it, initialises at its minimum and reports the
    period actually in force (GetCurrentSharedModeEnginePeriod; another
    stream can hold the engine at its own period). A driver that declares no
    small buffers reports min = default (Elgato Virtual Audio: 480 frames,
    10 ms); GetDevicePeriod's minimum (3 ms there) applies to exclusive mode
    only (Microsoft: IAudioClient::GetDevicePeriod remarks, "Low Latency
    Audio"). A failed InitializeSharedAudioStream is logged before falling
    back to the default period.
    Opens only an explicitly named endpoint (tray: saved endpoint id; CLI:
    `--device`, else the tray's saved choice); never the system default.
  - `discard`: no device; used while no output is chosen or the chosen one is
    absent, so networking, pairing and handoff keep running.
  - `cpal` (any OS): development and fallback.
  - `file`: writes a WAV, driven by a virtual clock, for deterministic tests.
- CLI: `--list-devices`, `--device "<substring>"`, `--port`, `--bind`,
  `--allow <ip>` (drop other sources), `--target-ms`, `--max-ms` (win over
  `tuning.conf`), `--shared`, `--backend`, `--config`, `--psk-hex` (test
  only); subcommands `pair` and `forget`. One status line per second on
  stderr; one `stats:` log line per 10 s while audio flows.
- Every UDP receive buffer is 64 KiB, and WSAEMSGSIZE (10040) counts as a
  dropped datagram (rate-limited log) in the streaming and pairing loops.
- On Windows the root key in `pairing.conf` is stored DPAPI-encrypted
  (`root_key_dpapi`, user scope); a plain `root_key` (0.1.x) is read and
  rewritten encrypted on the next save.
- Sends stats and hello packets as in §3/§3a; beacons only while pairing.

## 6. Security and network

LAN only and unencrypted — by design for latency. Packets are authenticated
(§3a), so only the paired Mac can feed the receiver and the Mac only streams
to a receiver that proves it is there. Audio content is not confidential on
the wire. The receiver only ever *plays* audio; `--allow` further restricts
the source. README documents the
Windows Firewall inbound rule (UDP 4810) and the Wave Link (or VB-Cable)
setup on both machines.

## 7. Verification

1. **Golden vectors:** `protocol/vectors` decoded and re-encoded identically by
   both Swift and Rust test suites, including tags, key schedule and SipHash
   reference vectors.
1a. **Pairing and auth:** automated pairing handshake over localhost (Swift
   initiator ↔ Rust receiver, `--test-auto-accept`); receiver drops
   unauthenticated, tampered and replayed packets; sender stops audio within
   ~3 s when the receiver goes away.
2. **Unit tests (Rust):** jitter buffer under reordering, duplication, loss,
   burst jitter; drift controller converges for ±200 ppm simulated clock
   offset without underrun; resampler THD+N.
3. **Loopback integration (macOS, no mic permission):** `tightwire-send --sine`
   → `tightwire-recv --backend file`; the WAV must contain a continuous sine with
   no discontinuities after the first 500 ms, at the right frequency.
4. **Latency measurement:** loopback test reports steady-state jitter buffer
   depth; must settle ≤ 4 ms on localhost.
5. **Windows build:** `cargo check --target x86_64-pc-windows-msvc` must pass;
   produce an `.exe` if a linker route is available without accepting
   third-party licence terms, otherwise document `cargo build --release` on the
   PC.
6. **Not verifiable from the Mac:** WASAPI exclusive playback, Wave Link
   endpoint behaviour, real cross-machine latency, live mic capture (needs the user's
   microphone permission). These need a manual pass.

## 8. Out of scope (v1)

Compression, encryption, retransmission/FEC, multiple simultaneous streams,
Windows → Mac audio, binary releases, notarization and in-app updates
(Tightwire is built from source by whoever runs it). (Device handoff is §9; packaging and
setup are §10, which supersede the earlier exclusion of a Windows tray app and
installers.)

## 9. Device handoff (Stream Deck over USB/IP)

Moves chosen USB devices (default suggestion: Elgato, VID `0fd9`, e.g. a
Stream Deck + `0fd9:0084`) between the Mac and the paired PC. Tightwire
orchestrates two existing tools at arm's length and does not implement USB/IP:

- **Mac: Longcable** (GPL-3.0, Swift), vendored as a git submodule at
  `vendor/longcable`, pinned to `dd0dceacd91df4e802cf8405bb548e18f79d88e7`
  (2026-10-27, "README: note that a Mac can't be a client"), unmodified. Its
  `longcable-server` runs as the root launchd job `org.sapd.longcable`, serving
  USB/IP on TCP 3240. Tightwire talks to the running service only through
  Longcable's local control interface: the root-owned Unix socket
  `/var/run/longcable/control.sock` (4-byte big-endian length + JSON
  `ControlRequest`/`ControlReply`), which is what `longcable-server --control`
  itself uses. The socket is used instead of spawning the CLI because the CLI
  cannot express the `add` action (registering a device that is not yet a
  share target). Tightwire keeps its own Codable mirror of the reply, with
  every field optional.
- **Windows: usbip-win2** (`usbip.exe`, default `C:\Program Files\USBip`,
  winget id `vadimgrn.usbip-win2`). The receiver runs
  `usbip.exe attach -r HOST -b BUSID --once --terse` (with the global
  `--tcp-port N` before `attach` when the port is not 3240; attach's `-t`
  means `--terse`, the global `-t` means `--tcp-port`, so long forms are
  used) and, to give a device back, `usbip.exe port` (parsed for `Port NN:` /
  `-> usbip://HOST:PORT/BUSID`) then `usbip.exe detach -p NN` and
  `usbip.exe attach -r HOST -b BUSID -x` (stop any automatic re-attach
  attempts). Re-checked against usbip-win2's source at commit `77a781ea`
  (2026-11-08). Before attaching it tries a 2 s TCP connect to HOST:PORT: a
  timeout fails the device at once (usbip.exe would outlast the Mac's 10 s
  ack wait), a refusal is logged with the hint "the Mac has not allowed this
  PC yet" and usbip.exe still runs. attach is bounded at 8 s. Every call is
  logged with command line, exit code, trimmed stdout/stderr and duration.

**Selection.** The Mac app lists devices Longcable can see (configured
targets plus `availableDevices`) and the owner ticks handoff devices. Ticking
registers a device that is not yet a target with `add` and **unshares it at
once** (Longcable shares every new target immediately), verified through
status; a device is shared only during a handover. A device is stored as
VID:PID, IORegistry serial, USB location and the Longcable BUSID recorded at
registration, and matched to a target by BUSID, then location, then serial.
Longcable's reported serial is not a stable key: it is the generated label
(`USB0FD90084`) until Longcable has read the descriptors and the real serial
afterwards. Unticking releases (if on the PC) and unshares; Longcable's
control API cannot remove a target, so it stays registered, unshared. At
start-up the Mac unshares the selected devices and every target Tightwire
registered earlier. Selection policy, from the device's USB interface
classes: the audio interface the stream reads from (matched through the
CoreAudio UID's serial/location; with a Wave Link input, every Elgato audio
interface) cannot be selected; audio, keyboard, mouse, storage and camera
devices need a confirmation; only non-audio Elgato devices are suggested
(never the XLR Dock).

**Ownership (Mac is the single source of truth).** `owner ∈ {mac, pc}`,
`mode ∈ {auto, heldOnMac}`.

- auto: while the paired receiver is alive (fresh §3a liveness on the handoff
  channel) devices go to the PC; once liveness has been lost for 5 s they are
  reclaimed.
- "Back to Mac" sets heldOnMac, which sticks until the owner chooses "Send to
  PC" or the receiver reconnects after having gone away. "Send to PC" returns
  to auto and hands over if the receiver is alive. Toggle picks whichever
  applies to the current owner.
- Safety: the Mac reclaims (unshares) when the receiver stops responding, on
  app quit and on any failed step, and never leaves a device shared without a
  confirmed attach. Requests arriving during a transition are coalesced into
  one pending target; every transition is idempotent and logged.

**Handover:** if Longcable's `allowedPeers` does not admit the PC's IPv4,
`allow-peer <PC IP>` and verify through status (the installer's default with
Tailscale running is the tailnet only) → `share BUSID` → wait (≤ 3 s) until
status shows it enabled and present → HANDOFF_OFFER → HANDOFF_ACK (≤ 10 s).
Longcable's state string is not a readiness signal: `share` leaves it at
"Available on this Mac", and "Waiting for Windows" appears only once a USB/IP
client lists or imports (Longcable reads descriptors lazily). The PC's import
is therefore what opens the device; if it fails and Longcable's `lastError`
says the device is held ("In use by …", "Another app …") and the device's
"quit Stream Deck app" option is on (default for Elgato), quit
`com.elgato.StreamDeck` (configurable) gracefully (≤ 5 s) and offer once more.
A held-device error already recorded before the offer quits the app first. Devices whose attach failed are unshared;
if none attached, everything is reclaimed. **Handback:** HANDOFF_RELEASE →
HANDOFF_ACK (≤ 2 s) → unshare regardless → relaunch the Stream Deck app if
Tightwire quit it.

**Control messages** (authenticated, §3a type range 16–31):

| Type | Message | Dir | Tag | Body |
|---|---|---|---|---|
| 16 | handoff offer | Mac → R | m2r + nonce | request id u32, USB/IP port u16, host (u8 len + UTF-8; empty = use the packet's source address), count u8, BUSIDs (u8 len + ASCII each) |
| 17 | handoff ack | R → Mac | r2m | request id u32, op u8 (1 attach, 2 detach), count u8, per device: BUSID, result u8 (0 ok, 1 failed, 2 usbip missing), port u8 (0 = none), message (u8 len + UTF-8) |
| 18 | handoff release | Mac → R | m2r + nonce | request id u32, count u8, BUSIDs |
| 19 | deck request | R → Mac | r2m | request id u32, sent-at Unix ms u64, target u8 (0 mac, 1 pc, 2 toggle) |
| 20 | deck request ack | Mac → R | m2r (no nonce) | request id u32, accepted u8, owner after u8 (0 mac, 1 pc), message (u8 len + UTF-8) |

The Mac runs the handoff channel on its own UDP socket, local port **4813**,
with its own probes/hello liveness, so handoff works whether or not audio is
streaming. Deck requests must be within ±30 s of the Mac's clock and a request
id is honoured once per 120 s; anything unauthenticated is dropped and counted.
Type 20 has no session nonce because the short-lived `deck` process never
learns one; its freshness comes from echoing the random request id. The
receiver answers repeated offers/releases with the cached ack instead of
re-running `usbip.exe`.

**Handoff security (0.2.0).** The Mac puts its own IPv4 (the local address
on its route to the PC) in the offer's `host`, inside the tagged body. The
receiver requires `host` to be an IPv4 literal equal to the offer's source
address and never falls back to the source (0.1.x Macs send an empty host
and get "update Tightwire on the Mac"). A request id is honoured once per
receiver run: a retransmission is answered from the 32-entry ack cache; a
repeat of an id no longer cached is refused as a replay. BUSIDs must match
`^[0-9]+-[0-9]+(\.[0-9]+)*$`. On the Mac, unauthenticated packets on 4813
are logged at most every 5 s with a count. `tightwire://deck/…` URLs act
only after the owner allowed them (asked once on the first URL; Stream Deck
pane toggle; default off).

**Longcable allow-list (0.2.0).** A range that merely contains the PC
(subnet, tailnet, `any`) does not count as permission: the sequencer lists
the PC's /32 for the handover and, if it added it, removes it after the
handback; when the PC's verified address changes the old /32 is removed.
The Stream Deck pane flags wider entries and offers "Allow Only <PC>"
(forget-peer each, allow-peer the PC, verified through status).

**Triggers.** Mac: `tightwire://deck/pc`, `tightwire://deck/mac`,
`tightwire://deck/toggle` (URL scheme, for a Stream Deck "Open URL" button) and
the same three actions in the menu. Windows: `tightwire-recv deck mac|pc|toggle`
sends a deck request to the stored Mac address (port 4813) with the stored
pairing key and prints the Mac's answer; it needs no IPC with a running
receiver.

**Testing.** Process execution and the Longcable socket sit behind injectable
interfaces on both sides. The state machine, sequences, timeouts and
failure/reclaim paths are tested with fakes, and the parsers against output
copied from the tools' sources, plus real Longcable status JSON captured from
a running service (sanitised). `tightwire-send handoff-check --device VID:PID --client-ip IP`
runs the Mac half against the real service: the sequencer shares, this Mac
imports the device over USB/IP from one of its own already-allowed addresses
in place of the PC, then hands it back and unshares. usbip-win2 on a real PC
has not been run from this project.

## 10. Packaging and setup

Normal install and use need no terminal. Deliverables go to `dist/` (ignored
by git).

**macOS: `dist/Tightwire.dmg`** (`mise run package-mac`) holds `Tightwire.app`,
self-contained: the menu bar app plus Longcable built from the pinned
submodule, embedded unmodified under `Contents/Resources/longcable`
(`bin/longcable-server`, its `scripts/install-mac.sh` and
`scripts/uninstall-mac.sh`, `LICENSE`). Signed with the first "Developer ID
Application" identity in the keychain when there is one (hardened runtime,
`com.apple.security.device.audio-input`), else ad-hoc; the embedded helper is
signed with the same identity, also with the hardened runtime (Deviation 23).
Nothing is notarized or uploaded. The bundle identifier defaults to
`app.sammykins.tightwire` and comes from one setting in `mac/scripts/bundle.sh`
(`BUNDLE_ID` in the environment or a gitignored `mac/local.env`); no Swift
code names it: the defaults domain, privacy grants, login item and Keychain
access all follow whatever the running bundle says.

The window's **Setup** pane is a checklist; the window opens on it at launch
until every row is done. Each row is detected live:

- Microphone access — `AVCaptureDevice.authorizationStatus`.
- Stream Deck handoff helper — installed = `/Library/LaunchDaemons/org.sapd.longcable.plist`
  exists; running = Longcable's root-owned control socket exists. Install /
  Reinstall / Uninstall run Longcable's own `install-mac.sh` /
  `uninstall-mac.sh` via `do shell script … with administrator privileges`
  (the standard macOS password prompt). Install is offered only once paired
  and passes `0.0.0.0 <PC IPv4>` (never Longcable's subnet/tailnet default).
  The root command (`HelperInstallCommand`, unit-tested) copies the app with
  `ditto` into a root-owned `mktemp -d`, verifies the copy with `codesign
  --verify --strict --deep -R 'cdhash H"<running app's cdhash>"'`, runs the
  script from the copy under `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`
  (`do shell script` inherits the app's environment, Apple TN2065) and
  removes the copy on exit.
- Open at login — `SMAppService.mainApp`.
- Paired with a PC — Keychain pairing present.

`mise run install-handoff` / `uninstall-handoff` remain as the developer path
(same Longcable installer, run by the owner with sudo).

**Windows: `dist/Tightwire-Windows.zip`** (`mise run package-windows`):
`tightwire.exe` cross-linked on macOS for `x86_64-pc-windows-gnu` with zig,
GUI subsystem. Without arguments it is a tray app; with arguments it is the
CLI (it attaches to the parent console), e.g. `tightwire.exe deck mac` for
Stream Deck buttons, `tightwire.exe --list-devices`, `tightwire.exe doctor`.

- Tray menu: version and build (`Tightwire 0.2.1 (<commit>)`), status
  (unpaired / waiting / streaming + buffer ms), output state, Output device,
  Exclusive mode (experimental for virtual devices), Pair… (code shown in a Yes/No dialog),
  Stream Deck → Mac / → PC, Start at login (HKCU Run), Open log, Forget
  pairing, Quit. Errors surface as a tray notification and a log line. Log:
  `%LOCALAPPDATA%\Tightwire\tightwire.log`, rotated at 1 MB, 3 kept; every
  process start writes a header line with version and git commit.
  Pairing uses the fixed UDP port 4814 so one firewall rule covers it.
- **Output selection** (policy in `receiver/src/outsel.rs`, unit-tested on
  any OS). Never automatic: with no saved choice the tray opens no output,
  shows "choose an output", notifies once and discards audio while pairing,
  liveness and handoff keep working. The device list shows every active
  render endpoint, the system default marked "— system default, not
  recommended", and the saved choice checked. The choice is stored by
  endpoint id (plus name) in `%APPDATA%\Tightwire\tray.conf`; if that id is
  missing, a unique exact-name match is accepted and the id updated (0.1.0's
  name-only file migrates this way); otherwise the tray shows "waiting for
  <name>", keeps the choice and retries every 5 s and after
  `WM_DEVICECHANGE`. Exclusive mode is a per-device setting, off unless the
  user turns it on ("Exclusive mode (experimental for virtual devices)");
  a refused exclusive open falls back to shared with a log line, and an
  exclusive stream with 4 underruns in 10 s or a drift loop saturated for
  10 s is reopened in shared mode with exclusive saved off for that device
  (notified once, logged, reported to the Mac). Open failures are logged
  with a plain-words reason and hint, retried every 10 s (logged at most
  once a minute) and their recovery notified. One tray runs per session
  (named mutex). The
  stream→device resampling ratio is logged once per format.
- **Handoff diagnostics.** Deck requests from the tray or CLI are logged as
  requests ("asked the Mac to send devices to this PC") with the Mac's
  answer or a timeout; an accepted request with no offer within 20 s is
  logged. The receiver logs each authenticated offer/release (peer, BUSIDs),
  every usbip.exe call (§9), the result per device and the ack sent; dropped
  handoff/deck packets are logged with the reason, at most every 5 s (other
  dropped packets at most once a minute). Failures raise a tray
  notification. The `deck` CLI also appends to the tray's log file.
- `doctor`: build; pairing state and, from the running tray's
  `%LOCALAPPDATA%\Tightwire\status.txt`, its state and the age of the last
  authenticated packet; the saved output, whether it is present and a 1 s
  open test on that device only (exclusive/shared result; skipped while the
  tray plays into it); all endpoints with ids; UDP 4810 state; the firewall
  rule; usbip-win2 path, `usbip.exe --version` and `usbip.exe port` (command,
  exit code, output, parsed ports); a TCP connect to the Mac's port 3240 with
  the hint "the Mac has not allowed this PC yet" on refusal or timeout; ping;
  log path.
- `Install Tightwire.cmd` → `install.ps1`, self-elevating once (UAC): copies to
  `%LOCALAPPDATA%\Programs\Tightwire`, adds the inbound rule "Tightwire (UDP)"
  (UDP 4810, 4814, program-scoped, remote = LocalSubnet, Private and Domain
  profiles only), finds usbip-win2 and
  offers `winget install --id vadimgrn.usbip-win2 -e` (the owner sees and
  accepts its terms), adds a Start menu shortcut, optional start at login,
  registers `Uninstall Tightwire.cmd` under HKCU Uninstall, then starts the
  tray unelevated. Idempotent; re-run to upgrade.

## Deviations

Recorded where the build differs from the text above, with the reason.

1. **DSCP class (§4).** The socket uses `SO_NET_SERVICE_TYPE =
   NET_SERVICE_TYPE_VO`, not `NET_SERVICE_TYPE_AV`. On Darwin `_AV` maps to
   AF4x (multimedia streaming); interactive voice `_VO` is the class that maps
   to DSCP EF, which is what the spec asks for.
2. **Jitter-buffer floor (§5).** Floor = one packet + one output period
   **+ 0.5 ms**. Mean depth sits at the target, but the packet and period
   sawtooths dip to target − (P+O)/2, so at the bare floor any 0.5 ms hiccup
   the arrival-jitter estimator cannot see (output scheduling, drift-loop lag
   at start-up) underran in loopback. Localhost steady state is ~2.6 ms.
   Since 0.2.1 the depth is measured against the device's playout line,
   which reads half a period lower than 0.2.0's measure, and "one output
   period" becomes the device's measured lead (Deviation 25): half a period
   for a steady device (the same real depth as before), half a burst for a
   bursty one; priming and trims fill to target + lead.
3. **`--no-beacon` (§5).** Removed from the streaming receiver: with §3a it
   never broadcasts outside `tightwire-recv pair` (which has `--no-beacon`).
4. **Mac SipHash (§3a).** Implemented once in C (`TightwireRT`) and used from
   both the Core Audio callback and Swift, rather than a separate Swift copy;
   it is tested against the reference vectors from Swift.
5. **Freshness proofs (§3a).** "Authenticated hello/stats within 3 s" is
   tightened to *fresh* proof: hello must echo a recent probe nonce and stats
   must echo a recently sent sequence under the current receiver session
   nonce, because a tag alone does not stop a replayed hello/stats from
   keeping the gate open.
6. **Gated audio (§3a).** While gated the packetizer keeps advancing the
   timestamp but spends no sequence numbers, and sets the discontinuity flag
   when the gate reopens so the receiver resyncs cleanly.
7. **CLI pairing store (§3a).** `tightwire-send` keeps its pairing in a 0600
   JSON file (`~/Library/Application Support/Tightwire/cli-pairing.json`)
   instead of the Keychain: the app's Keychain item is ACL'd to the app, and a
   CLI reading it would raise a Keychain prompt.
8. **Real-time threads on macOS (§5).** On macOS the receiver's socket thread
   and the file backend's virtual clock run in the Mach time-constraint class.
   With normal priority on a loaded Mac (load average ~8 during development)
   wake-ups ran 2–5 ms late and showed up as fake network jitter.
9. **Burst trimming (§5).** Besides the hard cap (`--max-ms`), depth above
   2 × target + 2 ms is faded out, skipped back to the target and faded in.
   After a host stall the sender's backlog arrives as a burst; the drift loop
   can only drain 1 ms per second, which held latency 5–10 ms high for
   seconds in loopback.
10. **Loopback retry (§7.3).** The file backend's virtual device runs on the
   host. If a loopback case fails *and* that virtual device's own thread was
   descheduled for more than 3 ms (a host stall, reported as
   `output_late_max_ms`), the case is re-run once and the retry is printed.
   Real devices are hardware-clocked, so this isolates host noise without
   loosening the pass criteria.
11. **Longcable control path (§9).** Tightwire speaks Longcable's control
   socket protocol directly rather than spawning `longcable-server --control`,
   because the CLI cannot express `add`. No Longcable code is linked or
   copied; the submodule is unmodified and only built and run as its own
   process.
12. **Handoff channel (§9).** The Mac runs handoff liveness on its own socket
   (UDP 4813) instead of reusing the audio session, so devices can move
   whether or not audio is streaming. Deck acks (type 20) carry no session
   nonce; see §9.
13. **Helper install mechanism (§10).** `do shell script … with
   administrator privileges` from the app, running Longcable's own installer,
   rather than SMAppService.daemon (needs the daemon inside our bundle under
   our label plus approval in System Settings, and would bypass Longcable's
   install path) or the deprecated SMJobBless.
14. **Pairing port (§3a, §10).** `tightwire-recv pair` now defaults to the fixed
   UDP port 4814 instead of an ephemeral one, so the Windows firewall rule can
   cover it.
15. **Handoff readiness (§9).** "Wait for Waiting for Windows" is replaced by
   "enabled and present" plus a retry after a held-device import failure:
   with Longcable dd0dcea that state only appears after a USB/IP client has
   asked for the device, so the original wait could never succeed.
16. **Selection (§9).** Devices are matched by BUSID/location, not serial, and
   registering never leaves a device shared (Longcable shares on `add`).
17. **Windows output default (§1a, §10), 0.1.1.** 0.1.0 defaulted to the first
   "Wave Link" endpoint, else the system default, and opened it in exclusive
   mode. Wave Link's endpoints are not named "Wave Link" on Windows, so the
   first real run took over the system default output at 192 kHz. There is no automatic
   choice any more (§10 Output selection).
18. **Protocol bump (§3), 0.2.0.** The version byte stays 2 with a
   revision number in the hello/stats extensions, instead of v3: all
   changes are append-only or new types, so a 0.1.1 PC in the field keeps
   playing a 0.2.0 Mac's audio and each side names the side to update.
19. **Probes and the Mac's address (§3a), 0.2.0.** Probes no longer move the
   receiver's idea of the Mac's address, so a Mac that only probes (handoff
   channel idle, no audio yet) is not "seen" by `doctor` and deck requests
   go to the address of the last audio/offer/release.
20. **Stats freshness (§3a).** Stats stay bound to the session nonce and a
   recent sequence echo but not to a Mac nonce, so a stats packet replayed
   on the LAN can hold the gate open up to ~3 s after the receiver left.
   Binding them to a Mac nonce would need a new Mac → receiver message per
   second; not done.
21. **Pairing disruption (§3a).** The first commit locks a pairing window
   for 120 s and aborts are unauthenticated, so a LAN attacker can block or
   cancel a pairing attempt (not complete one). Documented, not mitigated.
22. **Jitter target and isolated stalls (§5).** With a 10 ms device period
   the adaptive margin above the floor relaxes to ~1–1.5 ms within ~20 s of
   the last stall, so an isolated 3 ms stall after a quiet minute can cause
   one underrun (`two_hours_with_a_stall_every_minute…`). The latency test
   recommends a floor when it sees underruns; the estimator was not made
   slower to relax, which would cost latency all the time.
24. **Exclusive mode opt-in (§10), 0.2.1.** 0.2.0 defaulted to exclusive
   mode for virtual endpoints by name. In the field one accepted it, then
   delivered 3 ms buffers in bursts at an off-nominal rate (underruns
   climbing, correction pinned) while shared mode on the same endpoint was
   healthy. Exclusive is now opt-in and watched (§10).
25. **Drift control against real devices (§5), 0.2.1.** Depth is measured
   against a playout line fitted to the output callbacks (a critically
   damped 10 rad/s loop, back-to-back callbacks taken as one burst), not
   "last callback + nominal speed", whose error grew with the callback size
   and burst length. The floor uses the device's recent lead peak (8 s
   decay); priming and trims fill to target + lead, the instantaneous depth
   just before the device's next pull. The PI loop keeps its ±1000 ppm
   limit; a tracked rate (±1 %) takes over when its integral sits near the
   limit with a non-shrinking error for 2 s, or when steps or underruns in
   one direction, 0.5–5 s apart and with the loop pinned that way, imply a
   rate (three quarters of it is learned per lesson, at most 3000 ppm). Lasting errors beyond
   max(3 ms, target/4) (excess) or max(2 ms, target/4) (deficit) for 0.25 s
   are removed by one fade-out/skip-or-silence/fade-in, at most once a
   second: a ~1.3 ms dip once per disturbance instead of seconds of
   saturated pitch correction. The overflow trim never sits closer to the
   target than one packet + twice the lead + 2 ms (0.2.0 trimmed every
   sawtooth peak once target = cap). A device 5000 ppm fast costs a few
   underruns while its rate is learned from a cold start (simulated: 4 in
   the first 3 s). The target may fall at most 0.6 ms/s so
   the loop can follow a decaying jitter peak without saturating.
23. **Signing (§10).** Developer ID builds sign every executable,
   including the embedded longcable-server, with the hardened runtime; ad-hoc
   builds (the default for anyone building from source) do not. Both carry
   LICENSE and THIRD-PARTY-NOTICES.txt (GPL-3.0-or-later). There are no
   binary releases and no in-app updates (§8).
