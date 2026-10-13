# protoHDMIcapture

Low-latency viewer for any HDMI source (console, camera, another PC) through a
USB capture card on Linux. It runs on any desktop; on Hyprland (Omarchy) it also
matches the display refresh to the feed and focuses the running window.

It started life as `ps5-capture`, built to play a PS5 in a window, so the
measurements and source-side settings below were taken with a PS5 on the HDMI
side. Nothing in the scripts is PS5-specific.

Research + test kit. Written 2026-09-06, before the card was connected.

---

## Quick start

```bash
./view.sh                     # play; the first run offers the setup wizard
./view.sh --setup             # (re)run the wizard: card, mode, audio, display, sync
./view.sh --debug             # play with verbose logging and a live dashboard
./view.sh --settings          # every setting, its value and where it came from; checks them
./view.sh --tune              # re-tune audio sync: the overlay opens, ctrl -/+ shifts it, w keeps it
```

In the video window: **Tab** opens the overlay, **f** cycles fullscreen →
borderless → windowed, **d** opens the live dashboard, **1**–**8** adjust
colour and **s** takes a screenshot (mpv's own keys), **i** / **I** show
mpv's frame-timing stats, **q** quits.

### The overlay

For the first 10 seconds a bar along the bottom lists the main keys (Tab,
f, the audio keys, s, q); it goes as soon as the overlay opens.

**Tab** opens a panel over the picture with the keys and the live settings,
each with its current value:

| key | while the overlay is open |
|---|---|
| **l** | levels: auto → limited → full (the range of the card's frames) |
| **c** | scaling: balanced → fast → config |
| **1**–**8** | colour: contrast, brightness, gamma, saturation (mpv's keys, also with the overlay closed) |
| **ctrl −** / **ctrl +**, **alt −** / **alt +** | audio sync 10 / 50 ms earlier / later (also with the overlay closed); **w** saves it as `DELAY` |
| **m**, **9** / **0** | mute, volume down / up of the audio loopback (also with the overlay closed). Not in the config: WirePlumber remembers the stream's volume between sessions, as it does for any app |
| **r** | reset colour |
| **t** | the setup tips for the source (`SOURCE`); **←** / **→** browse the other sources |
| **w** | save what changed since the feed started to the config file |
| **Esc**, **Tab** | close |

Changes are live: they last until the feed closes, unless **w** saves them,
through the same writer the wizard uses. The overlay is mpv's; `BACKEND=gst`
and `ffplay` have none.

Settings are environment variables, or `KEY=value` lines in
`~/.config/protohdmicapture/config` (the wizard writes it; see
`config.example`); the environment wins. Machine-specific values (which card,
which cameras to skip, your tuned `DELAY`) belong in that file, not in the
scripts.

One video window and one audio loopback run at a time, however they were
started: a second launch focuses the running window (Hyprland) or says it is
already running, and a second loopback is refused rather than doubling the
audio. The tuning pair above is one of each, so it is allowed.

### The setup wizard

`./view.sh --setup` walks through, in a centered terminal UI:

1. **Capture card**: every capture device with its serial; the one with a
   known capture-chip USB id is recommended. Pinned by USB serial
   (`CARD_SERIAL`), or by USB port (`CARD_PORT`) when the card reports none.
2. **USB link**: the chip the id implies, with what is known about that
   card and its support status (its profile in `capture-cards/`; see
   [docs/capture-cards.md](docs/capture-cards.md)), and the negotiated
   speed. 480 Mbit/s (USB 2) is flagged, since uncompressed 1080p60 needs
   USB 3, unless the chip is a USB 2 one (MS2109) anyway.
3. **Source**: what is plugged into the card (a profile from `sources/`, or
   any HDMI source), and that source's setup tips to apply now (`SOURCE`).
4. **Picture format**: the card's own mode list, best first (frame rate, then
   size, then uncompressed), starting on the mode the source suggests or the
   closest one the card has, with a note when the card cannot do it.
5. **Signal check**: 2 seconds of capture at that mode, measuring the real
   frame rate. UVC cards cannot report signal lock, so frames are the test,
   and a few frames are checked for a uniformly black picture: some cards
   (the MS2109) stream black at full rate with nothing plugged in.
6. **Audio**: the card's own input (found on the card's USB device at every
   start), another source, or none.
7. **Display mode** and window size, **refresh matching** (where the desktop
   adapter can), and scaling. The player is mpv; a config set to gst or
   ffplay by hand is offered the switch.
8. **Audio sync**: optional; a windowed feed without audio opens and the
   j/k/h/l tuner runs in the wizard's terminal.
9. **Review**: a diff of exactly what will change; nothing is written before
   this. Then an optional test feed that behaves exactly as the saved config.

Re-running it edits: every question starts on the current value. Each run
also writes a Markdown report (summary with verdicts, config changes, and the
raw `lsusb` / `v4l2-ctl` / `pactl` / kernel-log output) to
`~/.local/state/protohdmicapture/setup-<time>.md`, saved or not; the newest 10
are kept. Paste one to an AI agent or a forum post when something is off.

On first run `./view.sh` offers the wizard (declining asks never again), and
the menu launcher opens it in a terminal, starting the feed only once
settings are saved. `NO_SETUP=1` skips both.

### Display modes

`WINDOW` works the way a game's display setting does:

| `WINDOW` | what you get | latency |
|---|---|---|
| `fullscreen` (default) | covers the monitor it opens on | lowest: direct scanout, tearing possible |
| `borderless` | maximized, no border; the desktop's bar stays visible | composited |
| `windowed` | a normal window of `WINDOW_SIZE` (default `1280x720`), centred | composited |

Each player is asked for the mode with its own flags (mpv `--fs` /
`--border=no --window-maximized` / `--geometry`, ffplay `-fs` / `-noborder` /
`-x -y`, gst `fullscreen=true`). Where the compositor ignores a client's
request, the desktop adapter finishes the job: on Hyprland it floats, sizes
and centres a windowed feed, and maximizes a borderless one with its border
removed; this also gives gst, which cannot ask for either, both modes.
`FULLSCREEN=yes|no` still works as a deprecated alias.

**f** in the mpv window cycles the three live. mpv's own properties are set
first, then `view.sh --place-window` runs the same adapter, which undoes
whatever the previous mode set.

### Debug mode

`DEBUG=log` (or a saved preference from the wizard) records, on screen and in
`$XDG_RUNTIME_DIR/protohdmicapture/debug.log`:

- every decision: each knob with where it came from (env, file, default),
  locks, the audio link, refresh and window placement, the player's PID and
  exit status
- the format the device actually negotiated, read back 2 s after start
- verbose player output, filtered for signal: mpv at `v` (libplacebo shader
  chatter at `info`), gst warnings, QoS and the negotiated caps, ffplay's
  stats line once a second
- a perf line every 5 s: player and loopback CPU, player memory, and for mpv
  the captured and display fps and the drop and delay counters

`DEBUG=window`, or `./view.sh --debug` for one run, adds a live dashboard in
its own terminal, refreshed every second: those numbers, PipeWire xruns for
the loopback, every monitor's mode and the latest warnings. It holds no lock
and only reads the session's run directory, so closing it never touches the
feed; it closes itself when the feed ends. **d** in the mpv window opens it
on demand.

---

## TL;DR

Don't replace OBS with a different all-in-one. **Split the problem in two**, which
is exactly what OBS *cannot* do and why you had to fight it:

| Stream | Path | Latency |
|---|---|---|
| Video | `mpv --untimed` (or GStreamer `sync=false`) straight from `/dev/videoN`, **no audio track** | shown the instant the frame arrives |
| Audio | PipeWire loopback: card's capture source → your sink, **never touches the video player** | ~5–20 ms, tunable |
| Sync | one fixed `--delay` on the loopback, tuned once | — |

A single pipeline (OBS, or mpv with an audio track) has to reconcile two clocks
that drift against each other, and it pays for that sync with buffer. That
buffer is the delay you were tweaking in OBS. Splitting the streams removes the
reconciliation entirely — nothing has to stay in sync, so nothing needs a
buffer. You line them up once with a constant.

---

## Confirmed configuration (measured on this card, 2026-09-06)

The card is connected and capturing. These are no longer assumptions:

```bash
DEV=/dev/video2 \
SRC=alsa_input.usb-1e4e_7016_20000130041415-02.analog-stereo \
RES=1920x1080 FPS=60 FMT=yuyv422 \
./view.sh
```

| | |
|---|---|
| USB id / serial | `1e4e:7016`, `20000130041415` |
| Link | `4-5.2`, **5000 Mbit/s SuperSpeed** |
| Formats offered | `MJPG` and `YUYV`, **1920x1080 @ 60 fps only** — no other size or rate |
| Chosen | **`yuyv422`** — uncompressed, no decode step |
| Measured rate | ~57–58 fps over 300 frames incl. startup, i.e. a real 60 |
| Audio source | `alsa_input.usb-1e4e_7016_20000130041415-02.analog-stereo`, **s16le 2ch 48000 Hz** |
| Resets during capture | none; `devnum` stable |

Two things worth knowing:

- **MJPEG is broken on this unit.** Capturing `-input_format mjpeg` produces
  `No JPEG data found in image` / `Error submitting packet to decoder`. YUYV is
  not merely preferable here, it is the only clean path.
- **The audio is plain 48 kHz stereo**, so the notorious MS2109 96 kHz-mono
  firmware bug does not apply to this unit. Nothing to work around.
- **No DV-timings support** (`VIDIOC_G_DV_TIMINGS: Inappropriate ioctl`), so
  there is no way to query "is the source locked in?" from the driver. Judge signal
  presence from the picture itself.

---

## What's already on this machine

Checked, not assumed:

- `mpv` 0.41.0 — `gpu-next` and `dmabuf-wayland` VOs present
- `ffplay` present; ffmpeg has the `v4l2`, `pulse`, `alsa` and `jack` demuxers built in
- GStreamer with `v4l2src`, `waylandsink`, `glimagesink`, `pipewiresrc/sink`, `jpegdec` — all OK
- PipeWire 1.6.8, `pw-loopback` supports `--latency` (ms) and `--delay` (float seconds)
- `v4l-utils` present
- Hyprland 0.56.2

Two gotchas found in the current state:

1. **Node numbers move.** The OBSBOT takes `video0`/`video1` and the card
   currently lands on `video2`/`video3` — but that depends purely on
   enumeration order, so it flips whenever either device is replugged. The
   scripts pin the card by its USB serial (`CARD_SERIAL`), or its USB port
   (`CARD_PORT`) when it has no serial, never by a fixed node. Without a pin
   they scan, skipping `IGNORE_CARDS_RE`.
2. **You are not in the `video` group** (`mattias gamemode i2c uucp wheel`).
   Access works anyway because logind puts an ACL on `/dev/video*` for the
   active seat (`crw-rw----+`). Fine for an interactive session; it will *not*
   work from a systemd system service or a detached shell.

---

## Preconditions on the source side

The settings on the source itself that produce a black screen or bad audio
when wrong (HDCP, audio format, HDR, resolution and refresh) live in one
profile per source in [`sources/`](sources/): [PlayStation 5](sources/ps5.md)
(tested), and from the vendors' own pages [PlayStation 4](sources/ps4.md),
[Xbox Series X/S](sources/xbox-series.md), [Nintendo Switch](sources/switch.md),
[Nintendo Switch 2](sources/switch-2.md) and [a Windows PC](sources/pc.md),
plus [any HDMI source](sources/generic.md). A path only a third-party guide
gives says so on its line. The setup wizard asks what
is plugged in, shows that source's tips before the signal check, and starts
the mode list on the mode the source suggests, or the closest one the card
can capture. **HDCP off first**: everything else is moot until it is.

Each profile is a small Markdown file: a `Suggest:` line of settings
(`RES`, `FPS`) the wizard starts from, a `Status:` line, and one bullet per
tip with its menu path.

---

## Preconditions on this machine

### Display cadence — the one that will annoy you

`DP-1` (ROG PG348Q, 3440×1440) is running at **100 Hz**. A 60 fps source on a
100 Hz display lands 1.66 refreshes apart, so frames get held for 1 or 2
refreshes alternately — visible judder no player can fix.

VRR would fix it, but **the PG348Q can't do VRR on Linux**: it's a native
G-Sync-module panel, and the kernel exposes no `vrr_capable` attribute for
`DP-1` at all (checked in `/sys/class/drm/card1-DP-1/`). Hyprland reports
`"vrr": false` for every output.

It does have a **3440x1440@59.97Hz** mode, and `view.sh` will switch to it
for the session (Hyprland only; on other desktops it leaves the display alone
and says so):

```bash
REFRESH=match ./view.sh      # 100Hz -> 59.97Hz, restored on exit
```

`REFRESH` defaults to `keep`, which leaves the display completely alone.

**No output is hardcoded.** `lib/pick-refresh-mode.py` reads `hyprctl monitors -j`,
takes whichever monitor is *focused*, and picks the mode at that monitor's
current resolution whose refresh is closest to `FPS`. Resolution is never
changed — only refresh — because changing resolution would rearrange a
multi-monitor layout. Position, scale and transform are all carried across, so a
rotated display stays rotated.

> **`hyprctl keyword` does not work on Omarchy.** It fails with `keyword can't
> work with non-legacy parsers. Use eval.` because Omarchy configures Hyprland
> in Lua — and it fails *silently*, reporting nothing while changing nothing.
> The correct form is:
>
> ```bash
> hyprctl eval 'hl.monitor({ output = "DP-1", mode = "3440x1440@59.97", position = "0x0", scale = 1 })'
> ```
>
> Verified end to end: switch, confirm at 59.97, exit, confirm back at 100, with
> DP-2 and HDMI-A-1 (including its `transform = 2`) untouched throughout.

That alone is probably a bigger perceived-smoothness win than any player flag.

### Tearing (optional, small win)

Hyprland 0.56.2 has `general:allow_tearing` (currently `false`) plus a
per-window `immediate` rule; tearing only engages when the window is fullscreen
(`WINDOW=fullscreen`, not borderless) and alone on the output. Worth about one frame. Note the option is
`render:direct_scanout` in this version, not the older `misc:no_direct_scanout`.

```ini
general { allow_tearing = true }
windowrule = immediate, title:^\(protoHDMIcapture\)$
```

I'd try the 59.97 Hz mode first and only add tearing if you still want more.

### Window rules (applied)

Blur, shadows and animations on a 60 fps live feed cost GPU frame time for
nothing. These are now in `~/.config/hypr/hyprland.lua`:

```lua
local function protohdmi_feed()
  return {
    tag = "-default-opacity",
    opacity = "1 1",
    no_blur = true,
    no_shadow = true,
    no_anim = true,
  }
end
o.window({ title = "^protoHDMIcapture$" }, protohdmi_feed())
o.window({ class = "^gst-launch-1\\.0$" }, protohdmi_feed())
```

mpv and ffplay set the window title; `BACKEND=gst` cannot (waylandsink has no
title property), so its window is matched by class. Each rule gets a **fresh
table**: Omarchy's `o.window()` writes the match into the table it is given, so
sharing one table makes the second rule require title *and* class, matching
nothing. Check with `hyprctl clients -j`: a covered window has no
`default-opacity` tag.

Omarchy configures Hyprland in **Lua**, not `hyprland.conf`, so the old
`windowrule = noanim, ...` syntax does not apply here. Opacity is dropped the way
Omarchy's own `steam.lua` and `pip.lua` do it — shed the `default-opacity` tag
and pin opacity to `1 1` — rather than with a raw `opaque` rule.

The key names were confirmed by falsification rather than assumption: adding a
deliberately bogus key produces `hl.window_rule: unknown field '...'` in
`hyprctl configerrors`, so a clean reload proves these five are really accepted.

### Audio quantum

PipeWire is at `clock.quantum = 1024` @ 48 kHz = **21.3 ms** per period. The
loopback asks for its own buffer via `--latency`, so you don't need to force the
global quantum down — and you shouldn't, since it affects everything else on the
system. Only if the loopback crackles at `LAT_MS=10` should you consider raising
`LAT_MS`, and only if *that* fails should you touch the global setting.

---

## The three video backends, and why

mpv is the player: the overlay and every live key exist only there, and the
wizard sets it. gst and ffplay stay wired up for measuring and fault
isolation (which one wins on latency is card-dependent and worth measuring
rather than arguing about); set them by hand, and `view.sh` says they have
no overlay. They share one set of knobs but do not all honour every one; a knob you set that the backend ignores gets a
warning, never silence:

| knob | mpv | gst | ffplay |
|---|---|---|---|
| `FMT` | yes | yes (`yuyv422`→YUY2, `nv12`, `mjpeg`, `rawvideo`) | yes |
| `WINDOW` fullscreen | yes | yes (waylandsink `fullscreen=true`) | yes |
| `WINDOW` borderless / windowed, `WINDOW_SIZE` | yes | only through the desktop adapter (Hyprland) | yes |
| `SCALING`, `LEVELS`, `IPC` | yes | no | no |
| `GST_IO_MODE` | no | yes | no |
| `AUDIO=mpv` | yes | refused | refused |
| desktop | any | Wayland only | any |

### `BACKEND=mpv` (default)

```
mpv av://v4l2:/dev/video2 \
    --demuxer-lavf-o=video_size=1920x1080,framerate=60,input_format=yuyv422 \
    --profile=low-latency --untimed --no-audio \
    --vo=gpu-next --swapchain-depth=1
```

`--profile=low-latency` is not a vague hint; it expands to a concrete set
(verified with `mpv --show-profile=low-latency` on 0.41.0):

```
audio-buffer=0            vd-lavc-threads=1
cache-pause=no            demuxer-lavf-o-add=fflags=+nobuffer
demuxer-lavf-probe-info=nostreams
demuxer-lavf-analyzeduration=0.1
video-sync=audio          interpolation=no
video-latency-hacks=yes   stream-buffer-size=4k
```

`--untimed` displays each frame the moment it arrives instead of scheduling it.
The mpv docs are explicit that this **breaks unless the stream has no audio** —
which is precisely why the audio lives outside this process. This is the single
biggest latency lever and it is only available *because* of the split
architecture.

`--swapchain-depth=1` makes the VO wait for each frame to become visible before
rendering the next, instead of keeping 2 in flight. Default is 2; that second
in-flight frame is one frame of latency.

`timestamps=abs` puts the V4L2 input on the wall clock rather than kernel
timestamps. It is needed **only** for the `AUDIO=mpv` fallback, and it is *not*
harmless elsewhere: it makes mpv read the position as seconds-since-epoch, treat
the timeline as `100%`, and pause itself. See Troubleshooting.

### Your mpv.conf fights this workload

`~/.config/mpv/mpv.conf` is tuned for films. Four of its settings survive into a
capture session and work against it, because `--profile=low-latency` does **not**
undo them. `view.sh` now overrides each explicitly on the command line:

| mpv.conf | why it hurts here | override |
|---|---|---|
| `profile=high-quality` (line 5) | ewa_lanczossharp + debanding + antiringing on every frame — real GPU time, and `low-latency` leaves it fully intact | `SCALING=balanced` (default) |
| `video-sync=display-resample` (14) | resamples to display refresh — precisely the timing machinery we're removing | `--profile=low-latency` already forces `video-sync=audio` |
| `keep-open=yes` (18) | on signal loss mpv sits paused on a stale frame instead of exiting | `--keep-open=no` |
| `save-position-on-quit` (20) | writes a resume position for a live device — meaningless, litters `watch_later` | `--save-position-on-quit=no` |
| `hwdec=auto-safe` (8) | nothing to decode in raw YUYV | `--hwdec=no` |
| `target-colorspace-hint=auto` (11) | HDR negotiation on an SDR feed, can shift colours | `--target-colorspace-hint=no` |

Note the config's comments mention Plasma 6, but you're on Hyprland — worth a
tidy-up sometime, no functional effect.

### The settings worth knowing

Each setting has one row in `lib/settings.sh`: its default, the values it
allows and the players that honour it. A value outside them stops the run
with a message naming the setting and where it came from (environment, the
config file, or the default), whatever the player; `./view.sh --settings`
shows the lot without starting anything.

| var | default | what it does |
|---|---|---|
| `CARD_SERIAL` | — | USB serial of the card; pins it among other cameras and across `/dev/video` renumbering. The setup wizard writes it |
| `CARD_PORT` | — | USB port path (udev `ID_PATH`) of a card that reports no serial; found again after a replug into the same port. The setup wizard writes it. Any of `CARD_SERIAL`, `CARD_PORT`, `DEV` set in the environment overrides all three in the config file, so `CARD_PORT=... ./view.sh` plays a second card for one run |
| `IGNORE_CARDS_RE` | — | without a pin: v4l2 card types to skip while scanning, e.g. `OBSBOT\|Integrated Camera` |
| `WINDOW` | `fullscreen` | `fullscreen` \| `borderless` \| `windowed`; see "Display modes". Opens on whatever monitor the desktop picks, none is hardcoded. **f** cycles it live |
| `WINDOW_SIZE` | `1280x720` | window size for `WINDOW=windowed` |
| `DEBUG` | `off` | `log` \| `window`; see "Debug mode". `--debug` = `window` for one run |
| `FULLSCREEN` | — | deprecated: `yes` = `WINDOW=fullscreen`, `no` = `WINDOW=windowed` |
| `SCALING` | `balanced` | `balanced` = spline36/mitchell, no deband, no dither. `fast` = the properties of mpv's `fast` profile, bilinear throughout, lowest GPU time and visibly softer. `config` = leave mpv.conf's high-quality settings alone. Applied by the mpv script, so **c** in the overlay cycles all three live |
| `BRIGHTNESS`, `CONTRAST`, `SATURATION`, `GAMMA` | `0` | mpv's colour adjustments, -100..100; keys **1**–**8** change them live and the overlay's **w** saves them |
| `SOURCE` | — | what is plugged in: a profile name from `sources/` (`ps5`); its setup tips show in the overlay. The wizard writes it |
| `LEVELS` | `auto` | the colour range of the card's frames: `limited` or `full` if blacks look crushed or washed (mpv `--vf=format:colorlevels=`; the display's own range is untouched). `auto` trusts the frames: YUYV is read as limited, while MJPEG frames (MS2109) arrive flagged full. The MS2130 reports `Quantization: Default (maps to Limited Range)` and the PS5 outputs limited, so `auto` is right there |
| `IPC` | `/tmp/mpv-protohdmicapture.sock` | mpv IPC socket, so you can drive it from a keybind or script |
| `REFRESH` | `keep` | `match` switches the focused monitor to its mode nearest `FPS` for the session and restores it on exit (Hyprland only). No output hardcoded; resolution, position, scale and transform all preserved |
| `DEV` | — | the card's video node (`/dev/videoN`) instead of finding it; a pin like `CARD_SERIAL`, but node numbers move on replug, so prefer the serial or the port |
| `SRC` | — | the PipeWire source to take the audio from; default: the card's own audio input, found on its USB device |
| `SINK` | — | the PipeWire sink to play it on; default: the desktop's default sink, followed when it changes |
| `RES` | `1920x1080` | capture resolution; the wizard picks it from the card's own mode list |
| `FPS` | `60` | capture frame rate; also what `REFRESH=match` matches |
| `FMT` | `yuyv422` | capture format: `yuyv422` (uncompressed), `mjpeg`, `nv12`, `rawvideo`. Empty leaves the device as it was last set, which is luck, not a setting |
| `DELAY` | `0.05` | audio sync in seconds, 0 to 1; **ctrl −/+** tunes it live and **w** saves it |
| `LAT_MS` | `10` | the audio loopback's buffer in ms; raise it if the sound crackles |
| `BACKEND` | `mpv` | the player: `mpv`; `gst` and `ffplay` are for comparisons and have no overlay |
| `AUDIO` | `loopback` | `loopback` (PipeWire, beside the player), `mpv` (inside mpv: one process, more latency), `none` |
| `GST_IO_MODE` | `mmap` | gst only: `mmap`, or `dmabuf` (zero-copy where the card supports it) |

