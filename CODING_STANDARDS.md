# Coding standards

Rules for reviewing a change to this repo. Mechanical ones are enforced by
`tools/check`; these are the judgement calls it cannot make.

1. **Every non-zero exit is handled.** When an external command can fail,
   every non-zero status is a failure with its error shown, not only the
   expected one (e.g. `timeout`'s 124). A measurement is computed only from
   a run that succeeded.
2. **Signal only processes you own.** Keep `$!` of what you start, or verify
   ownership (its parent is your child) before signalling; a PID read from
   the shared run directory may belong to another session. See
   [`docs/run-dir.md`](docs/run-dir.md).
3. **A setting whose empty value means something carries the `empty` flag**
   in `lib/settings.sh` (`IPC=` disables the socket). Without it, an empty
   value counts as unset and falls back to the saved setting, then the
   default. Outside the settings, the same rule: `${X-default}` when empty
   means something, `${X:-default}` only when it should mean the default.
4. **Cleanup reaches every path.** Anything a session starts (player,
   loopback, refresh switch, preview feed) is undone from its EXIT trap, and
   a background job is stopped explicitly: it ignores SIGINT.
