# The script viewer: view.sh + mpv

protoHDMIcapture legacy, `view.sh`, is the original edition: bash around
mpv for the picture and `pw-loopback` for the sound, Linux only, run from a
checkout. protoHDMIcapture itself (`protohdmicapture`, the
[README](../README.md)) is what the releases ship; legacy is kept as an
alternative, best effort: it may lag behind or break
([ADR 0002](adr/0002-legacy-kept-best-effort.md)). Both read the same settings file and the same `capture-cards/` and
`sources/`, so either can play a setup the other saved.

## Running it

```bash
./view.sh                     # play; the first run offers the setup wizard
./view.sh --setup             # (re)run the wizard in the terminal
./view.sh --tune              # the feed with the overlay open, to tune the audio sync
./view.sh --debug             # play with verbose logging and the dashboard
./view.sh --settings          # every setting, its value and where it came from
./view.sh --help              # every flag
```

It needs `v4l2-ctl` (v4l-utils), `flock`, `mpv`, and for the sound `pactl`,
`pw-loopback` and `pw-link` (PipeWire); `REFRESH=match` needs `python3`,
the dashboard `socat`. `lib/preflight.sh` checks them before a run and
names the package for each one missing.

The keys are the Rust viewer's (the README), plus mpv's own **i** / **I**
for its frame-timing stats. The overlay is a Lua script in the mpv window
(`lib/mpv/protohdmicapture.lua`), bound for that window only, so your
`input.conf` is untouched.

### What differs from the Rust viewer

- **The wizard** runs in the terminal, and its audio sync stage is a
  terminal tuner. Each run writes a Markdown report (verdicts, the config
  changes, and the raw `lsusb` / `v4l2-ctl` / `pactl` / kernel log) to
  `~/.local/state/protohdmicapture/setup-<time>.md`, saved or not; the
  newest 10 are kept.
- **An audio sync change restarts the loopback**, a moment of silence:
  the mpv script asks view.sh, which owns the loopback (`audio.request` and
  a USR1, see [run-dir.md](run-dir.md)). The Rust viewer moves the delay
  live.
- **Three players** (below), and `AUDIO=mpv`.
- **`FMT=nv12` and `rawvideo`** are captured; the Rust viewer refuses them.

## The menu entry

`desktop/protohdmicapture.desktop` runs `launch.sh`, which adds what a menu
launch needs: with no settings file it opens a terminal with the wizard
(`xdg-terminal-exec`, else `$TERMINAL`, else `x-terminal-emulator`) and
plays only if it saved; it sets `REFRESH=match` unless the settings say
otherwise; it keeps the run log at `$XDG_RUNTIME_DIR/protohdmicapture.log`;
and it notifies on a failure view.sh could not report itself. `NO_SETUP=1`
skips the wizard. Install it from the checkout:

```bash
sed "s|@CHECKOUT@|$PWD|" desktop/protohdmicapture.desktop \
    > ~/.local/share/applications/protohdmicapture.desktop
mkdir -p ~/.local/share/icons/hicolor
cp -r packaging/linux/icons/hicolor/. ~/.local/share/icons/hicolor/
```

The packaged Rust viewer installs an entry under the same name, so keep one
or the other.

view.sh itself keeps the rules that hold however it is started: one video
window and one audio loopback at a time (a lock, not a window lookup; a
second launch focuses the running window on Hyprland), and with no terminal
attached a failure goes to `notify-send` and exits 3 ("already reported").

## The players

mpv is the player; the overlay and every live key exist only there, and the
wizard sets it. `BACKEND=gst` and `ffplay` stay for measuring and fault
isolation, with no overlay. A setting the chosen player ignores gets a
warning, never silence:

| setting | mpv | gst | ffplay |
|---|---|---|---|
| `FMT` | yes | yes (`yuyv422`→YUY2, `nv12`, `mjpeg`, `rawvideo`) | yes |
| `WINDOW` fullscreen | yes | yes (waylandsink `fullscreen=true`) | yes |
| `WINDOW` borderless / windowed, `WINDOW_SIZE` | yes | only through the desktop adapter (Hyprland) | yes |
| `SCALING`, `LEVELS`, `IPC` | yes | no | no |
| `GST_IO_MODE` | no | yes | no |
| `AUDIO=mpv` | yes | refused | refused |
| desktop | any | Wayland only | any |

Each player is asked for the display mode with its own flags (mpv `--fs` /
`--border=no --window-maximized` / `--geometry`, ffplay `-fs` / `-noborder`
/ `-x -y`, gst `fullscreen=true`). Where the compositor ignores the
request, the desktop adapter (`lib/desktop.sh`) finishes the job: on
Hyprland it floats, sizes and centres a windowed feed and maximizes a
borderless one; **f** in the mpv window sets mpv's properties, then runs
`view.sh --place-window`.

### mpv