A few more that are simply on, with no knob, because there's no case for the
other setting here:

- `--force-window=immediate` — window appears instantly rather than after the
  first frame lands, so a no-signal card doesn't look like a hung launcher
- `--input-terminal=no` — mpv doesn't steal stdin from the terminal you launched
  it from, which matters because the wizard's sync tuner reads that terminal
- `--stop-screensaver=always` — a capture feed produces no input events, so idle
  blanking would otherwise fire mid-game
- `--term-status-msg=` — silences the per-frame `V: hh:mm:ss` status line,
  which means nothing for a live feed and used to fill the run log
- `--script=lib/mpv/protohdmicapture.lua` — the overlay and the **Tab**,
  **f** and **d** keys, bound for this window only, so your `input.conf` is
  untouched

Driving it over IPC, e.g.:

```bash
echo '{"command":["get_property","width"]}' | socat - /tmp/mpv-protohdmicapture.sock
echo '{"command":["cycle","fullscreen"]}'   | socat - /tmp/mpv-protohdmicapture.sock
```

### `BACKEND=gst`

```
v4l2src device=/dev/videoN io-mode=mmap do-timestamp=true
  ! video/x-raw,format=YUY2,width=1920,height=1080,framerate=60/1
  ! queue max-size-buffers=1 leaky=downstream
  ! videoconvert ! waylandsink sync=false async=false qos=false fullscreen=true
```

