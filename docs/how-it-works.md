# How protoHDMIcapture works, in detail

The [README](../README.md) is the short version. This page holds the detail
behind it: why the delay is low, what each setup wizard step does, the
debug log, display details, and the less common problems.

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
the newest. How it was measured: [testing.md](testing.md#measuring-latency);
why its own renderer: [ADR 0001](adr/0001-rust-viewer-own-renderer.md).

**The GPU does the picture work.** The viewer draws with wgpu, on Vulkan
(Linux) or DirectX 12 (Windows). An uncompressed YUYV frame goes to the GPU
as the card sent it: the conversion to RGB, the levels and colour
adjustments, and the scaling (Upscaling or the resize filter) all run as
shaders, so the CPU only copies the frame. MJPEG frames (the MS2109) are
decoded to YUYV on the CPU first, about 2 ms at 720p and 5 ms at 1080p,
then take the same GPU path. `--stats` prints the GPU time of each frame's
passes, where the driver reports it.

The largest term is the capture card's own latency (6 to 11 frames reported
for the MS2130), which no software reaches. For twitchy games, play off the
card's HDMI loop-out on a real display and use the PC for the picture.

How long a frame really takes, glass to glass: film the card's loop-out
display and the viewer side by side at 240 fps and count the frames
between them.

## Starting it

```bash
protohdmicapture              # the feed; with no settings file, the setup wizard first
protohdmicapture --setup      # the setup wizard, then the feed
protohdmicapture --settings   # each setting, its value and where it came from
protohdmicapture --debug      # the feed, with the dashboard
protohdmicapture --help       # every flag
protohdmicapture --version    # the version
```

The menu entry runs `protohdmicapture --menu`: `REFRESH=match` unless the
settings say otherwise, and a desktop notification when it fails or has no
sound. On Windows, started from the Start menu, a fatal error keeps its
console open until Enter.

One video window and one audio loopback run at a time: a second launch
focuses the running window, or says it is already running.

It started as a way to play a PS5 in a window, so most measurements were
taken with a PS5, the MS2130 and the MS2109. Nothing in it is PS5-specific.

## Flags for measuring

`--stats` prints the latency numbers every 10 s and a summary,
`--seconds N` ends the feed after N seconds, `--test-pattern` shows colour
bars instead of the card, and `--monitor NAME` opens the window on that
monitor (Hyprland). `protohdmicapture --help` lists every flag.

## The setup wizard, step by step

The wizard runs in the viewer's window: `protohdmicapture --setup`, or **u**
in the overlay. Each question starts on the current value, and nothing is
written before the review.

1. **Capture card:** every video device, the one with a known chip's USB id
   recommended. It is pinned by USB serial (`CARD_SERIAL`), by USB port
   (`CARD_PORT`) when it reports no serial, or as a last resort by its node
   (`DEV`, which can change on a replug). Then the card's details: the chip
   its USB id implies, its capture card profile and support status, and the
   USB link speed, with a warning at 480 Mbit/s (USB 2) for a chip that
   needs USB 3. Every other device can be marked an ignored device.
2. **Source:** what is plugged into the card (a source profile, or any HDMI
   source).
3. **Picture format:** the card's own modes, best first, starting on the
   mode the source profile suggests or the closest one the card has.
4. **Signal check:** first the source's setup tips over its live picture,
   so a setting changed on the source shows at once; then 2 seconds of
   capture at that mode, measuring the real frame rate, and checking a few
   frames for an all-black picture (the MS2109 streams black at full rate
   with nothing plugged in).
5. **Audio:** the card audio input, another input, or none.
6. **Display:** the display mode and window size, then refresh matching.
7. **GPU enhancements:** Upscaling, turned on with a dedicated GPU on a
   monitor that enlarges the picture; then, on a dedicated GPU, frame
   generation offered with its cost and delay, off unless you pick it.
   This window's monitor decides: move it to the monitor you watch on.
