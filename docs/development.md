# Development notes

Reference material for working on Tightwire: tasks, layout, CLI, files,
diagnostics and what has not been verified. How to set up and contribute is
in [CONTRIBUTING.md](../CONTRIBUTING.md); the design is in [SPEC.md](../SPEC.md).

## Tasks

Tooling comes from [mise](https://mise.jdx.dev) (`mise.toml` pins Rust, zig,
cargo-zigbuild, Python, cargo-about, gitleaks, actionlint, zizmor and resvg);
Swift comes from Xcode.

| Task | What it does |
|---|---|
| `mise run test` | Rust + Swift unit tests (protocol vectors, SipHash, jitter/drift/resampler, pairing, handoff fakes) |
| `mise run loopback` | localhost: pairing handshake, 10 s sine streams (paired keys; +150 ppm clock; 96 kHz stereo), a 30 s latency test with markers and a buffer floor/cap set from the Mac, liveness gate, handoff with the real receiver and a fake usbip.exe |
| `mise run check-windows` | `cargo check` + `clippy -D warnings` for x86_64-pc-windows-msvc |
| `mise run build-windows` | cross-link `tightwire-recv.exe` (windows-gnu via zig) |
| `mise run bundle` | `dist/Tightwire.app`, ad-hoc signed (`SIGN_IDENTITY=auto` for Developer ID) |
| `mise run package-mac` / `package-windows` | `dist/Tightwire.dmg` and `dist/Tightwire-Windows.zip` (README › Build and install) |
| `windows\Build Tightwire.cmd` (on a PC) | `windows/build.ps1`: native MSVC build into `dist\Tightwire-Windows`, same layout as the zip |
| `mise run notices` | regenerate `THIRD-PARTY-NOTICES.txt` |
| `mise run icon` | render `assets/icon/*.svg` to `Tightwire.icns` and `tightwire.ico` |
| `mise run secrets` / `lint-actions` | gitleaks over tree and history; actionlint + zizmor |
| `swift run --package-path mac Tightwire --snapshot out/ui` | PNGs of every window pane state at min and large size |
| `tightwire-send handoff-check --device 0fd9:0084 --client-ip <Mac IP Longcable allows>` | Mac half of a real handover (this Mac imports in place of the PC), ends unshared |
| `mise run vectors` | regenerate `protocol/vectors/vectors.json` |
| `mise run build-longcable` / `install-handoff` / `uninstall-handoff` | Longcable from the submodule; the install tasks only print the sudo command |

Layout: `protocol/` (vectors and generator), `receiver/` (Rust crate:
receiver, CLI, tray, WASAPI/cpal/file backends), `mac/` (SwiftPM: TightwireRT
C real-time packetizer, TightwireCore, the app, `tightwire-send` CLI),
`windows/` (installer), `vendor/longcable` (submodule), `assets/icon/`,
`scripts/`, `.github/workflows/`.

## Builds

There are no binary releases, notarization or in-app updates: everyone
builds their own (README › Build and install).

- **macOS:** `mise run package-mac` builds Longcable from the pinned
  submodule, embeds it under `Contents/Resources/longcable`, and signs with
  the first "Developer ID Application" identity in the keychain (hardened
  runtime on every executable), else ad-hoc. `SIGN_IDENTITY=-` forces
  ad-hoc; `DIST=<dir>` builds somewhere other than `dist/`. Needs Xcode: the
  Command Line Tools (27) build Longcable but not the app, whose SwiftUI
  `@State` is a macro whose plugin only Xcode ships.
- **Windows:** `mise run package-windows` cross-links for
  `x86_64-pc-windows-gnu` with zig on the Mac. On a PC,
  `windows/build.ps1` (or `Build Tightwire.cmd`) checks for rustup and the
  MSVC build tools, runs `cargo build --release --locked` and copies the
  same files as the zip, taking `THIRD-PARTY-NOTICES.txt` from the
  repository (keep it current with `mise run notices`). CI's Windows job runs
  that script. The exe icon is linked as a `.res` file that
  `receiver/build.rs` writes itself, so no resource compiler is needed on
  either path.

### Bundle identifier

