# Verifying a change

No unit tests: `tools/check` (also the pre-commit hook) plus real runs
against the card. These are the harness traps that produce false results.

## Before a run

- **Is the source awake?** A sleeping console delivers no frames, which looks
  like a code bug (gst/ffplay open no window; the signal check fails):

  ```bash
  timeout 6 v4l2-ctl -d /dev/video0 --stream-mmap --stream-count=60 --stream-to=/dev/null; echo $?   # 124 = no frames
  ```

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

  Count the answers against the stages actually shown. When the piped input
  runs out the pty stays open, so the next prompt **blocks** until the
  timeout rather than failing: a "hang" usually means one answer too few.
- **Keys in the mpv window** go through its IPC socket:
  `echo '{"command":["keypress","f"]}' | socat - /tmp/mpv-protohdmicapture.sock`.
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
