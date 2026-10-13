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