Upstream builds are `app.sammykins.tightwire`. The identifier is one setting
in `mac/scripts/bundle.sh`, taken from `BUNDLE_ID` in the environment, else
from `mac/local.env` (gitignored), else the default:

```sh
# mac/local.env
BUNDLE_ID=com.example.tightwire
```

Use it to keep an identifier you already run: macOS keys the app's settings
(its defaults domain), privacy grants, login item and the Keychain pairing's
access by identifier and signature, so a new identifier starts from scratch
(permissions, pairing, settings). No Swift code names the identifier; the app
reads its own bundle at run time. The log path, Longcable's service and the
PC side do not depend on it.

## CLI quick reference

```sh
tightwire-recv pair                      # PC: pairing mode (UDP 4814, 120 s)
tightwire-recv --list-devices
tightwire-recv --device "wave link"      # stream (WASAPI on Windows, cpal elsewhere)
tightwire-recv doctor
tightwire-recv deck toggle
tightwire-send pair --to PC:4814         # Mac CLI pairing (file store, not Keychain)
tightwire-send --device "MicrophoneFX"   # stream from a device
tightwire-send --sine 1000 --psk-hex … --to 127.0.0.1:4810   # tests, no mic
tightwire-send --sine 1000 --latency-test --apply-tuning 0,20 …  # latency test from the CLI
```

On Windows the receiver binary is installed as `tightwire.exe`; without
arguments it is the tray app, with arguments the CLI. `--device "<name
substring>"` opens that device; without it the CLI uses the tray's saved
choice and otherwise refuses. Tuning: packet size in the Mac's Stream pane
(48 frames = 1 ms at 48 kHz by default; 32 is lower latency, 128–256 kinder
to busy Wi-Fi). The buffer is adaptive (one packet + one device period +
0.5 ms, plus measured jitter), capped at 20 ms. The latency test's **Apply**
sends a floor and cap to the PC (type 21), saved in `tuning.conf`;
`--target-ms` (floor) and `--max-ms` (cap) on the command line win over
`tuning.conf`. `--shared` skips exclusive mode.

`--list-devices` and `doctor` label the WASAPI periods separately: "shared
default period" and "exclusive min period" come from
`IAudioClient::GetDevicePeriod`, the shared engine range from
`IAudioClient3`. Elgato Virtual Audio reports a 480-frame (10 ms) shared
minimum, equal to its default, so only exclusive mode reaches its 3 ms
minimum. `doctor` also says whether a device's exclusive preference is the
name default or was saved by the tray toggle.

## Files

- Mac: pairing in the Keychain (service "Tightwire pairing"); settings in the
  app's defaults; log `~/Library/Logs/Tightwire/tightwire.log` (1 MB, 3 kept).
- PC: pairing in `%APPDATA%\Tightwire\pairing.conf`
  (`~/Library/Application Support/Tightwire/` when the receiver runs on
  macOS); on Windows the root key is DPAPI-encrypted (`root_key_dpapi`, user
  scope) and a 0.1.x plain `root_key` is rewritten encrypted on the next
  save. Next to it: `tray.conf` (output endpoint id and name, per-device
  exclusive mode) and `tuning.conf` (buffer floor/cap set from the Mac);
  `%LOCALAPPDATA%\Tightwire\status.txt` (written every 5 s for `doctor`)
  and `tightwire.log` (a `stats:` line every 10 s while audio flows).
- Mac log: a `[stats]` line every 10 s while streaming (buffer, target,
  jitter, loss, underruns, drift, round trip) and one `[latency]` line per
  latency test with the full result.
- Firewall rule **Tightwire (UDP)**: UDP 4810 (audio and control) and 4814
  (pairing), program-scoped, from the local subnet, Private and Domain
  profiles only (0.1.x used all profiles; re-running the 0.2.0 installer
  replaces the rule). By hand, in an administrator PowerShell:

  ```powershell
  New-NetFirewallRule -DisplayName 'Tightwire (UDP)' -Direction Inbound -Action Allow -Protocol UDP `
    -LocalPort 4810,4814 -RemoteAddress LocalSubnet -Profile Private,Domain `
    -Program "$env:LOCALAPPDATA\Programs\Tightwire\tightwire.exe"
  ```

