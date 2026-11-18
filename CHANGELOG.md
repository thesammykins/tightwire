# Changelog

All notable changes to Tightwire. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). There are no binary releases:
a version is a point in the source history, and you build it yourself.

## Unreleased

### Added

- Mac: **Receiver › Network** pins the stream to one interface, for a PC
  reachable over both the LAN and a direct cable. Tightwire then streams to
  the PC's address on that link (stored address, else `<name>.local`) and
  never falls back to another route. CLI: `--interface NAME` and
  `--list-interfaces`.

### Fixed

- PC: tray **Quit** now stops the receiver and exits. It used to remove the
  window only, leaving a hidden process that held UDP 4810 and made every
  restart report "Tightwire is already running".

## 0.2.1

Fixes from the first long session on a real PC. Build both sides from the
same source: a 0.2.0 Mac and PC still stream together, but the Mac's "PC is
not playing" warning and receiver health need 0.2.1 on both.

### Fixed

- PC: the drift loop now measures buffer depth against what the output
  device really played, fitted from its callbacks, instead of assuming one
  callback per period at nominal speed. Output devices that hand out
  several buffers at once (common for virtual endpoints in exclusive
  mode), stall, or run thousands of ppm off nominal no longer cause an
  underrun storm, a pinned ±1000 ppm correction or a misreported buffer.
  The buffer floor covers the device's measured bursts.
- PC: with the buffer target at its cap, every peak of the buffer's
  sawtooth was faded and skipped (heard as static) and the correction sat
  at −1000 ppm. The overflow threshold now always stays above the target.
- PC: large, lasting buffer errors (after a stall, a burst or a jump in the
  target) are removed in one faded step (at most one a second) instead of
  seconds of saturated correction; rates beyond ±1000 ppm are tracked up
  to ±1 % without moving the pitch beyond 1.7 cents.
- PC: shared mode kept the whole endpoint buffer full, adding latency no
  stat showed; it now keeps one engine period plus a small margin queued.
- PC: each output open uses a fresh audio client and releases a failed one
  at once; errors such as 0x8889000A are explained in plain words with a
  hint; every retry outcome is logged; an unresponsive driver times out
  instead of freezing the tray.
- PC: the tray no longer flips between "searching" and "streaming" on
  every underrun.
- PC: a second tray now says Tightwire is already running and exits;
  "UDP port 4810 in use" says another Tightwire is probably running.
- Mac: with the Stream Deck unplugged, Stream Deck handoff waits for it to
  be plugged in (or for Send to PC) instead of retrying every 30 s and
  toggling the keep-awake assertion.

### Changed

- PC: exclusive mode is off unless you turn it on (tray › Exclusive mode,
  per output; CLI `--exclusive`), and is described as experimental for
  virtual devices. If an exclusive stream keeps underrunning or the drift
  correction stays at its limit for about 10 s, the PC switches that output
  to shared mode, says so once, logs why and leaves exclusive mode off for
  it until you turn it on again.
- Protocol revision 2 (version byte unchanged): the stats extension gains
  an output health block (output open, saturated, underruns climbing,
  exclusive fallback, audio queued in the PC's output endpoint, device
  rate, step and trim counts).

### Added

- Mac: when the PC has no output open, the menu bar icon shows a yellow
  "!" and the Stream pane says "PC is not playing: choose an output on the
  PC"; PC health problems are shown in the Stream pane. The latency
  breakdown includes the PC's output queue as reported by the PC.
- PC: the 10 s `stats:` log line says what the output device does
  (callbacks per second, interval mean/max, frames per second against
  nominal, measured rate, burst lead, queued audio) and counts trims,
  skips and inserts.

## 0.2.0

The first public version. Build the Mac app and the PC receiver from the
same source: a 0.1.x PC still plays a 0.2.0 Mac's audio, but Stream Deck
handoff needs 0.2.0 on both sides.

### Added

- Latency breakdown in the Stream pane (each term labelled measured,
  reported, fixed or estimate) and a 30 s **Run Latency Test** that
  measures capture → PC output device time with timestamped marker packets,
  recommends a packet size and buffer floor/cap, and can apply them to the
  PC (saved in `tuning.conf`).
- Protocol revision 1 (version byte unchanged): hello and stats extensions,
  latency markers and buffer tuning messages. Each side tells you when the
  other needs updating.
- A stats line every 10 s in both logs while streaming.
- The Mac does not idle-sleep while streaming or while a device is on the PC.
- Stereo: devices with two or more channels are sent with all channels
  unless you pick one; the input defaults to the system default input and
  the channel is remembered per device.
- Menu bar icon states: searching (ring), streaming (red dot), error ("!").
- Version and git commit in the Mac log, Setup › Diagnostics and Info.plist.
- Licensed under GPL-3.0-or-later. The built Mac app and Windows folder
  include LICENSE and THIRD-PARTY-NOTICES.txt.
- App icon for the Mac app and the Windows exe.
- Build from source on both sides: `mise run package-mac` (ad-hoc signed,
  no Apple account) and, on the PC, `windows\Build Tightwire.cmd` (Rust and
  the Visual Studio C++ Build Tools) or the zip cross-built on the Mac.
- The Mac bundle identifier is `app.sammykins.tightwire`, overridable for
  one's own builds (`BUNDLE_ID`, `mac/local.env`).

### Changed

- Windows: `--list-devices` and `doctor` report shared-mode and
  exclusive-mode periods separately and log the driver's shared-mode range;
  `doctor` says whether the exclusive preference is the default or saved.
- Developer ID builds sign every executable, including the embedded
  Longcable helper, with the hardened runtime.

### Security

- The PC attaches USB devices only from the Mac address signed inside the
  handoff offer, and honours each request once (replays are refused).
- The Mac changes where it streams only when the PC signs its own address
  in an authenticated packet that came from that address; probes no longer
  move either side's idea of the other's address.
- Longcable's allow-list: the PC's exact address is listed for each
  handover (a wider range no longer counts) and removed afterwards if
  Tightwire added it; the Stream Deck pane offers **Allow Only <PC>**.
- The handoff helper can be installed only after pairing, with only the PC
  on its allow-list, and runs from a root-owned copy of the app verified
  against the running app's code signature, with a clean environment.
- `tightwire://` URL buttons are off until you allow them.
- Pairing shows the code first; peer names with control or formatting
  characters, or over 40 characters, are refused; duplicate announcements
  are flagged.
- Windows: the pairing key is stored DPAPI-encrypted; the firewall rule is
  limited to Private and Domain networks (re-run the installer; a home
  network set to Public must be switched to Private); oversized datagrams
  are counted as dropped.

## Before 0.2.0

0.1.0 and 0.1.1 were private builds used on one desk: the first Mac sender
and Windows receiver, pairing, WASAPI output and Stream Deck handoff, then an
explicit output choice on the PC (never the system default), per-device
exclusive mode, handoff logging and `doctor` checks.
