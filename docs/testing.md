# Verifying a change

`tools/check` (also the pre-commit hook, which checks the staged tree:
with unstaged or untracked changes, a copy in `.git/precommit-tree`) plus
real runs against the card.
`tools/check` includes `tools/harness/`: hardware-free checks of the
functions behind past defects. Fixing one that a stand-in can reach, add a
harness or a test, and make it fail with the bug put back before trusting
it. protoHDMIcapture legacy's harnesses are in `legacy/tests/`
(`lib.sh` there lifts functions out of view.sh and lib/setup.sh, which run
when sourced); no check runs them (ADR 0002). Below: the traps that
produce false results on real runs; the ones that name view.sh, mpv,
gst or ffplay are legacy's.

## Before a run

- **Is the source awake?** A sleeping console delivers no frames, which looks
  like a code bug (gst/ffplay open no window; the signal check fails):

  ```bash
  v4l2-ctl --list-devices                     # the card's node: it moves on a replug
  timeout -k 2 6 v4l2-ctl -d /dev/videoN --stream-mmap --stream-count=60 --stream-to=/dev/null; echo $?
  ```

  124 means no frames; 1 ("Cannot open device") means the node is gone,
  not that the source sleeps. To test the no-frames case on purpose: after
  the PS5 enters rest mode the MS2130 streams its own "No Signal" box for
  about a minute (a picture, by the glossary), then no frames, which can
  flicker back for a few seconds. Check right before each run.
