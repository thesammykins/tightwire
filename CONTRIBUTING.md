# Contributing to Tightwire

Thanks for helping. Bug reports with logs, test results from hardware the
maintainers do not have (other PCs, audio interfaces, Wave Link setups) and
small focused pull requests are all welcome.

## Setup

You need a Mac with Xcode 27 (Swift comes from Xcode; the Command Line
Tools alone cannot build the app) and [mise](https://mise.jdx.dev) for
everything else; do not install the project's tools through other package
managers.

```sh
git clone --recurse-submodules https://github.com/thesammykins/tightwire.git
cd tightwire
mise trust
mise install            # Rust, zig, cargo-zigbuild, Python, lint tooling
mise run test           # Rust + Swift unit tests
mise run loopback       # end-to-end on localhost, no microphone needed
mise run check-windows  # type-check and clippy the Windows code from the Mac
```

`mise tasks` lists everything; [docs/development.md](docs/development.md)
explains the tasks, layout, CLI and diagnostics. The receiver also builds and
tests natively on Windows (`windows\Build Tightwire.cmd`, then
`cargo test --manifest-path receiver/Cargo.toml`, MSVC toolchain); CI does
that on every push.

## Before you open a pull request

- `mise run test` and `mise run loopback` pass; for receiver changes also
  `mise run check-windows` and `cargo fmt --manifest-path receiver/Cargo.toml --check`.
- Behaviour changes come with a test where one is practical, and SPEC.md is
  updated when the protocol or a documented behaviour changes (record
  departures from the spec under "Deviations").
- Changes that touch Windows-only code say how they were checked: CI runs the
  build, tests, clippy and a headless smoke test, but nothing exercises
  WASAPI playback or the tray.
- Keep the real-time paths allocation- and lock-free (see SPEC §4 and §5).
- Never commit captured data from your own machines without replacing serial
  numbers, addresses and host names; `mise run secrets` runs gitleaks.

## Style

- Rust: `rustfmt` defaults and `clippy -D warnings`.
- Swift: Swift 6 language mode, no warnings; follow the surrounding code.
- Shell: bash with `set -euo pipefail`; shellcheck-clean.
- Comments explain why, not what. Commit messages follow
  [Conventional Commits](https://www.conventionalcommits.org/).

## Licence

Tightwire is licensed under GPL-3.0-or-later (see [LICENSE](LICENSE)). By
submitting a contribution you agree that it is licensed under the same terms.
Copyright stays with each contributor; the project's notice is
"Copyright © Tightwire contributors".

**SPDX headers.** New source files may start with
`SPDX-License-Identifier: GPL-3.0-or-later`; existing files are not being
rewritten to add them. The repository-wide licence in LICENSE applies to
every file without its own header, except `vendor/longcable` (Longcable's own
GPL-3.0-or-later licence and copyright) and generated files.

Adding a dependency: it must be under a licence compatible with
GPL-3.0-or-later. Rust crates are checked by cargo-about against
`receiver/about.toml` when notices are generated; run `mise run notices` and
commit the updated `THIRD-PARTY-NOTICES.txt`.

## Security issues

Do not open a public issue; see [SECURITY.md](SECURITY.md).
