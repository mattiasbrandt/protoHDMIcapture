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
   [`docs/run-dir.md`](docs/run-dir.md). One exception: an orphan a killed
   session left running (legacy's `view.sh` `kill_orphan`, and
   protoHDMIcapture's in `src/session.rs`), signalled only while
   holding the lock its session held, and only if it is the user's own and
   its command line is ours.
3. **A setting whose empty value means something carries the `empty` flag**
   in `settings.rows` (`IPC=` disables the socket). Without it, an empty
   value counts as unset and falls back to the saved setting, then the
   default. Outside the settings, the same rule: `${X-default}` when empty
   means something, `${X:-default}` only when it should mean the default.
4. **Cleanup reaches every path.** Anything a session starts (player,
   loopback, refresh switch, preview feed) is undone from its EXIT trap
   (protoHDMIcapture: `Drop`), and
   a background job is stopped explicitly: it ignores SIGINT.
5. **A lock stays with the script that holds it.** `exec {fd}>` is inherited
   by every child, so each child closes the script's lock fds first:
   players, background helpers, process substitutions (a stderr filter or
   a `tee` lives as long as the pipe it reads), and the wizard's children
   (`setup.lock`). A child that outlives its parent keeps the lock held
   otherwise. Legacy's view.sh: `_close_locks`; its wizard: `run_child`.
6. **A record is written or removed after what it records.** A file that
   says something is up (`audio.env`) is written once it is checked, and
   a check that fails comes before it, before any announcing line and
   before slower follow-up work. A file that undoes something
   (`refresh.restore`) is removed only after the undo succeeded, so a
   failure leaves it for the next run.