```
mpv av://v4l2:/dev/videoN \
    --demuxer-lavf-o=video_size=1920x1080,framerate=60,input_format=yuyv422 \
    --profile=low-latency --untimed --no-audio \
    --vo=gpu-next --swapchain-depth=1
```

- `--profile=low-latency` expands to `audio-buffer=0`, `vd-lavc-threads=1`,
  `cache-pause=no`, `demuxer-lavf-o-add=fflags=+nobuffer`,
  `demuxer-lavf-probe-info=nostreams`, `demuxer-lavf-analyzeduration=0.1`,
  `video-sync=audio`, `interpolation=no`, `video-latency-hacks=yes` and
  `stream-buffer-size=4k` (`mpv --show-profile=low-latency`, 0.41.0).
- `--untimed` shows each frame the moment it arrives instead of scheduling
  it. mpv's manual says this breaks unless the stream has no audio, which
  is why the sound lives outside mpv: the biggest latency lever, and only
  available because of the split.
- `--swapchain-depth=1` waits for each frame to be visible before rendering
  the next, instead of keeping 2 in flight: one frame less.
- `timestamps=abs` is set **only** for `AUDIO=mpv`, which needs the wall
  clock to share a time base with the pulse input. Anywhere else it makes
  mpv read the position as seconds since the epoch, decide it is at the end
  and pause (see Troubleshooting).
- Always on: `--force-window=immediate` (a window before the first frame,
  so a card with no signal does not look like a hung launcher),
  `--input-terminal=no` (the wizard's tuner reads that terminal),
  `--stop-screensaver=always` (a feed makes no input events),
  `--term-status-msg=` (no per-frame status line in the log), and
  `MANGOHUD=0` (a session-wide `MANGOHUD=1` would inject MangoHud's implicit
  Vulkan layer into every frame).

An `mpv.conf` tuned for films survives into the session, since
`--profile=low-latency` does not undo it, so view.sh overrides what hurts
a live feed on the command line: `--keep-open=no`,
`--save-position-on-quit=no`, `--hwdec=no` (nothing to decode in YUYV),
`--target-colorspace-hint=no`, and the scaler through `SCALING` (`config`
leaves mpv.conf's alone).

`IPC` (default `/tmp/mpv-protohdmicapture.sock`) is mpv's IPC socket, for a
keybind or a script:

```bash
echo '{"command":["cycle","fullscreen"]}' | socat - /tmp/mpv-protohdmicapture.sock
```

### gst

```
v4l2src device=/dev/videoN io-mode=mmap do-timestamp=true
  ! video/x-raw,format=YUY2,width=1920,height=1080,framerate=60/1
  ! queue max-size-buffers=1 leaky=downstream
  ! videoconvert ! waylandsink sync=false async=false qos=false fullscreen=true
```

`sync=false` renders on arrival with no clock, and a one-buffer leaky queue
drops instead of building up latency. `GST_IO_MODE=dmabuf` tries zero-copy;
not every UVC card supports it, hence `mmap`. waylandsink has no title, so
a window rule matches it by class (`gst-launch-1.0`).

### ffplay

A reference: `-fflags nobuffer -flags low_delay -probesize 32
-analyzeduration 0 -vf setpts=0`. Useful to tell whether a problem is in
mpv or in the capture card.

## The audio path

```
pw-loopback -n protohdmicapture-audio -C <card-source> --latency 10 --delay 0.05
```

`--latency` is the loopback's buffer in ms (`LAT_MS`), `--delay` the audio
sync in seconds (`DELAY`; `target.delay.sec` in
`libpipewire-module-loopback(7)`). With no `-P` the sound follows the
default sink, so switching outputs mid-session just works. `AUDIO=mpv` puts
the sound inside mpv instead (`--audio-file=av://pulse:<source>`): one
process, but without `--untimed`, so slower.

`pw-loopback` silently binds the default source when the one named in `-C`
is not there yet (a capture card that enumerates late), so you would hear
your microphone. view.sh checks the real link after the start; a link to
any other source stops the loopback, says which source it got, and plays
on without sound.

## Troubleshooting

**Picture stuck, sound fine.** The sound never passes through mpv, so the
fault is in mpv: most likely it paused itself (the `timestamps=abs` trap
above). Check which side is stuck:

```bash
# Is the capture card delivering? About 300 frames in 5 s is healthy.
ffmpeg -f v4l2 -input_format yuyv422 -video_size 1920x1080 -framerate 60 \
       -i /dev/videoN -t 5 -f null - 2>&1 | grep -oE 'frame= *[0-9]+' | tail -1

# Is mpv playing? Healthy: false, 60.0, 0.
for p in pause estimated-vf-fps frame-drop-count; do
    echo "{\"command\":[\"get_property\",\"$p\"]}" | socat - /tmp/mpv-protohdmicapture.sock
done
```

**`ioctl(VIDIOC_QBUF): Inappropriate ioctl for device` on exit** is
libavdevice's untidy V4L2 teardown, not a device fault; view.sh drops that
one line from mpv's output.
