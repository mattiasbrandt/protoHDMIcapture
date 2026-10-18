<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/logo-lockup-for-dark-bg.svg">
    <img alt="protoHDMIcapture" src="assets/brand/logo-lockup-for-light-bg.svg" width="480">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/mattiasbrandt/protoHDMIcapture/actions/workflows/check.yml"><img alt="check" src="https://github.com/mattiasbrandt/protoHDMIcapture/actions/workflows/check.yml/badge.svg"></a>
  <a href="https://github.com/mattiasbrandt/protoHDMIcapture/actions/workflows/release.yml"><img alt="release" src="https://github.com/mattiasbrandt/protoHDMIcapture/actions/workflows/release.yml/badge.svg"></a>
</p>

Play a console, a camera or another PC on your screen through a USB capture
card, on Linux and Windows. protoHDMIcapture shows the picture and plays the
sound, and nothing else: no recording, no streaming.

- **No bloated vendor app.** No account, no streaming suite, no background
  service. One program that opens straight to the picture.
- **Simple and open source.** One binary, one plain-text settings file, and
  a setup wizard that asks only what it needs. MIT or Apache-2.0, built in
  the open.
- **Built for low latency.** About 45 ms from a frame reaching the PC to the
  screen at 1080p60. The GPU does the picture work, and the sound runs
  beside the picture instead of holding it back.
- **Made for cheap no-name capture cards.** The capture cards sold under
  dozens of names for the price of a game, with a MacroSilicon chip inside.
  The viewer recognises the chip by its USB id, knows its quirks, and picks
  its best mode.

It started as a way to play a PS5 in a window, so most measurements were
taken with a PS5, the MS2130 and the MS2109. Nothing in it is PS5-specific.

## Contents