- **Pulling the card's USB ends the feed first**, before anything that
  watches the card audio input alone (the audio sync stage's loopback,
  the feed's dead loopback) sees it. To stop only the card audio input,
  switch its sound-card profile off and back:

  ```bash
  pactl list cards short                                  # its alsa_card.usb-… name
  pactl set-card-profile alsa_card.usb-<id> off            # gone: "defined target not found"
  pactl set-card-profile alsa_card.usb-<id> input:analog-stereo
  ```
- **Bare mpv is not view.sh's mpv for signals.** view.sh passes
  `--input-terminal=no`, which leaves SIGTERM fatal; a bare mpv without it
  catches TERM and quits cleanly, so the two behave differently.

- **Frames are not a picture.** The MS2109 streams uniformly black frames at
  full rate with nothing plugged in; grab one before trusting a frame count.
- **Bound ffmpeg probes with `timeout -k`.** ffmpeg blocked in a V4L2 read on
  a card with no source ignored SIGTERM and outlived a plain `timeout`.

- **Protect the real config** when testing first run or the wizard. Point
  `CONFIG_FILE` at a scratch copy, or move it aside with a restoring trap:

  ```bash
  C=~/.config/protohdmicapture/config; mv $C $C.bak; trap 'mv -f $C.bak $C' EXIT
  ```

## Driving the interactive parts

- **Legacy's wizard** reads a terminal, so run it in a pty and pipe the
  answers:

  ```bash
  printf '\n\n\n...y\n' | CONFIG_FILE=$scratch timeout 60 script -qec 'legacy/view.sh --setup' out.ts >/dev/null
  ```

  Count the answers against the stages actually shown. The card stage asks
  once more for each other video device not yet ignored (a webcam), so the
  count depends on what is plugged in and on `IGNORE_CARDS_RE`. When the piped input
  runs out the pty stays open, so the next prompt **blocks** until the
  timeout rather than failing: a "hang" usually means one answer too few.
- **Keys in the mpv window** go through its IPC socket:
  `echo '{"command":["keypress","f"]}' | socat - /tmp/mpv-protohdmicapture.sock`.
- **Keys in protoHDMIcapture's window** have no socket: focus it, then type.
  Omarchy's Hyprland takes Lua dispatches only, and the old
  `dispatch focuswindow` form fails without a word:

  ```bash
  hyprctl dispatch 'hl.dsp.focus({ window = "class:^protohdmicapture$" })'
  wtype -k Tab          # ctrl+=: wtype -M ctrl -k equal -m ctrl
  ```

  Focus follows the mouse: the dispatch holds for one key, then focus goes
  back under the pointer, so focus again before every key (a wizard walk
  lost three keys of four). A key the user types lands in the window under
  the pointer, and their own typing can reach the viewer mid-run (it
  closed the panel in a 10-minute overlay-open run). Screenshot the panel
  now and then to prove a long run's state.
- **`--setup` takes no other flag** (no `--monitor`): focus the monitor
  first, `hyprctl dispatch 'hl.dsp.focus({ monitor = "DP-1" })'`.
- **See the overlay** by screenshotting the window with its OSD, then reading
  the PNG: `{"command":["screenshot-to-file","/path/shot.png","window"]}`.
  The MS2109 with no source streams black, so the panel reads cleanly.
  With no frames at all (the no-signal banner) mpv has nothing to
  screenshot; take the window from the compositor instead:
  `grim -g "$(hyprctl clients -j | jq -r --argjson p "$(cat $XDG_RUNTIME_DIR/protohdmicapture/video.pid)" '.[] | select(.pid==$p) | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" shot.png`.
- **Toggle keys an even number of times** in a long run: an odd count of
  Tab leaves the overlay open, which hides the no-signal banner.
- **Volume persists** through WirePlumber by stream name: a test that
  changes the loopback's volume leaves the next real session at it. Put it
  back to 100 % before finishing.
- **The menu path**: `gtk-launch protohdmicapture`. A stand-in
  `xdg-terminal-exec` first on `PATH` can run the wizard with scripted answers.
  Legacy's entry (`legacy/desktop/`) runs `legacy/launch.sh` (view.sh);
  packaging's runs `protohdmicapture --menu`. Reach that path the
  way a menu starts it: `setsid protohdmicapture --menu </dev/null >log 2>&1 &`
  (no terminal, so it notifies).
- **A new window takes the fullscreen** of a fullscreen viewer on its
  workspace (Hyprland): any terminal opened there hides the feed, and a
  hidden window presents nothing, which reads as a viewer bug.

## Measuring latency

- **Compare builds, not sessions:** alternate the builds on the same
  monitor and card, two runs each, `--stats --seconds 40`; one run's median
  moves with the capture/display phase. Build an old commit in a scratch
  worktree with its own `CARGO_TARGET_DIR`.
- **Pin the monitor's mode first.** `REFRESH=match` switches a 100 Hz
  monitor to about 60 Hz, and builds before it ran at the monitor's own rate
  (about 30 ms, against 41 at 60 Hz): a bisect across it measures the
  monitor. `hyprctl eval 'hl.monitor({ output = "DP-1", mode = "3440x1440@59.97", position = "0x0", scale = 1 })'`,
  and back afterwards.
- **`--monitor NAME`** picks the monitor REFRESH=match switches and the
  window opens on; without it, both follow focus.
- **The baseline** (2026-10-16, `78951f6`, MS2130 1080p60, DP-1 at
  59.97 Hz, 2 min): kernel → presented median
  44.9 ms, p99 53.7 ms, 2 repeated refreshes. The dashboard colours
  kernel → presented against it (plus 16.7 ms while frame generation makes
  frames; `src/stats.rs`). Re-measure against it after a
  change to pacing, the event loop or the present path; count the repeated
  refreshes too (they come in bursts every ~37 s at 60.00 against 59.97 Hz,
  so run at least 2 minutes). `27c31d3` (draw in `about_to_wait`, two-pass
  scalers) matched it on 2026-10-17, against `0251c13` on DP-2 at 59.95 Hz
  (#19).
- **Check that the scaler runs.** The `scaling:` line names the filter;
  1080p fullscreen on a 1920x1200 monitor is 1:1, a bilinear sampler and
  no filter. A windowed 1280x720 runs mitchell; an upscale needs a bigger
  monitor, or `--test-pattern` with `RES=1280x720` fullscreen (spline36).
- **`s` screenshots are taken before scaling**, at the capture's size: to
  compare scalers, grab the window from the compositor (`grim`).
- **The MS2109 sends about 7 s of black** after every stream start, with a
  source playing: a shorter probe sees only black.
- **Windows: mains or battery?** An idle laptop on mains loses about one
  card frame a second; on battery, a third as many (#32). Check before a
  capture count: `(Get-CimInstance -Namespace root/wmi BatteryStatus).PowerOnline`.
  Load hides the losses too: a heavy scaler or a busy core makes a run
  look clean, so name the scaling and what else ran. The idle case to
  measure: fullscreen at 1:1, `CARD_SERIAL` set, `FMT=yuyv422`,
  `--stats --seconds 60`, nobody at the keyboard; 60 s runs swing from
  about 20 to 70 gaps, so alternate and compare several.
- **Windows power plan changes for a test:** `powercfg /setacvalueindex
  scheme_current sub_processor IDLEDISABLE 1` then `powercfg /setactive
  scheme_current`; put the value back in a `finally`. Hidden settings
  such as IDLEDISABLE show only with `powercfg /qh`, not `/query`.

## Reading GPU use (#72)

- **Only with a GPU enhancement on**: frame generation (`--framegen`) or an
  enhancement SCALING preset. A plain run samples and shows nothing new.
- **The dashboard's GPU section**, the last 10 s at 10 Hz, as nvtop and
  amdgpu_top read them:
  - `busy`, `clocks`, `VRAM`, `power`, `temperature` and `fan`: the whole
    card, from amdgpu's sysfs and hwmon (other drivers: "not available",
    with why);
  - `protoHDMIcapture`: its own share of the GPU's engines and its VRAM,
    from its DRM file descriptors (`/proc/self/fdinfo`, `drm-engine-*`).
- **`--stats`**:
  - each 10 s line ends with the GPU's busy %, ours, the power and the
    junction temperature;
  - the summary adds `GPU use` (each number's median and max over the run)
    and `GPU share of a refresh` (the passes' GPU times against the
    refresh interval: how much of the frame budget protoHDMIcapture
    spends; under 10 % at 1080p60 on a gaming GPU leaves headroom for
    picture quality).
- **`refreshes between presents`** (`--stats` summary): how many refreshes
  passed from one present to the next. A 60 fps feed is all 1s; a 30 fps
  source in new-frames-only all 2s. A 3 next to a 1 is a present that
  landed a refresh late and the next one on time: judder, and one
  repeated refresh.
- **`latency: not measurable`**: every present was discarded while the
  compositor's frame callbacks kept coming, so something presents over
  ours before display. The openfg layer does: inside our present it
  commits its generated frame first, carrying our feedback and frame
  callback, then the real frame over it in the same refresh (Mailbox), so
  our feedback is always "discarded" and its generated frame never shows.

## GameMode (#77)

With amdgpu's power level on `auto`, the driver clocks the GPU only as high
as a paced 60 Hz load needs, so a GPU enhancement takes longer paced than
back to back (#75's upscaler: 7.2 ms against 12.4 ms). While an
enhancement is on, protoHDMIcapture asks Feral's GameMode for performance
(`RegisterGame` over D-Bus) and unregisters when it turns off or the
window closes. A plain run never registers.

- **What it changes is GameMode's config, never protoHDMIcapture's.** Its
  CPU settings (governor, renice, core pinning) come from any
  `gamemode.ini`. Its GPU part is read only from **`/etc/gamemode.ini`**:
  gamemoded ignores `[gpu]` in `~/.config/gamemode.ini` ("not configurable
  from unsafe config files"). To let it raise an AMD GPU's clocks:

  ```ini
  [gpu]
  apply_gpu_optimisations=accept-responsibility
  gpu_device=<n>          ; the N of /sys/class/drm/cardN
  amd_performance_level=high
  ```
- **To check it**, with an enhancement on: the dashboard's GPU section has
  a `GameMode` row (`active for protoHDMIcapture`, `not available: …`, or a
  warning) under the clocks; `gamemoded -s` says `gamemode is active`; and
  `cat /sys/class/drm/card<n>/device/power_dpm_force_performance_level`
  reads `high` (back to `auto` once it is off). With `--stats` stderr says
  `gamemode: active for protoHDMIcapture`.
- **No GameMode** (not installed, not running, not Linux): nothing
  happens; `--stats` says `note: GameMode not available: …` once.
- **The ignored test** registers this process with the running gamemoded:
  `tools/heavy cargo test registers_with_the_running_gamemoded -- --ignored`.

## Real-time audio (#76)

The dashboard's `priority` line (orange) says the audio thread was
refused real-time scheduling; the sound may crackle under load. To check
a run:

- RTKit runs: `busctl get-property org.freedesktop.RealtimeKit1 /org/freedesktop/RealtimeKit1 org.freedesktop.RealtimeKit1 MaxRealtimePriority RTTimeUSecMax`
  answers (here `i 20`, `x 200000`).
- The threads: `ps -eLo pid,tid,cls,rtprio,comm` shows `FF` or `RR` with a
  priority for a real-time thread, `TS` for an ordinary one; RTKit's
  journal (`journalctl -u rtkit-daemon`) logs each grant.
- protoHDMIcapture sets `DISABLE_RTKIT=1` for its own libpipewire: with
  RTKit, PipeWire 1.6.9's client module clamps the process's
  `RLIMIT_RTTIME` to 0 (`PIPEWIRE_DEBUG=mod.rt:5 pw-mon` shows "clamping
  rt.time.soft … to 0 because of RTKit"), and cpal's own RTKit promotion
  then cannot set the limit it needs (`setrlimit` EPERM). The `--ignored`
  test `pipewire_clients_rttime_limit_with_and_without_rtkit` shows both.
- PipeWire's own server needs RTKit at its start: one started before RTKit
  was reachable logs `mod.rt: RTKit error: …ServiceUnknown` and runs its
  data loop as `TS` until restarted.

## Recording a clip (dev builds)

For trying frame generation and upscaling on the same card frames every
time (#63, #64), a dev build records them while its window shows the feed
(#79), and replays them. A release build has neither flag:
protoHDMIcapture does not record.

- **Build:** `cargo build --features dev`.
- **Record:** `target/debug/protohdmicapture --record NAME --seconds N`.
  It is a normal run with its window (the sound, the keys, the overlay,
  the dashboard and `--stats` as usual), so the player can play on it
  with no screen on the card's loop-out. The card is the one a normal run
  opens, at RES, FPS and FMT; add `--test-pattern` to record the pattern
  instead. With `--framegen` the window generates frames; the clip holds
  the card's frames only. It ends at `--seconds`, or earlier when the
  window closes (q, Ctrl-C), with the report and the metadata for what
  was recorded; a run that recorded no frame leaves no clip. It is a card
  run: brief it and wait for a go, like any other.
- **Where clips go:** `/var/tmp/protohdmicapture-clips/NAME.clip`. That is
  on disk, and systemd empties it after 30 days; `/tmp` is RAM-backed on
  many desktops. An existing clip is never written over.
- **Size:** the frames are raw YUYV, about 250 MB a second at 1080p60, so
  a 20 s clip is about 5 GB. The recording is refused, with both numbers,
  when it would not fit with 1 GB to spare.
- **Metadata:** `NAME.json` beside the clip: the version, when it was
  recorded, the seconds asked, `--note "TEXT"` (say what was on screen,
  e.g. "GTA 6, PS5 Pro, standing still"), the card's name and USB id, the
  format, the card and picture settings, the totals, and one entry per
  written frame (index, capture and dequeue times, mean luma, new or
  duplicate, and the duplicate detector's scores for tuning its
  thresholds). A replay does not need it; copy it along with a clip you
  keep.
- **The recording never costs the picture:** the capture thread gives
  each frame to the window first, then only copies it into a queue of
  about 2 s of frames (some 500 MB at 1080p60) for a writer thread. A disk
  behind for longer loses frames from the clip, never from the window:
  counted, and left as gaps in the clip. A failed write (a full disk) ends
  the recording with a warning; the window goes on, and the run exits
  with the error. To check it on the card, compare `--stats`'
  kernel → presented against a plain run's.
- **The report** at the end gives the frames written, the capture gaps,
  any frames not written, the length, the mean luma per second, and the
  share of new frames, so a game that went idle or dimmed shows (a
  warning when nearly every frame is a duplicate).
- **Replay:** `target/debug/protohdmicapture --replay FILE`. It plays the
  normal window at the clip's own size and rate, looping, with `--stats`,
  `--seconds` and the dashboard as usual; RES, FPS and FMT are not used.
- **Keep a clip** by copying it to `~/.local/share/protohdmicapture/clips/`.
  Clips are game footage and gigabytes: never commit one.

## Scoring frame generation on a clip (dev builds, #81)

How close a generator's in-between picture comes to a real one, on a
kept clip, headless: no card, no window, no monitor.

```bash
target/release/protohdmicapture --framegen-score CLIP --framegen MODE \
    [--every N] [--worst K] [--out DIR] [--hud X,Y,W,H]
```

- **The test:** three new frames in a row, N0, N1 and N2 (duplicates
  skipped by the recorder's detector). The generator makes the picture
  halfway between N0 and N2, and it is compared with the real N1. Twice
  the live gap, so harsher than the window, but the same for every
  approach. The triples are chained (N0 N1 N2, then N2 N3 N4), so a
  generator's state from pair to pair carries as in the window; a gap in
  the clip starts the chain again. `--every N` scores every Nth triple
  only, with the chain broken between them.
- **The scores**, on luma: PSNR over the picture, and **tear blocks**:
  16×16 blocks off by more than 24 levels on average, the local failures
  a person sees (a tear, a seam), which PSNR averages away. `--hud`
  scores a rectangle on its own (the GTA 6 minimap at 1080p is about
  `32,883,302,170`).
- **`--out DIR`** writes the worst K triples (`--worst`, 5): N1, the
  generated picture and the error map (the error ×4, tear blocks
  outlined red), as PNGs. Never next to the clip: it is footage.
- **`copy` is the floor**: it shows N0 for N1, which is what no
  interpolation costs. On `gta6-driving-01` (2026-11-23): PSNR 19.36 dB,
  1424 tear blocks a triple, 97 % of triples torn, the minimap 28.20 dB.
- Each frame goes through the window's convert pass at the picture
  controls' defaults; a pair a generator declines (a scene cut) is not
  scored, and counted. The whole 30 s clip takes about 15 s.

## Signals

- **Run a signal test in the foreground.** A job started with `&` from a
  non-interactive shell inherits SIGINT as ignored, and bash cannot trap a
  signal ignored on entry, so Ctrl-C handling looks broken when it is not.
  Send a real `^C` through `script` in the foreground.
- **Anchor process patterns**: `pgrep -af '^mpv av:'`, never a bare
  `pkill -f view.sh`, which also matches the shell running the command.

## After a run

Nothing left behind: `pgrep -af '^mpv av:|^pw-loopback|^gst-launch|^ffplay|^bash .*view.sh'`,
and `hyprctl monitors -j` shows the original refresh rate.

## Stand-ins and inspection

- **A missing tool:** a `PATH` of symlinks to every `/usr/bin` program but
  that one (`legacy/tests/preflight.sh` builds one with just the tools it
  wants).
- **A tool that misbehaves:** a directory first on `PATH` with a script of
  the same name, e.g. a `pw-loopback` that exits at once or never links, or
  a `hyprctl` whose `eval` fails (`legacy/tests/refresh-record.sh`;
  `src/desktop.rs`'s tests do the same for protoHDMIcapture).
- **Who holds a lock:** `fuser -v $XDG_RUNTIME_DIR/protohdmicapture/*.lock`.
  A scan of `/proc/*/fd` takes over a minute.