Theoretically the lowest-latency path: `sync=false` means the sink renders on
arrival with no clock at all, and `leaky=downstream` on a 1-buffer queue makes
latency structurally unable to creep — it drops instead of accumulating. Try
`GST_IO_MODE=dmabuf` for zero-copy; not every UVC card supports it, hence the
`mmap` default.

### `BACKEND=ffplay`

Reference/sanity check. `-fflags nobuffer -flags low_delay -probesize 32
-analyzeduration 0 -vf setpts=0`. Useful mainly to confirm a problem is in mpv
rather than in the card.

---

## The audio path

```
pw-loopback -n protohdmicapture-audio -C <card-source> --latency 10 --delay 0.05
```

Both flags confirmed against `pw-loopback --help` on 1.6.8 and the
`libpipewire-module-loopback(7)` man page (`target.delay.sec`, since 0.3.60):

- `--latency` — loopback buffer in ms
- `--delay` — deliberate offset in seconds, the sync knob

Omitting `-P` makes playback follow your **default sink**, so switching between
the Arctis Nova Pro and HDMI output mid-game just works.

**Which direction to tune:** audio through this path is ~10–20 ms; video through
the card + USB + decode + display is ~50–100 ms. So audio arrives *early*, and
`--delay` should go *up* from 0 until it matches. Starting guess `DELAY=0.05`.

