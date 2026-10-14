# Rust viewer PoC: findings (#5)

Research note, 2026-10-14. Scope: whether a single-process Rust viewer
(capture, display and audio in one program) meets #5's go criteria on Linux
with the MS2130 and a PS5. The code is `poc/rust/` (`hdmipoc capture | view |
audio`); the run logs it cites are `poc/rust/runs/`. Measured on: RX 9070 XT
(RADV, Mesa 26.2.2), Hyprland, wgpu 30.0.1, winit 0.30.13, cpal 0.18.2,
PipeWire; every run with `MANGOHUD=0` (see "What broke").

**Decision (the user, 2026-10-14): no-go** on the budget as written: the
feed's median time to the screen (23.7 ms at best) misses 16.7 ms, and
added visual delay in games is not acceptable. Capture and audio passed.

**Measured after the decision:** today's viewer (view.sh -> mpv) is ~29 ms
slower than the PoC on the same screen (kernel -> presented median 69.0 vs
40.5 ms; "Compared with today's viewer"). The decision is open again on
that basis, and the user keeps the PoC: other tech approaches get
evaluated with the same timing method before any go / no-go.

## Results against the go criteria

| Criterion | Result | Verdict |
|---|---|---|
| Feed: no capture gaps over 10 min | 0 gaps in every 10-minute run (36,001 frames; 30,872 in the one the PS5 cut short) | ✅ |
| Audio: buffers of 10 ms or less | 256-frame (5.3 ms) quanta on both streams; 480 asked | ✅ |
| Audio: zero underruns over 10 min | 0 ring underruns, 0 xruns, every run | ✅ |
| Audio sync changes live, no gap, no restart | 50 → 150 → 20 → 50 → 150 ms in one run; each reached in 1.8–5.4 s by playing at most 3 % fast or slow; 0 underruns | ✅ |
| Video: dequeue → presented median ≤ 16.7 ms | best 23.7 ms (direct scanout) | ❌ |
| Video: p99 ≤ 33.3 ms | 32.9 ms with direct scanout; 33.35 composited | ✅ with scanout |

The video criterion measures from **dequeue**, not from the kernel
timestamp: the user changed it on 2026-10-14 (see "What broke").

## The 10-minute runs

All fullscreen on DP-2, dequeue → presented, from the compositor's
`wp_presentation` feedback. "Repeated" counts refreshes that showed the same
frame again.

| Run (log in `poc/rust/runs/2026-10-14-…`) | Display | Median | p99 | Repeated | Notes |
|---|---|---|---|---|---|
| `fullscreen-fifo-dp2` | 59.95 Hz | 71.8 | 82.5 | 284 | unpaced: a queue built up (below) |
| `fullscreen-mailbox-paced-dp2` | 59.95 Hz | 24.4 | 33.35 | 395 | composited (flags 0x7) |
| `fullscreen-fifo-paced-dp2` | 59.95 Hz | 25.8 | 33.7 | 1191 | 515 s: the PS5 went to rest |
| `fullscreen-mailbox-paced-scanout-dp2` | 59.95 Hz | 23.7 | 32.9 | 5 | direct scanout (flags 0xf) |
| `fullscreen-mailbox-paced-dp2-60hz` | 60.00 Hz | 25.8 | 33.6 | (no signal inflates it) | DP-2 at 1920x1080@60.00; no signal 382–589 s (PS5 in rest) |

Kernel timestamp → presented is ~16 ms more in every run (the USB transfer).
Windowed was measured only in 15 s runs (Fifo, unpaced: median 38.3 ms).

## Why the median sits at ~24 ms

With pacing, the viewer holds no frames back: one frame in flight, no creep
over 10 minutes. What remains is the compositor and the clocks:

- **The spread is one refresh wide.** A 60 s paced run: min 13.6, p1 16.3,
  median 25.6, p99 33.6, max 35.2 ms. A frame waits one full refresh (the
  compositor commits a refresh ahead) plus wherever it landed in the refresh
  before: uniform over ~16.7–33.4 ms, median ~25.