- [Why it is fast](#why-it-is-fast)
- [Install](#install)
- [Quick start](#quick-start)
- [Keys and the overlay](#keys-and-the-overlay)
- [The setup wizard](#the-setup-wizard)
- [Settings](#settings)
- [Display modes and refresh](#display-modes-and-refresh)
- [Debugging and the dashboard](#debugging-and-the-dashboard)
- [Capture cards and sources](#capture-cards-and-sources)
- [Troubleshooting](#troubleshooting)
- [The script viewer (view.sh)](#the-script-viewer-viewsh)
- [Development](#development)
- [License](#license)

## Why it is fast

The usual tools (OBS, or a player with an audio track) run picture and sound
through one pipeline, which has to reconcile two clocks that drift against
each other and pays for that with buffering. protoHDMIcapture splits them:

| | Path | Latency |
|---|---|---|
| Picture | the card's frames straight to the screen, no audio track, each frame shown as soon as the display can take it | about 45 ms from the frame reaching the PC to the screen |
| Sound | a loopback from the card audio input to your output, never through the picture's path | about 10 to 20 ms, plus the audio sync |
| Audio sync | one fixed delay on the sound, tuned once | none |

Nothing has to stay in sync, so nothing needs a buffer: the sound arrives
early and the audio sync holds it back to match. The viewer holds a frame
for the display's next refresh only within 5 ms of it, and otherwise shows
the newest. How it was measured: [docs/testing.md](docs/testing.md#measuring-latency);
why its own renderer: [ADR 0001](docs/adr/0001-rust-viewer-own-renderer.md).

**The GPU does the picture work.** The viewer draws with wgpu, on Vulkan
(Linux) or DirectX 12 (Windows). An uncompressed YUYV frame goes to the GPU
as the card sent it: the conversion to RGB, the levels and colour
adjustments, and the scaler (`SCALING`) all run as shaders, so the CPU only
copies the frame. MJPEG frames (the MS2109) are decoded to YUYV on the CPU
first, about 2 ms at 720p and 5 ms at 1080p, then take the same GPU path.
`--stats` prints the GPU time of each frame's passes, where the driver
reports it.

The largest term is the capture card's own latency (6 to 11 frames reported
for the MS2130), which no software reaches. For twitchy games, play off the
card's HDMI loop-out on a real display and use the PC for the picture.

## Install

Packages are on the [GitHub releases](https://github.com/mattiasbrandt/protoHDMIcapture/releases);
[packaging/INSTALL.md](packaging/INSTALL.md) has the details.

| Platform | How |
|---|---|
| Arch Linux | the AUR package `protohdmicapture` (once published), or `cd packaging && makepkg -si` |
| Other Linux | the release archive: `tar xf protohdmicapture-<version>-x86_64-linux.tar.gz`, then `./install.sh` in it |
| Windows 10/11 | the release's `.msi`: per user, no administrator prompt |

Linux needs a Wayland desktop with a Vulkan driver and PipeWire. Refresh
matching and window placement need Hyprland; other desktops get the feed
without them. Windows needs a DirectX 12 driver.

From a checkout: `cargo build --release` in `viewer/` (Rust stable, clang,
and the development files of PipeWire, ALSA, D-Bus, Wayland and
libxkbcommon); the binary is `viewer/target/release/protohdmicapture`.

## Quick start

```bash
protohdmicapture              # the feed; with no settings file, the setup wizard first
protohdmicapture --setup      # the setup wizard, then the feed
protohdmicapture --settings   # each setting, its value and where it came from
protohdmicapture --debug      # the feed, with the dashboard
protohdmicapture --help       # every flag
```

The menu entry runs `protohdmicapture --menu`: `REFRESH=match` unless the
settings say otherwise, and a desktop notification when it fails or has no
sound. On Windows, started from the Start menu, a fatal error keeps its
console open until Enter.

For measuring: `--stats` prints the latency numbers every 10 s and a
summary, `--seconds N` ends the feed after N seconds, `--test-pattern`
shows colour bars instead of the card, and `--monitor NAME` opens the
window on that monitor (Hyprland).

One video window and one audio loopback run at a time: a second launch
focuses the running window, or says it is already running.

## Keys and the overlay

| Key | In the window |
|---|---|
| **Tab** | open or close the overlay |
| **f** | cycle fullscreen → borderless → windowed |
| **d** | open the dashboard (on another monitor, on Hyprland); with one open, raise it |
| **s** | a screenshot to `~/Pictures/protoHDMIcapture/` |
| **ctrl −** / **ctrl +** | audio sync 10 ms earlier / later, live |
| **alt −** / **alt +** | audio sync 50 ms earlier / later |
| **m**, **9** / **0** | mute; volume down / up of the loopback |
| **1**–**8** | contrast, brightness, gamma, saturation down / up |
| **q** | quit |

For the first 10 seconds a bar along the bottom lists the main keys. **Tab**
opens the overlay: a panel with the keys and the live settings, each with
its current value, and these keys:

| Key | With the overlay open |
|---|---|
| **l** | levels: auto → limited → full (the range of the card's frames) |
| **c** | scaling: balanced → fast → high-quality |
| **r** | reset the colour |
| **t** | the setup tips for the source; **←** / **→** browse the other source profiles |
| **w** | save what changed since the feed started |
| **Esc**, **Tab** | close |

Every change is a live change: it lasts until the viewer closes, unless
**w** saves it. The volume is not a setting: the sound system remembers the
loopback's level between sessions, as it does for any app. The audio sync
keys also start the loopback again if it died.

**No signal.** When the capture card delivers no picture, a banner says so:
after 5 s if none has come since the feed opened ("is the source on and
connected?"), or 2 s after the last one (a source gone to sleep; the source
profile says how to wake it). It goes with the first new frame. A card that
keeps sending frames shows no banner: the MS2130 streams its own "No
Signal" box for about a minute after the source sleeps, and the MS2109
streams black with nothing plugged in.

## The setup wizard

`protohdmicapture --setup` runs in the viewer's window. Each question starts
on the current value, and nothing is written before the review.

1. **Capture card:** every video device, the one with a known chip's USB id
   recommended. It is pinned by USB serial (`CARD_SERIAL`), by USB port
   (`CARD_PORT`) when it reports no serial, or as a last resort by its node
   (`DEV`, which can change on a replug). Then the card's details: the chip
   its USB id implies, its capture card profile and support status, and the
   USB link speed, with a warning at 480 Mbit/s (USB 2) for a chip that
   needs USB 3. Every other device can be marked an ignored device.
2. **Source:** what is plugged into the card (a source profile, or any HDMI
   source) and its setup tips.
3. **Picture format:** the card's own modes, best first, starting on the
   mode the source profile suggests or the closest one the card has.
4. **Signal check:** 2 seconds of capture at that mode, measuring the real
   frame rate, and checking a few frames for an all-black picture (the
   MS2109 streams black at full rate with nothing plugged in).
5. **Audio:** the card audio input, another input, or none.
6. **Display:** the display mode and window size, refresh matching, and
   scaling.
7. **Audio sync:** optional; the sound plays while you watch, and the keys
   move it until it lines up.
8. **Troubleshooting:** debug output off, a log, or a log and the dashboard.
9. **Review:** exactly what will change, then save.

## Settings

Settings come from the environment, else the settings file, else their
default. The file is `~/.config/protohdmicapture/config` on Linux and
`%APPDATA%\protohdmicapture\config` on Windows: one `KEY=value` per line,
read, not executed; [config.example](config.example) documents it. The
wizard writes it, and so does **w** in the overlay. `--settings` shows every
value, where it came from, and whether it is valid; a value out of bounds
stops the run with a message naming it.

`lib/settings.sh` holds one row per setting, shared by both viewers.

### The settings worth knowing

| Setting | Default | What it does |
|---|---|---|
| `SOURCE` | — | what is plugged in: a source profile name from `sources/` (`ps5`); its setup tips show in the overlay |
| `CARD_SERIAL` | — | the capture card's USB serial; pins it among other cameras and across `/dev/video` renumbering |
| `CARD_PORT` | — | the USB port (udev `ID_PATH`) of a card that reports no serial; found again after a replug into the same port. Any of `CARD_SERIAL`, `CARD_PORT`, `DEV` set in the environment overrides all three in the file |
| `DEV` | — | the card's node (`/dev/videoN`) instead of finding it; node numbers move on a replug, so prefer the serial or the port. A `DEV` that is gone, not a capture node, or an ignored device is set aside with a warning |
| `IGNORE_CARDS_RE` | — | ignored devices: card types (extended regex) never opened as the capture card, e.g. `Integrated Camera` |
| `SRC` | — | the audio input to play; default: the card audio input, found on the card's USB device |
| `SINK` | — | the output to play it on; default: the desktop's default output, followed when it changes |
| `RES` | `1920x1080` | capture size; the wizard picks it from the card's own modes |
| `FPS` | `60` | capture frame rate; also what `REFRESH=match` matches |
| `FMT` | `yuyv422` | capture format: `yuyv422` (uncompressed) or `mjpeg`. `nv12` and `rawvideo` are view.sh only |
| `DELAY` | `0.05` | the audio sync in seconds, 0 to 1; **ctrl/alt −/+** tune it live, **w** saves it |
| `LAT_MS` | `10` | the audio loopback's buffer in ms; raise it if the sound crackles |
| `AUDIO` | `loopback` | `loopback`, or `none` for no sound; `mpv` is view.sh only |
| `WINDOW` | `fullscreen` | `fullscreen`, `borderless` or `windowed` (see below); **f** cycles it |
| `WINDOW_SIZE` | `1280x720` | the window's size when windowed |
| `REFRESH` | `keep` | `match` switches the monitor to its refresh nearest `FPS` for the session and restores it after (Hyprland) |
| `SCALING` | `balanced` | `balanced` (spline36 or mitchell), `fast` (bilinear, the least GPU time, softer) or `high-quality` (EWA Lanczos upscale, more GPU time); `config` is view.sh's mpv.conf, read as balanced here. **c** cycles it |
| `LEVELS` | `auto` | the range of the card's frames: `limited` or `full` if blacks look crushed or washed out. `auto` reads YUYV as limited and MJPEG as full |
| `BRIGHTNESS`, `CONTRAST`, `SATURATION`, `GAMMA` | `0` | colour adjustments, -100 to 100; keys **1**–**8** change them, **w** saves them |
| `DEBUG` | `off` | `log` or `window`; see [Debugging](#debugging-and-the-dashboard). `--debug` is `window` for one run |
| `BACKEND` | `mpv` | view.sh's player: `mpv`, or `gst` and `ffplay` for comparisons; the Rust viewer refuses the last two |
| `IPC` | `/tmp/mpv-protohdmicapture.sock` | view.sh only: mpv's IPC socket |
| `GST_IO_MODE` | `mmap` | view.sh's gst only: `mmap` or `dmabuf` |
| `FULLSCREEN` | — | deprecated: `yes` is `WINDOW=fullscreen`, `no` is `WINDOW=windowed` |

## Display modes and refresh

`WINDOW` works like a game's display setting:

| `WINDOW` | What you get | Latency |
|---|---|---|
| `fullscreen` (default) | covers the monitor it opens on | lowest: direct scanout where the desktop allows it |
| `borderless` | maximized with no border; the desktop's bar stays visible | composited |
| `windowed` | a window of `WINDOW_SIZE`, centred | composited |

**Refresh matching.** A 60 fps source on a 100 Hz monitor lands 1.67
refreshes apart, so frames are held for one or two refreshes in turn:
judder no player can fix, unless the monitor runs variable refresh.
`REFRESH=match` switches the focused monitor (or `--monitor`'s) to its mode
nearest `FPS` at the same resolution, and puts it back when the feed ends.
Resolution, position, scale and transform are kept, so a multi-monitor
layout stays as it was. It works on Hyprland; elsewhere, and on Windows,
the viewer says so and leaves the display alone.

**On Hyprland**, blur, shadows and animations cost GPU time on a live feed
for nothing. A window rule for the title `protoHDMIcapture` turns them off;
with a Lua config (as Omarchy has) that is a `window` rule with `no_blur`,
`no_shadow` and `no_anim`. Hyprland in Lua rejects `hyprctl keyword`
silently: use `hyprctl eval`.

## Debugging and the dashboard

`DEBUG=log` records every decision (each setting and where it came from,
the card found, the audio link, refresh and window placement), the format
the card was opened at, and a perf line every 5 s, to the terminal and
to `debug.log` in the run directory (`$XDG_RUNTIME_DIR/protohdmicapture/`;
on Windows `%LOCALAPPDATA%\protohdmicapture\`).

`DEBUG=window`, or `--debug` for one run, adds the dashboard in its own
terminal: frame rates, drops, repeated refreshes, latency, CPU, the audio
sync, underruns, the monitors and recent events, every second.
`protohdmicapture --dashboard` attaches one to a running viewer, and **d**
opens it from the window. Closing it never touches the feed.

## Capture cards and sources

**Capture cards.** [docs/capture-cards.md](docs/capture-cards.md) is the
support table: which chips have been tested, probed or only reported, and
their best modes. Each chip's capture card profile in
[capture-cards/](capture-cards/) holds its USB ids and quirks. In short:

- **MS2130** (USB 3, often sold as "USB3.0 Video Capture HDMI 4K"): tested.
  Uncompressed YUYV at 1080p60, so no decode step; it needs a 5 Gbit/s
  port and is steadiest with its DC5V power connected. "4K" is the input
  and loop-out; capture is 1080p.
- **MS2109** (USB 2 sticks): tested. MJPEG at 720p60 or 1080p30, decoded
  on the CPU.

A card that is not in the table is worth trying: the wizard shows what it
offers, and the support status grows with reports.

**Sources.** The settings on the source itself (HDCP, audio format, HDR,
resolution) that give a black picture or bad sound when wrong live in one
source profile each in [sources/](sources/): [PlayStation 5](sources/ps5.md)
(tested), [PlayStation 4](sources/ps4.md), [Xbox Series X/S](sources/xbox-series.md),
[Nintendo Switch](sources/switch.md), [Nintendo Switch 2](sources/switch-2.md),
[a Windows PC](sources/pc.md) and [any HDMI source](sources/generic.md).
**HDCP off first**: nothing else matters until it is.

## Troubleshooting

- **Black picture.** HDCP on the source is the usual cause; then whether
  the source is awake. A frame count proves frames, not a picture:
  `v4l2-ctl -d /dev/videoN --stream-mmap --stream-count=60 --stream-to=/dev/null`.
- **Low frame rate or no uncompressed mode.** The card is on a USB 2 port:
  the wizard's card details show the link speed. Use a USB 3 port, not a
  hub.
- **The card resets or drops off the bus.** Power: connect its DC5V input,
  and keep it off bus-powered hubs.
- **An MS2130 sends only broken frames.** It can get stuck; unplug both its
  USB cable and its DC5V power, then plug them back
  ([capture-cards/ms2130.md](capture-cards/ms2130.md)).
- **Hiss in the sound.** Some cards mix their 3.5 mm mic jack into the
  captured sound.
- **The sound comes before or after the picture.** Tune the audio sync with
  **ctrl −/+** and save it with **w**; console menu clicks make a good test.
  Re-tune after changing the capture card or the display.
- **Not in the `video` group?** The desktop session gets access to
  `/dev/video*` anyway (logind), but a system service does not.

How long a frame really takes, glass to glass: film the card's loop-out
display and the viewer side by side at 240 fps and count the frames
between them.

## The script viewer (view.sh)

`view.sh` is the original viewer, bash around mpv and `pw-loopback`, Linux
only, run from this checkout. It reads the same settings file, so it plays
the same setup; it is kept as an alternative until the Rust viewer replaces
it everywhere. Its players, its menu entry and its own troubleshooting are
in [docs/view-sh.md](docs/view-sh.md).

```bash
./view.sh            # the feed; the first run offers the wizard in the terminal
./view.sh --help     # every flag
```

## Development

| Path | What it is |
|---|---|
| `viewer/` | the Rust viewer |
| `capture-cards/`, `sources/` | capture card profiles and source profiles, read by both viewers |
| `view.sh`, `launch.sh`, `lib/`, `desktop/` | the script viewer |
| `packaging/` | the PKGBUILD, the release archive, the Windows installer, the icons |
| `tools/` | `check`, the hardware-free harnesses, the lost-frame simulation, fixtures |
| `docs/` | testing, the run directory, the capture card table, ADRs, research |
| `assets/brand/` | the logo and the social preview |

- `tools/check` runs every check: lint, syntax, consistency between code
  and docs, the Rust viewer's build, tests and clippy (for Windows too,
  with the `x86_64-pc-windows-msvc` target), and the harnesses. CI runs it
  on every push; `git config core.hooksPath .githooks` runs it before each
  commit.
- [docs/testing.md](docs/testing.md) lists the traps that fake failures in
  a run against a real card; [docs/run-dir.md](docs/run-dir.md) is the
  contract for the session files.
- [GLOSSARY.md](GLOSSARY.md) names the project's terms;
  [CODING_STANDARDS.md](CODING_STANDARDS.md) is the review checklist.

## License

Either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