Tune it in the feed: **ctrl −** / **ctrl +** move the audio 10 ms earlier /
later, **alt −** / **alt +** by 50 ms, and **w** in the overlay (**Tab**)
saves it as `DELAY`. `./view.sh --tune` is the feed with the overlay already
open. Each change restarts the loopback (a moment of silence): `view.sh`,
which owns it, does that on a request from the mpv script (`audio.request`
and a USR1, see [`docs/run-dir.md`](docs/run-dir.md)); volume and mute carry
over. The setup wizard's sync stage still uses its own terminal tuner.

PS5 menu navigation clicks are the best test signal — sharp transient, tied to a
visible highlight moving.

### Fallback: `AUDIO=mpv`

Puts audio back inside mpv (`--audio-file=av://pulse:<source>`, ffmpeg here has
the `pulse` demuxer so this will work). Drops `--untimed`, so it's laggier — but
it's one process and mpv handles sync itself. Worth having if the loopback
misbehaves.

---

## Test protocol, in order

1. Set the source-side options (see [`sources/`](sources/)). **HDCP off first** — everything else is moot.
2. Plug the card into a **rear USB 3 port**, powered via its DC5V jack, and not
   into the `3-5` hub. Source on and outputting 1080p.
3. `./view.sh --setup` — this is the step that decides the real settings. It names the
   chip and prints the link speed; **check it says 5000, not 480**. Its format
   list decides `FMT`/`RES`/`FPS`: the right combination is card-specific, and
   ffmpeg *silently* falls back to some other mode if you ask for a
   combination the card doesn't have.
