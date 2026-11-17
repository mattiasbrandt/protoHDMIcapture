# protoHDMIcapture

A low-latency viewer for an HDMI source through a cheap USB capture card:
the picture and the sound, nothing recorded or streamed. The product is
protoHDMIcapture, the Rust program at the repo root (Linux and Windows), which
the releases ship. protoHDMIcapture legacy, `view.sh` + mpv in `legacy/`, is
kept best effort, Linux only, and is not retired
([ADR 0002](docs/adr/0002-legacy-kept-best-effort.md)): nothing in
protoHDMIcapture depends on it, no check holds it to parity, and a change to
protoHDMIcapture need not touch it.

- **State:** the open issues hold what is open; #50 holds the deferred
  Windows work. Epic #10 (closed) and its sub-issues hold what was done and
  decided; its closing comment links the decisions.
- **Generic:** code and docs serve any user's setup. One machine's values
  (its card serial, monitor, paths) live in that user's settings file, and
  examples use placeholders (`/dev/videoN`, `DP-1`). The repo is public.
- **Shared with legacy:** the settings rows in `settings.rows` (compiled
  into protoHDMIcapture, read by legacy at run time), the settings file,
  `capture-cards/` and `sources/`. `tools/check` runs protoHDMIcapture's
  `--settings` over `tools/fixtures/settings/` and checks the README's
  settings table and `config.example` against the rows.
- **Checks:** `tools/check` (lint and syntax, legacy's included,
  consistency between code and docs, protoHDMIcapture's formatting, build, tests and
  clippy, and the hardware-free harnesses in `tools/harness/`; each check
  prints its name); enable the hook per clone with
  `git config core.hooksPath .githooks`. Legacy's own harnesses
  (`legacy/tests/`) run by hand only.
- **Building:** run cargo from inside the checkout. Cargo reads
  `.cargo/config.toml` (the static C runtime on Windows) only from the
  working directory up, so a build started outside it with
  `--manifest-path` makes an exe that needs the Visual C++ runtime.
- **Commits and releases:** [`CONTRIBUTING.md`](CONTRIBUTING.md).
  Every commit subject is a Conventional Commit (`tools/commit-subject`;
  the `commit-msg` hook and CI enforce it); its type picks the next
  semver version. Releases are automatic (release-please); never edit
  the version, `CHANGELOG.md` or a tag by hand.
- **Verifying on hardware:** read [`docs/testing.md`](docs/testing.md) in
  full before a real run: its traps, and how to drive the window yourself
  (keys via hyprctl and wtype, screenshots), so the user does only the
  physical steps.
- **Session files** (`$XDG_RUNTIME_DIR/protohdmicapture/`): the contract
  between protoHDMIcapture, legacy, the wizard, the dashboard and the mpv
  script is
  [`docs/run-dir.md`](docs/run-dir.md).
- **Language:** [`GLOSSARY.md`](GLOSSARY.md) names the project's terms;
  code, docs, commits and issues use them.
- **Reviewing:** [`CODING_STANDARDS.md`](CODING_STANDARDS.md).