`doctor` prints: pairing state and, from the running tray, the last
authenticated packet's age; the saved output, whether it is present, and a
1-second open test on it (only on the chosen device and only when the tray is
not playing into it); every endpoint with its id; UDP 4810 and the firewall
rule; usbip-win2's path, `--version` and `port` output; a TCP connect test to
the Mac's port 3240 with a hint when it fails.

## Latency measurement

The Mac probes once a second while live (5/s during a test) and fits the
PC's clock against its own: NTP-style midpoints, the lowest-round-trip
exchange of each 2 s slot, a least-squares line for crystal drift; the
uncertainty is half that round trip plus the fit residual. During the
latency test the C packetizer flags one packet a second as a marker,
stamped with Core Audio's capture time (no allocation or locks on the audio
thread); the PC's stats report when that packet's first frame left the
resampler for the output device. Details: SPEC.md §4 and §3 (protocol
revision 1).

Measured so far only by `mise run loopback` (sender and receiver on one Mac,
file backend): about 3 ms capture → output with a 2.6–3.5 ms buffer and a
clock-fit uncertainty under 1 ms. The README's design budget for two
machines is not a measurement.

## Clock drift tests

`receiver/tests/core.rs` runs two hours of stream time on a virtual clock at
+100 and −100 ppm with 0–0.5 ms random network delay into a 10 ms-period
device: the buffer holds at 11.95–12.07 ms (target 12.0–12.5 ms) with zero
underruns, and the correction settles at ±100.0 ppm. Same file: an isolated
3 ms stall after a quiet minute can cause one underrun, because the adaptive
margin has relaxed by then (SPEC.md Deviation 22); the latency test
recommends a floor when it sees that.

## Reading the handoff logs

A tray click or a `deck` command logs what was *asked* ("deck: asked the Mac
to send devices to this PC …") and the Mac's answer; that answer only means
the Mac accepted the request. The move itself shows up as `handoff:` lines:
the offer received (peer, BUSIDs), each exact `usbip.exe` command line with
exit code, output and duration, the resulting port, and the ack sent back. If
no offer arrives within 20 s of an accepted request, the log says so.

First real handoff checklist:

1. Mac Setup shows the helper as running; PC `tightwire.exe doctor` shows
   usbip-win2 found. Its TCP test may say REFUSED or TIMED OUT before the
   first handoff: the Mac adds this PC to Longcable's allow-list itself when
   you press **Send to PC**.
2. Tick the deck, press **Send to PC**. It should light up on the PC within
   ~5 s; the Mac log shows "PC attached 1-N on port M", the PC log
   `handoff: … ack sent …`.
3. Press a key on the PC: Windows' Stream Deck software sees it.
4. **Back to Mac**: the deck returns and `usbip.exe port` on the PC lists
   nothing from the Mac.
5. Unplug the PC's network for 10 s: the deck returns to the Mac by itself.

## What is untested

- Everything Windows-side that is new in 0.2.0 is compiled, linted and
  unit-tested only: shared-mode period logging, DPAPI key storage, the
  WSAEMSGSIZE handling, the firewall profile change, the stats extension and
  buffer tuning on WASAPI.
- On Windows, 0.1.0 ran on one PC (pairing, streaming, WASAPI exclusive on
  real and Elgato virtual endpoints), and the first real 0.1.1 run on that PC
  showed streaming and Stream Deck handoff (usbip-win2) working end to end.
  Not exercised for real: waiting/retry for an absent device and the
  quit-and-retry path for a deck held by the Mac's Stream Deck app. CI builds
  with MSVC through `windows/build.ps1` and runs the tests and a headless CLI
  smoke test on Windows, but not the tray, the installer or real audio.
- `windows/build.ps1` on a real PC: only CI's `windows-latest` runner (with
  Visual Studio preinstalled) has run it; its missing-Rust and
  missing-build-tools messages have not been seen on a bare machine.
- Live microphone capture through the Mac app (tests use a synthetic sine to
  avoid the permission prompt).
- Real cross-machine latency: the marker measurement exists but has only
  run on localhost.
- The embedded `longcable-server` running as the root launchd service with
  the hardened runtime: Developer ID builds (`package-mac` when the
  keychain has the identity) sign it that way, the bundle
  verifies, but no such build has been installed as the helper yet.