8. **Audio sync:** optional; the sound plays while you watch, and the keys
   move it until it lines up.
9. **Troubleshooting:** debug output off, a log, or a log and the dashboard.
10. **Review:** exactly what will change, then save.

## The overlay and live changes

For the first 10 seconds a bar along the bottom lists the main keys; **Tab**
opens the overlay, a panel with the keys and the live settings, each with
its current value. **d** opens the dashboard on another monitor (on
Hyprland); with one open, it raises it.

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

## Display details

`WINDOW` works like a game's display setting:

| `WINDOW` | What you get | Latency |
|---|---|---|
| `fullscreen` (default) | covers the monitor it opens on | lowest: direct scanout where the desktop allows it |
| `borderless` | maximized with no border; the desktop's bar stays visible | composited |
| `windowed` | a window of `WINDOW_SIZE`, centred | composited |

**The resize filter** (`SCALING`), where Upscaling does not enlarge the
picture: `balanced` is spline36 enlarging and mitchell shrinking, `fast` is
bilinear (softer), `high-quality` is EWA Lanczos.

**Refresh matching.** A 60 fps source on a 100 Hz monitor lands 1.67
refreshes apart, so frames are held for one or two refreshes in turn:
judder no player can fix, unless the monitor runs variable refresh.
`REFRESH=match` switches the focused monitor (or `--monitor`'s) to its mode
nearest `FPS` at the same resolution, and puts it back when the feed ends.
Resolution, position, scale and transform are kept, so a multi-monitor
layout stays as it was. It works on Hyprland; elsewhere, and on Windows,
the viewer says so and leaves the display alone.

**On a bigger or wider monitor.** The picture keeps its shape: it is
scaled to the largest size that fits, and black bars fill the rest (an
ultrawide gets them at the sides; a 1080p feed on a 2560x1080 monitor is
shown pixel for pixel, with no scaling at all). Capture cards send colour
at half the width of the picture; protoHDMIcapture fills it back in
smoothly, so coloured edges and HUD text stay crisp.

**On Hyprland**, blur, shadows and animations cost GPU time on a live feed
for nothing. A window rule for the title `protoHDMIcapture` turns them off;
with a Lua config (as Omarchy has) that is a `window` rule with `no_blur`,
`no_shadow` and `no_anim`. Hyprland in Lua rejects `hyprctl keyword`
silently: use `hyprctl eval`.

## The debug log and the dashboard

`DEBUG=log` records every decision (each setting and where it came from,
the card found, the audio link, refresh and window placement), the format
the card was opened at, and a perf line every 5 s, to the terminal and
to `debug.log` in the run directory (`$XDG_RUNTIME_DIR/protohdmicapture/`;
on Windows `%LOCALAPPDATA%\protohdmicapture\`).

`DEBUG=window`, or `--debug` for one run, adds the dashboard in its own
terminal: frame rates (the card's, and the source's own, told from its
duplicate frames, with its cadence and cadence breaks), drops, repeated
refreshes, latency, CPU, the audio sync, underruns, the monitors and
recent events, every second. `protohdmicapture --dashboard` attaches one to
a running viewer, and **d** opens it from the window. Closing it never
touches the feed.

## Settings: where they come from

Settings come from the environment, else the settings file, else their
default. The file is read, not executed; [config.example](../config.example)
documents it. `--settings` shows every value, where it came from, and
whether it is valid; a value out of bounds stops the run with a message
naming it. [`settings.rows`](../settings.rows) holds one row per setting;
protoHDMIcapture legacy reads the same rows.

## Less common problems

- **Hiss in the sound.** Some cards mix their 3.5 mm mic jack into the
  captured sound.
- **Not in the `video` group?** The desktop session gets access to
  `/dev/video*` anyway (logind), but a system service does not.
- **An MS2130 sends only broken frames.** It can get stuck; unplug both its
  USB cable and its DC5V power, then plug them back
  ([capture-cards/ms2130.md](../capture-cards/ms2130.md)).
