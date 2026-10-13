# protoHDMIcapture

- **Checks:** `tools/check` (shellcheck, Lua/Python syntax, banned patterns);
  enable the hook per clone with `git config core.hooksPath .githooks`.
- **Verifying on hardware:** read [`docs/testing.md`](docs/testing.md) before
  a real run; it lists the harness traps that fake failures.
- **Session files** (`$XDG_RUNTIME_DIR/protohdmicapture/`): the contract
  between view.sh, launch.sh, the wizard, the dashboard and the mpv script
  is [`docs/run-dir.md`](docs/run-dir.md).
- **Reviewing:** [`CODING_STANDARDS.md`](CODING_STANDARDS.md).
