# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub:
**Security › Report a vulnerability** on this repository
(<https://github.com/thesammykins/tightwire/security/advisories/new>).
Do not open a public issue or pull request for them.

Include what you found, how to reproduce it, the versions of Tightwire (Mac
menu › Setup › Version; PC tray menu's first line) and of macOS/Windows, and
what an attacker would need (same LAN, local account, etc.).

This is a small volunteer project: expect an acknowledgement within a week.
Fixes land on `main`; the advisory is published once the fix is there.
There are no binary releases: rebuild from source to pick a fix up.

## Supported versions

Only the current `main` branch receives fixes.

## Scope

In scope: the Tightwire Mac app, the Windows receiver and installer, the
pairing and packet authentication (SPEC §3a), the handoff control messages
(SPEC §9) and the build scripts.

Known, documented properties that are not vulnerabilities by themselves (see
README › Security model):

- audio is authenticated but not encrypted;
- USB/IP (Longcable ↔ usbip-win2) is unauthenticated and unencrypted by
  protocol design, and Longcable's allow-list is IP-based;
- `tightwire://deck/…` URLs can be opened by any local app.

Problems in Longcable or usbip-win2 themselves belong to those projects
([Longcable](https://github.com/Sapd/longcable),
[usbip-win2](https://github.com/vadimgrn/usbip-win2)); tell us too if
Tightwire's use of them makes a problem worse.
