# Changelog

All notable changes to Tightwire. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). There are no binary releases:
a version is a point in the source history, and you build it yourself.

## Unreleased

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
