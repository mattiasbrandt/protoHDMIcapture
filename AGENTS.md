# protoHDMIcapture

A low-latency viewer for an HDMI source through a cheap USB capture card:
the picture and the sound, nothing recorded or streamed. The product is
protoHDMIcapture, the Rust program in `viewer/` (Linux and Windows), which the
releases ship. protoHDMIcapture legacy, `view.sh` + mpv in `legacy/`, is
kept best effort, Linux only, and is not retired
([ADR 0002](docs/adr/0002-legacy-kept-best-effort.md)): nothing in
protoHDMIcapture depends on it, no check holds it to parity, and a change to
protoHDMIcapture need not touch it.

- **State:** epic #10 and its sub-issues hold what is done, open and
  decided; read its status comment before planning.
- **Generic:** code and docs serve any user's setup. One machine's values
  (its card serial, monitor, paths) live in that user's settings file, and
  examples use placeholders (`/dev/videoN`, `DP-1`). The repo is public.
- **Shared with legacy:** the settings rows in `settings.rows` (compiled
  into protoHDMIcapture, read by legacy at run time), the settings file,
  `capture-cards/` and `sources/`. `tools/check` runs protoHDMIcapture's
  `--settings` over `tools/fixtures/settings/` and checks the README's
  settings table and `config.example` against the rows.
- **Checks:** `tools/check` (lint and syntax, legacy's included,
  consistency between code and docs, protoHDMIcapture's build, tests and
  clippy, and the hardware-free harnesses in `tools/harness/`; each check
  prints its name); enable the hook per clone with
  `git config core.hooksPath .githooks`. Legacy's own harnesses
  (`legacy/tests/`) run by hand only.
- **Building:** run cargo from `viewer/`. Cargo reads
  `viewer/.cargo/config.toml` (the static C runtime on Windows) only from
  the working directory up, so `--manifest-path` from the root builds an exe
  that needs the Visual C++ runtime.
- **Releasing:** a `v<version>` tag, matching `viewer/Cargo.toml`, runs
  `.github/workflows/release.yml` (packaging: #14).
- **Verifying on hardware:** read [`docs/testing.md`](docs/testing.md) before
  a real run; it lists the traps that fake failures.
- **Session files** (`$XDG_RUNTIME_DIR/protohdmicapture/`): the contract
  between protoHDMIcapture, legacy, the wizard, the dashboard and the mpv
  script is
  [`docs/run-dir.md`](docs/run-dir.md).
- **Language:** [`GLOSSARY.md`](GLOSSARY.md) names the project's terms;
  code, docs, commits and issues use them.
- **Reviewing:** [`CODING_STANDARDS.md`](CODING_STANDARDS.md).