- **The phase drifts with the clock mismatch.** The card runs at 60.000 Hz,
  DP-2 at 59.95 Hz, so where a frame lands in the refresh drifts round in
  ~20 s; 10 s medians swing 19–29 ms in every 59.95 Hz run.
- **With matched rates the phase nearly stops.** At 60.00 Hz each 10 s
  window's spread collapses to ~2 ms (median 25.4 / p99 27.4, then 29.5 /
  31.2 …) around wherever the phase sits, and the phase creeps ~2 ms a
  minute (the two clocks still differ slightly). Over the run the median is
  the same 25.8 ms: where the phase sits is luck, and it averages out.
- **Direct scanout does not move the median** (23.7 vs 24.4 ms): it removes
  the compositing pass and nearly all repeated refreshes, not the one-refresh
  lead.

So the 16.7 ms median would need the frame shown in the refresh it arrives
in: tearing (`Immediate` present + Hyprland `allow_tearing`) or VRR, both
compositor-side and outside the one tuning round #5 allows. The same floor
applies to any vsynced viewer on this compositor. mpv, measured, does not
reach it: it queues frames as the unpaced PoC did (next section).

## Compared with today's viewer

Same screen (DP-2, 59.95 Hz, fullscreen), same card, same PS5, measured
from outside every player: `hdmipoc meta` reads the card's metadata node
(uvcvideo copies each frame's sequence number and kernel timestamp onto
it) while the player owns the video node, and
`poc/rust/tools/present-latency.py` matches each present's
`wp_presentation` feedback (from `WAYLAND_DEBUG=client`) to the newest
frame the card had finished. Kernel timestamp -> presented, so it
includes the ~16 ms USB transfer in every row. For mpv this is a lower
bound: if mpv shows an older frame than the newest, it is slower still.
Checked on the PoC: the external method gave median 42.19 / p99 49.75 /
max 54.21 ms, exactly its own timing. GStreamer's waylandsink and ffplay
(SDL over EGL) ask for no presentation feedback; `tools/present-shim.c`
(LD_PRELOAD) asks for it on every commit of any Wayland app, and on the
PoC it too matched the PoC's own timing exactly (41.50 / 49.77 / 51.01).

