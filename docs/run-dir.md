# Run directory

Every process of a session talks through files in
`$XDG_RUNTIME_DIR/protohdmicapture/` (cleared at logout; protoHDMIcapture on
Windows uses `%LOCALAPPDATA%\protohdmicapture`). This table is the
contract; change a writer or a reader, update the row. `view.sh`,
`launch.sh`, `legacy/lib/` and the mpv script are protoHDMIcapture legacy's, in
`legacy/`; best effort (ADR 0002), so its rows describe it as it is, not a
promise.

| File | Written by | Read by | Meaning |
|---|---|---|---|
| `video.lock` | `view.sh` (flock, held while a player runs); protoHDMIcapture (held for its whole run, `src/session.rs`; its wizard only while its window shows the feed) | `view.sh`, protoHDMIcapture, setup sync stage (probe only) | one video window at a time, across protoHDMIcapture and legacy |
| `video.pid` | `view.sh` `run_player`; removed in its EXIT trap (left by a killed session); protoHDMIcapture (its own PID, removed at its end) | `view.sh` and protoHDMIcapture (a second launch focuses it; with the video lock held, both stop the orphan player it names), `--place-window`, dashboard, setup sync readiness | PID of the **player** (mpv/gst/ffplay, or protoHDMIcapture, which is its own player), not of `view.sh` |
| `audio.lock` | `view.sh` (flock, held while pw-loopback runs) | `view.sh`, setup sync stage (probe only) | one audio loopback at a time (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `audio.pid` / `audio.env` | `view.sh` `start_audio`: `audio.pid` at start, `audio.env` only once the loopback is linked (both removed by `stop_audio`) | dashboard, `debug_after_start`, mpv script (`audio.env`: confirms a sync change) | loopback PID; `DELAY`, `LAT_MS`, `SRC` (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `audio.log` | `view.sh` `start_audio` (the loopback's stdout and stderr, new each start) | `audio_failed` (its last lines go in the warning), the user | why a loopback failed (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `audio.failed` | `view.sh` `audio_failed`; removed by the next `start_audio` and, by the audio lock holder, at the session's end | mpv script (the overlay's Audio line, the sync confirmation), dashboard | the loopback is not running: it exited, found no source, or linked to another source than the card's; one line, the reason (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `audio.request` | mpv script (overlay's ctrl/alt −/+), written as `.tmp` and renamed, then USR1 to `view.sh` (its parent, PID passed as a script option and checked against mpv's own parent first) | `view.sh` `apply_sync_request`, which claims it by renaming to `.claimed`, then removes it | the wanted `DELAY`; `view.sh` restarts its own loopback with it (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `audio.ctl.lock` | `legacy/lib/audio.sh` (flock, held while one control runs) | `view.sh --audio`, `audio_state_apply` | one mute/volume change or restore at a time (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `audio.state` | `view.sh --audio` (overlay's m, 9, 0), written as `.tmp` and renamed; cleared by `take_audio_lock` at a session's start | `start_audio` (`audio_state_apply`) | the session's loopback `VOLUME` and `MUTE`, re-applied to a restarted loopback; written via `audio.state.tmp` and renamed (view.sh only; protoHDMIcapture keeps this in-process, `src/audio.rs` `Loopback`, #29) |
| `backend`, `ipc.path` | `view.sh` `run_player`; removed in its EXIT trap (left by a killed session) | dashboard | which player; mpv IPC socket (absent for gst/ffplay) |
| `refresh.restore` | `desktop_refresh_match` (hyprland), before it switches; removed by `desktop_refresh_restore`; protoHDMIcapture's hyprland adapter likewise (`src/desktop.rs`) | `view.sh` at start, with the video lock held (`desktop_refresh_recover`); protoHDMIcapture the same way, and its dashboard's Display section (which monitor was switched, and to what) | `monitor\|rate set\|original rate\|restore command`: a switch a killed session never undid; restored only if the monitor is still at the rate set. Removed only once settled (restored, or changed by hand); while it stands, `REFRESH=match` does not switch |
| `window.mode`, `window.size` | `view.sh` `run_player`, `--place-window`; removed in its EXIT trap (left by a killed session) | `--place-window` | current `WINDOW` and `WINDOW_SIZE` for the f key |
| `player.quit` | mpv script `on_unload` hook (via `quitfile`), then USR2 to `view.sh` (checked as for `audio.request`) | `view.sh` after the player exits | mpv began to quit; a later SIGKILL is `view.sh`'s 2 s quit bound (teardown hung without frames), another signal death is the upstream teardown crash |
| `dashboard.sock` | protoHDMIcapture (`src/dashboard.rs`), bound with the video lock held, replacing a killed viewer's; removed at its end if still its own | `protohdmicapture --dashboard` | Unix socket: one JSON line a second with the viewer's live numbers; a dashboard that stops reading is dropped, never waited for. view.sh's dashboard reads the files above instead. On Windows not a file: the named pipe `\\.\pipe\protohdmicapture-dashboard-<run directory, \ and : as ->`, its owner and SYSTEM only, gone with the viewer |
| `debug.log` | `view.sh` with `DEBUG≠off` (tee); protoHDMIcapture with `DEBUG≠off` (its stdout and stderr, `[dbg]` lines and a perf line every `DEBUG_PERF_INTERVAL` s; how the run ended). Both truncate it once `video.lock` is held | view.sh's dashboard, the user | the session's full output |
| `mpv.sock` | mpv, when `DEBUG=window` and `IPC=""` | dashboard | private IPC socket for that session |
| `setup.lock` | `legacy/lib/setup.sh` (flock, held while it runs); protoHDMIcapture's wizard likewise (`--setup` or a first run, `src/main.rs`) | `launch.sh` (blocks on it) | one wizard; "wizard ended" however it ended |
| `setup.pid` | `legacy/lib/setup.sh`; protoHDMIcapture's wizard (its own PID) | `launch.sh` | the wizard started |
| `setup.done` | `legacy/lib/setup.sh` EXIT trap; protoHDMIcapture's wizard when it ends | `launch.sh`, `view.sh` first run | `saved` or `cancelled` |
| `setup-preview.config` | `legacy/lib/setup.sh` `run_child` | children of the wizard (`CONFIG_FILE`) | the config as it would be saved |
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