4. Video-only smoke test:
   `mpv av://v4l2:/dev/videoN --profile=low-latency --untimed --no-audio`
5. `AUDIO=none FMT=yuyv422 RES=1920x1080 FPS=60 ./view.sh` — then confirm
   the mode actually stuck, in another terminal:
   `v4l2-ctl -d /dev/videoN --get-fmt-video && v4l2-ctl -d /dev/videoN --get-parm`
   If YUYV won't do 60 fps at 1080p, retry with `FMT=mjpeg`.
6. Bring audio up, tune `DELAY`.
7. Compare `BACKEND=gst` against `BACKEND=mpv` on feel.
8. Switch DP-1 to 59.97 Hz and re-judge.
9. Only then consider tearing.

### Measuring latency rather than guessing

If the card has HDMI passthrough, run it to a second display and film both
screens with a phone in slow-mo (240 fps): count frames between the same event
on each. That's a real glass-to-glass number. Without passthrough, film your
thumb on the controller and the screen reaction, and compare the count against
the PS5 plugged directly into a monitor.

---

## What software cannot fix

The dominant term is the **card's own internal latency** — typically 30–80 ms
for USB cards, and entirely out of reach from this side. If after all of the
above it still feels laggy for anything twitchy, the honest answer is to play
through the card's HDMI passthrough on a real display and use the PC only for
the video feed. No player flag beats a wire.

---

## Problems found on this machine while testing

Three things turned up that were not part of the question but bear on it.

### 1. The reset loop WAS the capture card — and a USB 3 port fixed it

`1e4e:7016` was not a mystery webcam. It is **this capture card** (serial
`20000130041415`; the `1e4e` "Cubeternet" vendor id is borrowed, the silicon is
MacroSilicon, and it reports no manufacturer or product strings at all).

Plugged into the bus-powered **USB 2.1 hub** at `3-5.2` it re-enumerated ~950
times a day and died mid-stream with `VIDIOC_QBUF: Inappropriate ioctl for
device`. Moved to a **SuperSpeed port** at `4-5.2`, measured over the same
window:

| path | resets in 1 hour |
|---|---|
| `3-5.2` — USB 2 hub | **276** |
| `4-5.2` — USB 3 direct | **2** |

`devnum` now stays put across repeated captures. The reset loop was power and/or
the hub, not a faulty card.

**Keep it on a SuperSpeed port.** It is not merely a stability matter — see the
format table below; USB 2 also costs you the uncompressed path.

### 2. The OBSBOT Meet 2 is wedged — nothing holds it, the kernel just says EBUSY

`/dev/video0` and `/dev/video1` return `Device or resource busy` to every client.
Chased to a conclusion:

- `sudo fuser -v /dev/video0` → **no process holds it**
- the refusal is **kernel-level**, not an ffmpeg quirk: plain `v4l2-ctl` gets
  `VIDIOC_G_INPUT: failed: Device or resource busy`, and `--stream-mmap` gets
  `VIDIOC_STREAMON returned -1 (Device or resource busy)` with no ffmpeg in the
  picture at all
- both PipeWire camera nodes are `state=suspended`, no `Stream/Input/Video`
  client exists, and `/proc/asound/card4/stream0` shows `Status: Stop`
