# protoHDMIcapture

- **Checks:** `tools/check` (lint, syntax, consistency between code and
  docs, and the hardware-free harnesses in `tools/harness/`; each check
  prints its name); enable the hook per clone with
  `git config core.hooksPath .githooks`.
- **Verifying on hardware:** read [`docs/testing.md`](docs/testing.md) before
  a real run; it lists the traps that fake failures.
- **Session files** (`$XDG_RUNTIME_DIR/protohdmicapture/`): the contract
  between view.sh, launch.sh, the wizard, the dashboard and the mpv script
  is [`docs/run-dir.md`](docs/run-dir.md).
- **Language:** [`GLOSSARY.md`](GLOSSARY.md) names the project's terms;
  code, docs, commits and issues use them.
- **Reviewing:** [`CODING_STANDARDS.md`](CODING_STANDARDS.md).
