# protoHDMIcapture

A low-latency viewer for an HDMI source through a cheap USB capture card:
the picture and the sound, nothing recorded or streamed. The product is the
Rust viewer in `viewer/` (Linux and Windows), which the releases ship.
`view.sh` + mpv (`view.sh`, `launch.sh`, `lib/`, `desktop/`) is the legacy
alternative, Linux only, until it retires (#10).

- **State:** epic #10 and its sub-issues hold what is done, open and
  decided; read its status comment before planning.
- **Generic:** code and docs serve any user's setup. One machine's values
  (its card serial, monitor, paths) live in that user's settings file, and
  examples use placeholders (`/dev/videoN`, `DP-1`). The repo is public.
- **Shared by both viewers:** the settings rows in `lib/settings.sh`, the
  settings file, `capture-cards/` and `sources/`. A settings change lands in
  both; `tools/check` diffs their `--settings` over `tools/fixtures/settings/`
  and checks the README's settings table against the rows.
- **Checks:** `tools/check` (lint, syntax, consistency between code and
  docs, the viewer's build, tests and clippy, and the hardware-free
  harnesses in `tools/harness/`; each check prints its name); enable the
  hook per clone with `git config core.hooksPath .githooks`.
- **Building:** run cargo from `viewer/`. Cargo reads
  `viewer/.cargo/config.toml` (the static C runtime on Windows) only from
  the working directory up, so `--manifest-path` from the root builds an exe
  that needs the Visual C++ runtime.
- **Releasing:** a `v<version>` tag, matching `viewer/Cargo.toml`, runs
  `.github/workflows/release.yml` (packaging: #14).
- **Verifying on hardware:** read [`docs/testing.md`](docs/testing.md) before
  a real run; it lists the traps that fake failures.
- **Session files** (`$XDG_RUNTIME_DIR/protohdmicapture/`): the contract
  between both viewers, the wizard, the dashboard and the mpv script is
  [`docs/run-dir.md`](docs/run-dir.md).
- **Language:** [`GLOSSARY.md`](GLOSSARY.md) names the project's terms;
  code, docs, commits and issues use them.
- **Reviewing:** [`CODING_STANDARDS.md`](CODING_STANDARDS.md).