| Viewer | Median | p99 | Run (`poc/rust/runs/2026-10-14-…`) |
|---|---|---|---|
| PoC, paced Mailbox | 40.5 | 49.5 | `fullscreen-mailbox-paced-dp2`, 10 min |
| PoC, paced, direct scanout | 39.9 | 49.1 | `fullscreen-mailbox-paced-scanout-dp2`, 10 min |
| view.sh (mpv `--untimed --profile=low-latency --swapchain-depth=1`) | 69.0 | 81.9 | `viewsh-fullscreen-dp2`, ~90 s |
| bare mpv (the user's `mpv.conf`) | 74.2 | 82.2 | `mpv-bare-fullscreen-dp2`, 3 min |
| GStreamer (view.sh `BACKEND=gst` pipeline) | 57.4 | 65.8 | `gst-fullscreen-dp2`, 3 min (#6) |
| ffplay | 58.0 | 66.0 | `ffplay-fullscreen-dp2`, 3 min (#6) |

- **view.sh's mpv builds the same queue** the unpaced PoC did: 41.7 ms in
  the first 10 s, then +8.6 ms every 10 s (the 60.000 vs 59.95 Hz
  difference) until it levels off at ~70–85 ms after ~50 s.
  `--swapchain-depth=1` does not stop it. The run is ~90 s because the
  user quit it (keys reached the fullscreen window); the queue had
  levelled off by then.
- **Bare mpv** sits at ~70–78 ms from the start, and lost frames: 490
  sequence gaps in 3 minutes (10,241 frames, ~57 fps), with no gaps in
  the view.sh or PoC runs.
- **GStreamer and ffplay** don't build mpv's queue but sit one refresh
  behind the PoC (~57–58 ms). GStreamer lost 1 frame in 3 minutes,
  ffplay none. ffplay opens fullscreen on a display of its own choosing
  (DP-1 twice); it was started windowed on DP-2, then put in fullscreen.
- The PoC's lead comes from one thing: it hands the compositor a frame
  only after the compositor's frame callback for the previous one
  (`App::render`). That is core Wayland (`wl_surface.frame`), not a
  Hyprland setting; on Windows the counterpart is a waitable swapchain.

## What broke, and what it taught

- **The kernel timestamp is the start of the frame.** uvcvideo stamps a
  buffer when the frame's first USB packet arrives (`TSTAMP_SRC_SOE`,
  `uvc_video.c`), and a YUYV 1080p60 frame takes about one frame interval to
  arrive: kernel → dequeue median 16.0, p99 20.1 ms. The user moved the
  criterion's start to dequeue, the earliest moment any player has the frame.
- **Fifo queues behind a fast source.** The card (60.000 Hz) outran DP-2
  (59.95 Hz); each early frame waited behind the last, latency crept
  ~0.87 ms/s up to ~4 frames (median 71.8 ms), and `get_current_texture`
  blocked 16.3 ms per frame. `desired_maximum_frame_latency = 1` did not
  bound it. The fix is pacing: hand a frame over only after the compositor's
  frame callback for the previous one, keeping only the newest frame
  meanwhile.
- **Pacing on "presented" halves the frame rate.** That feedback comes a
  refresh or more after the hand-over (1,034 of 1,800 frames shown in 30 s).
  The frame callback comes as the compositor takes the frame.
- **Nearest-pixel scaling looked jagged.** The first shader sampled YUYV
  straight onto the surface. Now: YUYV → RGB at 1920x1080 with chroma
  interpolated, then a bilinear scale; at 1:1 the text matches ffmpeg's
  conversion pixel for pixel.
- **MangoHud hooks every Vulkan app** here (`MANGOHUD=1` in the session
  environment), the PoC included. Measured with `MANGOHUD=0`.
- **No realtime audio threads.** cpal's promotion goes through rtkit, which
  is not installed; the two "errors" in every audio summary are that
  failure. The threads ran `SCHED_OTHER` and still had 0 underruns.
- **The card's start-up.** After stream-on the first two buffers come short
  with the error flag and one sequence number is skipped; logged as
  start-up, not counted as a gap.
- **The card re-enumerated** once mid-session (USB reset; capture node
  `video1` → `video0`). Finding it by USB id, never by node, carried it.

## Not verified

- The `[`, `]` and `q` keys by keypress: the window never had focus while
  the user was at the desktop. The audio sync path they call is the one
  `hdmipoc audio --sweep` exercised.
- Windowed latency over 10 minutes (15 s runs only).
- That cpal falls back loudly when the card audio input is missing: cpal
  sets `target.object` but not `node.dont-fallback`, so PipeWire may link
  the default source (the microphone) instead, as `pw-loopback` did. The
  runs checked the link with `pw-link`; the PoC itself does not.
- view.sh for a full 10 minutes (~90 s; the user quit it) and mpv's frame
  identity (the comparison uses the lower bound above).
- Offering the `linuxvideo` patch upstream
  (`poc/rust/vendor/linuxvideo-timestamp-sequence.patch`): not done.

## Open questions for Windows

- **Capture: Media Foundation.** Whether `IMFSourceReader` gives YUY2
  1080p60 from the MS2130 with per-sample timestamps and a way to detect
  dropped frames, and how to read with a timeout.
- **Audio: WASAPI.** Shared-mode buffer sizes on the card audio input and
  the default render device, and whether event-driven shared mode reaches
  10 ms without underruns.
- **Presentation timing.** DXGI's `GetFrameStatistics` / present stats in
  place of `wp_presentation`, and whether a flip-model swapchain with
  `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` gives the pacing the
  frame callback gives here.