- all four USB interfaces are at `bAlternateSetting 0` (idle) with their normal
  drivers bound (`uvcvideo` ×2, `snd-usb-audio` ×2)

Everything is idle and unclaimed, and the device still refuses. That is wedged
firmware. **Physically unplug and replug it** — that is the reliable fix.

Do *not* use the sysfs deauthorize/reauthorize trick on this device:

```bash
# DON'T — tested here, and it strands the device:
sudo sh -c "echo 0 > .../authorized; sleep 1; echo 1 > .../authorized"
```

The `echo 0` succeeds, but the `echo 1` fails with `write error: Invalid
argument` and leaves `authorized` stuck at `0`, i.e. the device is now gone from
userspace entirely. Re-authorizing makes the kernel re-enumerate, and
enumeration is exactly what wedged firmware fails at — so the recipe breaks
hardest on the case it exists for. Recovery is a physical replug anyway.

`usbreset` (already installed, `/usr/bin/usbreset`) is the safer in-place option
when the device is still authorized, since it issues a port reset without
deauthorizing:

```bash
sudo usbreset 3564:0ea1
```

**Why this mattered to the PS5 work:** the obvious worry was that ffmpeg's v4l2
demuxer chokes on `VIDIOC_G_INPUT` for UVC devices generally — which would make
the whole `mpv` backend a dead end. Testing killed that theory: `v4l2-ctl` fails
identically without ffmpeg, so the fault is this device, and it implies nothing
about the capture card. The mpv backend stands.

(Aside: `sudo -A` **hangs** rather than prompting in this agent context — the
1Password askpass helper blocks without a TTY. Privileged checks here need a
plain interactive `sudo`.)

### 3. `pw-loopback` silently binds the wrong source (guarded now)

Asked for the OBSBOT microphone via `-C`, it connected to the `1e4e` device's
capture instead — no warning, no error, exit code 0. Verified with `pw-link -li`:

```
input.protohdmicapture-audio:input_FL
  |<- alsa_input.usb-1e4e_7016_20000130041415-02.analog-stereo:capture_FL
```

If the node named in `-C` is not available at that moment, it falls back to the
**default source**. With a capture card that enumerates a second or two late,
that means you would be listening to your microphone and wondering why the
source is silent. `view.sh` now checks the actual link after starting and prints a
yellow warning naming the source it really got.

---

## Verification — what was actually run

Everything below was executed on this machine, not predicted.

| Check | Result |
|---|---|
| `bash -n` + `shellcheck -S warning` on both scripts | clean |
| the former `probe` script / `view.sh` with no card present | refuse cleanly with an actionable message |
| Full mpv option set (`--profile=low-latency --untimed --vo=gpu-next --swapchain-depth=1` …) rendering 120 frames | exit 0 |
| GStreamer sink flags (`sync=false async=false qos=false leaky=downstream`) rendering 120 buffers | ran in 0.28 s — i.e. `sync=false` really does render on arrival rather than to a clock |
| `pw-loopback` with `-C`/`-P`/`--latency 10`/`--delay 0.075` | starts, creates `input.protohdmicapture-audio` + `output.protohdmicapture-audio`, establishes real links to the target sink |
| Cleanup trap after the player exits | no leaked `pw-loopback`, verified repeatedly |
| Source-mismatch warning | fires with the correct node name when wrong; silent when right |
| `mpv --show-profile=low-latency` | expansion quoted in this document is from 0.41.0 on this box, not from memory |

**Not verified, and cannot be until the card is here:** the actual v4l2 capture
path end-to-end, the real latency numbers, and the correct `FMT`/`RES`/`FPS`.
The OBSBOT is busy and the `1e4e` device resets out from under any client, so
neither could stand in for a full run. The v4l2 attempt against `/dev/video2`
*did* get as far as negotiating MJPEG 1920x1080 and queuing buffers before the
device reset, which at least confirms the demuxer option string is well-formed.

---

## Unrelated small find

`~/.config/mpv/mpv.conf:31` makes mpv print a warning on **every** launch:

```
mpv.conf:31: fixed-length quoting expected - put "quotes" around the option
value if you did not intend to use this, but your option value starts with '%'
```

Line 31 is `screenshot-template=%F-%wH.%wM.%wS`. mpv 0.41 reads a leading `%` as
its fixed-length quoting syntax. Fix is to quote the value:

```ini
screenshot-template="%F-%wH.%wM.%wS"
```

I have not touched your mpv.conf — say the word.

---

## Troubleshooting

### Frozen picture, but audio works

The single most likely cause is **mpv paused itself**, and the split
architecture makes that diagnosable in one glance: audio never passes through
mpv, so working audio + frozen video localises the fault to mpv alone, with no
bisecting.

The specific trap: passing `timestamps=abs` to the v4l2 demuxer puts frames on
the wall clock, so mpv computes a position of seconds-since-epoch, reads the
timeline as `100%`, decides it is at end-of-file and pauses. `view.sh` now
only sets it for `AUDIO=mpv`, where it is genuinely needed to share a time base
with the pulse input.

Confirm which side is stuck rather than guessing:

```bash
# Is the CARD delivering? ~300 frames in 5 s = healthy.
ffmpeg -f v4l2 -input_format yuyv422 -video_size 1920x1080 -framerate 60 \
       -i /dev/video2 -t 5 -f null - 2>&1 | grep -oE 'frame= *[0-9]+' | tail -1

# Is MPV flowing? ask it directly while it runs.
echo '{"command":["get_property","pause"]}'             | socat - /tmp/mpv-protohdmicapture.sock
echo '{"command":["get_property","estimated-vf-fps"]}'  | socat - /tmp/mpv-protohdmicapture.sock
echo '{"command":["get_property","frame-drop-count"]}'  | socat - /tmp/mpv-protohdmicapture.sock
```

Healthy looks like `pause: false`, `estimated-vf-fps: 60.0`, `frame-drop-count: 0`,
and `playback-time` advancing one second per second.

### MangoHud is injected into mpv

`~/.config/environment.d/99-proton-amd.conf` sets `MANGOHUD=1` globally, and
MangoHud is an **implicit** Vulkan layer — so it hooks `gpu-next`'s present path
and adds an overlay draw to every frame of a latency-sensitive feed. The
launcher runs mpv with `MANGOHUD=0`. Nothing else on the system is affected.

