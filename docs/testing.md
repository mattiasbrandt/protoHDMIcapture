# Verifying a change

`tools/check` (also the pre-commit hook) plus real runs against the card.
`tools/check` includes `tools/harness/`: hardware-free checks of the
functions behind past defects. Fixing one that a stand-in can reach, add a
harness there (`lib.sh` lifts functions out of view.sh and lib/setup.sh,
which run when sourced), and make it fail with the bug put back before
trusting it. Below: the traps that produce false results on real runs.

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

- **The wizard** reads a terminal, so run it in a pty and pipe the answers:

  ```bash
  printf '\n\n\n...y\n' | CONFIG_FILE=$scratch timeout 60 script -qec './view.sh --setup' out.ts >/dev/null
  ```

  Count the answers against the stages actually shown. The card stage asks
  once more for each other video device not yet ignored (a webcam), so the
  count depends on what is plugged in and on `IGNORE_CARDS_RE`. When the piped input
  runs out the pty stays open, so the next prompt **blocks** until the
  timeout rather than failing: a "hang" usually means one answer too few.
- **Keys in the mpv window** go through its IPC socket:
  `echo '{"command":["keypress","f"]}' | socat - /tmp/mpv-protohdmicapture.sock`.
- **Keys in the Rust viewer's window** have no socket: focus it, then type.
  Omarchy's Hyprland takes Lua dispatches only, and the old
  `dispatch focuswindow` form fails without a word:

  ```bash
  hyprctl dispatch 'hl.dsp.focus({ window = "class:^protohdmicapture$" })'
  wtype -k Tab          # ctrl+=: wtype -M ctrl -k equal -m ctrl
  ```

  Focus follows the mouse: a key the user types lands in the window under
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
  The repo's own entry (`desktop/`, the one installed here) runs `launch.sh`
  (view.sh); packaging's runs `protohdmicapture --menu`. Reach that path the
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
- **The baseline** (2026-10-16, `20ca430`, MS2130 1080p60, DP-1 at
  59.97 Hz, 10 min): kernel → presented median 41.8 ms, p99 50.6 ms; about
  4 % of refreshes repeat. Re-measure against it after a change to pacing,
  the event loop or the present path.
- **The MS2109 sends about 7 s of black** after every stream start, with a
  source playing: a shorter probe sees only black.

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
  that one (`tools/harness/preflight.sh` builds one with just the tools it
  wants).
- **A tool that misbehaves:** a directory first on `PATH` with a script of
  the same name, e.g. a `pw-loopback` that exits at once or never links, or
  a `hyprctl` whose `eval` fails (`tools/harness/refresh-record.sh`).
- **Who holds a lock:** `fuser -v $XDG_RUNTIME_DIR/protohdmicapture/*.lock`.
  A scan of `/proc/*/fd` takes over a minute.
