# Run directory

Every process of a session talks through files in
`$XDG_RUNTIME_DIR/protohdmicapture/` (cleared at logout). This table is the
contract; change a writer or a reader, update the row.

| File | Written by | Read by | Meaning |
|---|---|---|---|
| `video.lock` | `view.sh` (flock, held while a player runs) | `view.sh`, setup sync stage (probe only) | one video window at a time |
| `video.pid` | `view.sh` `run_player` | `view.sh` (second launch focuses it), `--place-window`, dashboard, setup sync readiness | PID of the **player** (mpv/gst/ffplay), not of `view.sh` |
| `audio.lock` | `view.sh` (flock, held while pw-loopback runs) | `view.sh`, setup sync stage (probe only) | one audio loopback at a time |
| `audio.pid` / `audio.env` | `view.sh` `start_audio` (removed by `stop_audio`) | dashboard, `debug_after_start` | loopback PID; `DELAY`, `LAT_MS`, `SRC` |
| `backend`, `ipc.path` | `view.sh` `run_player` | dashboard | which player; mpv IPC socket (absent for gst/ffplay) |
| `window.mode`, `window.size` | `view.sh` `run_player`, `--place-window` | `--place-window` | current `WINDOW` and `WINDOW_SIZE` for the f key |
| `player.quit` | mpv script `shutdown` event (via `quitfile`) | `view.sh` after the player exits | mpv began a normal quit; a later signal death is its teardown bug |
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