### `ioctl(VIDIOC_QBUF): Inappropriate ioctl for device` on exit

Harmless. It is libavdevice's untidy v4l2 teardown, not a device fault —
verified by `devnum` staying put with zero re-enumerations across many runs. The
launcher filters that exact line and nothing else.

### `input_format` silently not applied

`FMT` used to default to empty, which means "use whatever mode the device was
last left in" — so a run could appear to work while actually being at the mercy
of the previous run. It now defaults to `yuyv422`. Check what really got
negotiated with:

```bash
v4l2-ctl -d /dev/video2 --get-fmt-video
```

---

## Files

- `capture-cards/` — one profile per capture chip: the USB ids it appears
  under, its link, best mode, support status and quirks. The wizard reads
  them; [`docs/capture-cards.md`](docs/capture-cards.md) is the support
  table, generated from them by `tools/capture-cards-table`
- `lib/capture-cards.sh` — reads those profiles
- `lib/audio.sh` — mute and volume of the audio loopback (`view.sh --audio`)
- `sources/` — one profile per kind of source: suggested `RES`/`FPS` and the
  setup tips with menu paths; `lib/sources.sh` reads them
- `lib/setup.sh` — the setup wizard (`./view.sh --setup`): identifies the card
  and its USB link, lists its modes, checks the signal, picks audio, display
  mode and player, writes the config and a Markdown diagnostics report to
  `~/.local/state/protohdmicapture/`
- `lib/ui.sh` — the wizard's and dashboard's centered terminal UI
- `lib/debug.sh` — `DEBUG=log`: decision lines, perf pulse, player verbosity
- `lib/dashboard.sh` — the live debug dashboard (`./view.sh --dashboard`)
- `lib/mpv/protohdmicapture.lua` — the overlay and the keys in the mpv window
- `view.sh` — the player; every knob is an env var or config line, plus
  `--tune` (the feed with the overlay open, for audio sync). Owns the
  one-window/one-loopback rule
- `launch.sh` — GUI entry point used by the desktop launcher; see below
- `lib/settings.sh` — one row per setting (default, allowed values, players,
  flags) and the functions that resolve, validate and list them;
  `tools/fixtures/settings/` holds the configs `tools/check` runs through
  `--settings`
- `lib/card.sh` — config file, and which `/dev/video` node and PipeWire source
  are the card
- `lib/desktop.sh` — notifications, opening a terminal, focusing and shaping
  a window, refresh switching; Hyprland adapter plus a generic one for every
  other desktop
- `lib/pick-refresh-mode.py` — picks the focused monitor's mode closest to
  `FPS` and emits the `hyprctl eval` Lua for switching and restoring
- `config.example` — the settings file, documented
- `tools/check` — static checks (shellcheck, Lua/Python syntax, banned
  patterns, the capture card table being current); the pre-commit hook in
  `.githooks/` runs it once enabled with
  `git config core.hooksPath .githooks`
- `docs/testing.md`, `docs/run-dir.md`, `CODING_STANDARDS.md` — how to verify a
  change on hardware, the session-file contract, and the review rules
- `desktop/` — the `.desktop` entry and icon, symlinked into `~/.local/share`

---

## Desktop launcher

`protoHDMIcapture` shows up in the Omarchy launcher (SUPER+SPACE). Both files
live in `desktop/` in this repo and are symlinked to where XDG looks for them:

| file | what it is |
|---|---|
| `~/.local/share/applications/protohdmicapture.desktop` → `desktop/protohdmicapture.desktop` | the menu entry; `Exec=` points at `launch.sh` |
| `~/.local/share/icons/hicolor/scalable/apps/protohdmicapture.svg` → `desktop/protohdmicapture.svg` | the icon |

On a fresh machine:

```bash
ln -s "$PWD/desktop/protohdmicapture.desktop" ~/.local/share/applications/
ln -s "$PWD/desktop/protohdmicapture.svg" ~/.local/share/icons/hicolor/scalable/apps/
```

`Exec=` is an absolute path to this checkout; edit it if the repo lives
somewhere other than `~/Documents/GitHub/protoHDMIcapture`.

The rules that must hold however the player is started live in `view.sh`, not
here:

- **Won't start twice.** `view.sh` holds a lock while a player runs and records
  its PID; a second launch focuses that window by PID (Hyprland, any backend)
  or reports that it is already running. A lock rather than a window lookup,
  so nothing depends on window titles, and `omarchy launch or focus`, whose
  regex would match a browser tab called "protoHDMIcapture ...", is not needed.
- **Failures become notifications.** With no terminal attached, `die()` sends
  its reason to `notify-send` itself and exits 3 ("already reported").

`launch.sh` adds what only a menu launch needs, and works on any desktop:

- **First run:** with no config file it opens a terminal running the setup
  wizard (`xdg-terminal-exec`, else `$TERMINAL`, else `x-terminal-emulator`)
  and waits for it; the feed starts only if settings were saved. With no
  terminal at all it notifies and plays on defaults. `NO_SETUP=1` skips this.
- **Menu defaults:** `REFRESH=match` unless the config sets `REFRESH`. From a
  menu there is no place to type it, and the 100 Hz judder described above is
  the whole reason that knob exists.
- **The run log survives** at `$XDG_RUNTIME_DIR/protohdmicapture.log`.
- **Unreported failures still notify:** any other non-zero exit (a missing tool,
  a crash) gets a generic "failed, see log" notification.

Every env var still passes through, so the terminal path is unchanged:

```bash
REFRESH=keep ./launch.sh     # or just use ./view.sh directly
```

---

## The card: generic "USB3.0 Video Capture HDMI 4K" blue box

Identified from the product photo: aluminium blue case, HDMI-in, HDMI loop-out,
3.5 mm mic jack, USB-C `DC5V` aux power, LED. That is the standard
**MacroSilicon MS2130** reference design (the USB 3.0 successor to the MS2109).
Confirm once plugged in — `./view.sh --setup` prints the USB ID and names the chip.

Expected IDs: `534d:2109` = MS2109 (USB 2.0), MS2130 boards commonly enumerate
as `345f:2130` or under a `534d:` id. Whatever it is, `lsusb` decides, not the
box.

### Read the marketing correctly

"4K" on this box is the **input and passthrough** resolution. **Capture is
1080p** — the panel next to it says so ("HD 4K Input" / "HD 1080p"). Set the PS5
to 1080p output; asking for 3840x2160 over USB will silently give you something
else or nothing.

