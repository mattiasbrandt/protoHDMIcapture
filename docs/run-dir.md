# Run directory

Every process of a session talks through files in
`$XDG_RUNTIME_DIR/protohdmicapture/` (cleared at logout). This table is the
contract; change a writer or a reader, update the row.

| File | Written by | Read by | Meaning |
|---|---|---|---|
| `video.lock` | `view.sh` (flock, held while a player runs) | `view.sh`, setup sync stage (probe only) | one video window at a time |
| `video.pid` | `view.sh` `run_player`; removed in its EXIT trap (left by a killed session) | `view.sh` (second launch focuses it), `--place-window`, dashboard, setup sync readiness | PID of the **player** (mpv/gst/ffplay), not of `view.sh` |
| `audio.lock` | `view.sh` (flock, held while pw-loopback runs) | `view.sh`, setup sync stage (probe only) | one audio loopback at a time |
| `audio.pid` / `audio.env` | `view.sh` `start_audio`: `audio.pid` at start, `audio.env` only once the loopback is linked (both removed by `stop_audio`) | dashboard, `debug_after_start`, mpv script (`audio.env`: confirms a sync change) | loopback PID; `DELAY`, `LAT_MS`, `SRC` |
| `audio.log` | `view.sh` `start_audio` (the loopback's stdout and stderr, new each start) | `audio_failed` (its last lines go in the warning), the user | why a loopback failed |
| `audio.failed` | `view.sh` `audio_failed` (removed by the next `start_audio`) | mpv script (the overlay's Audio line) | the loopback is not running: it exited, or found no source to link to; one line, the reason |
| `audio.request` | mpv script (overlay's ctrl/alt −/+), written as `.tmp` and renamed, then USR1 to `view.sh` (its parent, PID passed as a script option and checked against mpv's own parent first) | `view.sh` `apply_sync_request`, which claims it by renaming to `.claimed`, then removes it | the wanted `DELAY`; `view.sh` restarts its own loopback with it |
| `audio.ctl.lock` | `lib/audio.sh` (flock, held while one control runs) | `view.sh --audio`, `audio_state_apply` | one mute/volume change or restore at a time |
| `audio.state` | `view.sh --audio` (overlay's m, 9, 0); cleared by `take_audio_lock` at a session's start | `start_audio` (`audio_state_apply`) | the session's loopback `VOLUME` and `MUTE`, re-applied to a restarted loopback; written via `audio.state.tmp` and renamed |
| `backend`, `ipc.path` | `view.sh` `run_player`; removed in its EXIT trap (left by a killed session) | dashboard | which player; mpv IPC socket (absent for gst/ffplay) |
| `refresh.restore` | `desktop_refresh_match` (hyprland), before it switches; removed by `desktop_refresh_restore` | `view.sh` at start, with the video lock held (`desktop_refresh_recover`) | `monitor\|rate set\|original rate\|restore command`: a switch a killed session never undid; restored only if the monitor is still at the rate set |
| `window.mode`, `window.size` | `view.sh` `run_player`, `--place-window`; removed in its EXIT trap (left by a killed session) | `--place-window` | current `WINDOW` and `WINDOW_SIZE` for the f key |
| `player.quit` | mpv script `on_unload` hook (via `quitfile`), then USR2 to `view.sh` (checked as for `audio.request`) | `view.sh` after the player exits | mpv began to quit; a later SIGKILL is `view.sh`'s 2 s quit bound (teardown hung without frames), another signal death is the upstream teardown crash |
| `debug.log` | `view.sh` with `DEBUG≠off` (tee) | dashboard | the session's full output |
| `mpv.sock` | mpv, when `DEBUG=window` and `IPC=""` | dashboard | private IPC socket for that session |
| `setup.lock` | `lib/setup.sh` (flock, held while it runs) | `launch.sh` (blocks on it) | one wizard; "wizard ended" however it ended |
| `setup.pid` | `lib/setup.sh` | `launch.sh` | the wizard started |
| `setup.done` | `lib/setup.sh` EXIT trap | `launch.sh`, `view.sh` first run | `saved` or `cancelled` |
| `setup-preview.config` | `lib/setup.sh` `run_child` | children of the wizard (`CONFIG_FILE`) | the config as it would be saved |
| `tune.out` | `view.sh --tune` (`TUNE_OUT`) on q | setup sync stage | the kept `DELAY` |
| `setup-video.log`, `setup-test.log` | wizard children | setup (shown on failure) | child output |

The menu run log is `$XDG_RUNTIME_DIR/protohdmicapture.log` (one level up),
written by `launch.sh`.

## Rules

- A PID read from this directory may belong to another session by the time
  it is read. Signal only processes you started (keep `$!`) or whose
  ownership you verified (e.g. its parent is your child).
- A lock is held by an open fd, and children inherit fds. A helper that may
  outlive its parent closes `VIDEO_LOCK` / `AUDIO_LOCK` first (see
  `run_player`).