### Use YUYV, not MJPEG

This is the one place the specific chip changes the plan. The MS2130 does
**uncompressed YUV422 (YUYV) at 1080p60 over USB 3.0** — 1920×1080×2×60 ≈ 249
MB/s, which fits comfortably in USB 3's 5 Gbit/s. Uncompressed means **there is
no JPEG decode step at all**, which removes a whole stage of latency and CPU
work:

```bash
FMT=yuyv422 RES=1920x1080 FPS=60 ./view.sh
```

Try that **first**. Fall back to `FMT=mjpeg` only if the setup wizard shows YUYV
capped at a low framerate — which is what happens on the USB 2.0 MS2109, where
YUYV manages about 5 fps at 1080p and MJPEG is mandatory.

**So: plug this into a rear USB 3 port (blue/teal, or a USB-C port).** On a USB 2
port the card silently drops to the MJPEG path and you lose the main advantage.
`./view.sh --setup` prints the negotiated link speed — anything reporting `480` means
you're on USB 2 and should move it.

### Use the DC5V input

The board has a USB-C aux power jack for a reason: 4K input plus USB 3 streaming
exceeds what some ports comfortably deliver. Given that this machine already has
a USB device stuck in a reset loop (see below), power it externally and plug the
data side into a rear motherboard port — **not** the `3-5` hub.

### The HDMI loop-out is genuinely useful

First, which port is which: the source goes into the port labelled `HDMI IN`
(on this box, on the edge with `MIC`, the LED and `DC5V`); the HDMI on the
opposite edge is the loop-out. A PC plugged into `HDMI IN` sees a 1080p60
monitor (the card's EDID, "QCW MStar Demo"), and Hyprland extends the desktop
onto it. The details are in [`capture-cards/ms2130.md`](capture-cards/ms2130.md).

Two things it buys you:

1. **A real latency measurement.** Run the loop-out to a second display, film
   both screens in phone slow-mo, count frames. That is a true glass-to-glass
   number rather than a guess.
2. **The honest fallback.** Reported MS2130 internal latency is roughly **6–11
   frames** (~100–180 ms at 60 fps) and that is *inside the card* — no player
   flag on this side can touch it. For anything twitchy, play off the loop-out
   and use the PC purely for the picture.

Set expectations accordingly: this setup will be excellent for playing on the PC
and fine for slower games, but it will not feel like a directly-connected
display, and OBS was never what was holding that back.

### The mic jack

The card mixes the 3.5 mm mic input into the captured USB audio stream. With
nothing plugged in, some units inject audible hiss. If the audio sounds noisy,
that is the first suspect — not PipeWire.

### Audio quirks: already handled

The MS2109 has a well-known firmware bug — it declares 96 kHz mono when it is
really 48 kHz stereo, with swapped channels and unaligned transfers. On Windows
people run [mono-to-stereo](https://github.com/ToadKing/mono-to-stereo) for it.
**On Linux this is fixed in the kernel** (`QUIRK_AUDIO_ALIGN_TRANSFER` plus the
96 kHz-mono → 48 kHz-stereo remap, mainlined years ago and backported as far as
4.9). This box runs 7.1.9, so it is handled automatically — nothing to do.

If the audio still comes through mono, at half speed, or with L/R swapped,
that's the signal the quirk didn't match your exact ID; capture the output of
`lsusb -v -d <id>` and `dmesg | grep -i usb-audio` and it can be dealt with.

### Other cards, for reference

Not yours, but this is why the model mattered:

- **Elgato Cam Link 4K** — needs a [known workaround](https://github.com/AdamGleave/elgato-camlink-workaround) on Linux
- **Elgato HD60 X** — [reported on the Arch forums](https://bbs.archlinux.org/viewtopic.php?id=283338) to send one frame then hang
- **Magewell USB Capture** — clean UVC/UAC, no workarounds needed

---

## Sources

- [mpv wiki: Video4Linux2 Input](https://github.com/mpv-player/mpv/wiki/Video4Linux2-Input)
- [mpv manual](https://mpv.io/manual/stable/) (and the locally installed `man mpv` for 0.41.0)
- [FFmpeg devices documentation](https://ffmpeg.org/ffmpeg-devices.html) — v4l2 `timestamps`, `input_format`, `video_size`
- [libpipewire-module-loopback(7)](https://man.archlinux.org/man/libpipewire-module-loopback.7.en) — `target.delay.sec`
- [PipeWire: Loopback](https://docs.pipewire.org/page_module_loopback.html)
- [Hyprland wiki: Tearing](https://wiki.hypr.land/Configuring/Advanced-and-Cool/Tearing/)
- [PowerPyx: PS5 how to disable HDCP](https://www.powerpyx.com/ps5-how-to-disable-hdcp/)
- [Elgato: HDCP and Elgato Game Capture devices](https://help.elgato.com/hc/en-us/articles/360040482032-HDCP-and-Elgato-Game-Capture-devices)
- [Level1Techs: low latency preview applications for a v4l2 USB capture device](https://forum.level1techs.com/t/low-latency-preview-applications-for-a-v4l2-usb-capture-device-aspect-ratio-problems/174332)
- [Elgato HD60 X hangs on Arch](https://bbs.archlinux.org/viewtopic.php?id=283338)
- [elgato-camlink-workaround](https://github.com/AdamGleave/elgato-camlink-workaround)
- [UltraSemi: MacroSilicon MS2130 datasheet page](https://doc.ultrasemi.com/en/ic/macrosilicon/ms2130) — USB 3.0, YUV422 + MJPEG
- [CNX Software: MS2130-based "4K" HDMI to USB 3.0 dongle](https://www.cnx-software.com/2022/11/07/ms2130-based-4k-hdmi-to-usb-3-0-video-capture-dongle-sells-for-19/)
- [HyperHDR discussion #499: MS2130 vs MS2109](https://github.com/awawa-dev/HyperHDR/discussions/499) — internal latency ~6–11 frames
- [ALSA: usb-audio: add quirk for MacroSilicon MS2109](https://www.spinics.net/lists/stable/msg395797.html) — the in-kernel audio fix
- [ToadKing/mono-to-stereo](https://github.com/ToadKing/mono-to-stereo) — the Windows-side workaround Linux doesn't need
- [Making the MacroSilicon MS2109 work on Linux](https://www.mjt.me.uk/posts/fixing-missing-macrosilicon-ms2109/)
