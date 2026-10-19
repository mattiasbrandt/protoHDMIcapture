---
status: accepted
---

# The Rust viewer draws the feed itself (wgpu), not through libmpv

Decided 2026-10-14 (#11, in the #10 grilling), after #5 and #7 measured both
inside the Rust window as equally fast: kernel timestamp -> presented median
40.5 ms with our own wgpu renderer, 42.5 ms with libmpv (swap interval 0),
against 69.0 ms for today's view.sh with mpv (`docs/research/rust-viewer-poc.md`).
With libmpv, mpv reads the capture card itself, so the viewer would give up
its own capture loop: the capture-gap count, "no signal" after 1 s of no
frames (5 s and 2 s since `0217d89`, `src/capture/policy.rs`), and
a quit that never waits on a frame, the loop that removed the
no-frames quit hang. On Windows that capture would be FFmpeg's DirectShow
rather than the Media Foundation path #9 plans. We keep our own capture and
renderer, and write mpv's share ourselves.

## Considered options

- **libmpv inside the window**: mpv's scalers, colour handling and OSD for
  free, but mpv owns capture again, the render API is OpenGL-only (EGL; on
  Windows OpenGL or ANGLE), and libmpv plus FFmpeg ship with every package.
  It must hand frames over with swap interval 0: pacing on the frame callback
  makes it a frame slower (#7).
- **Both behind one interface**: rejected as twice the code and testing for
  a choice the measurements did not need.

## Consequences

- Picture features become our shaders: SCALING presets fast (bilinear),
  balanced (spline36 / Mitchell) and a higher-quality EWA Lanczos-class one,
  each with its GPU time measured; LEVELS; the colour keys; screenshots.
  `SCALING=config` (leave mpv.conf alone) has no meaning here and reads as
  balanced.
- The overlay and the wizard are drawn with egui in the same wgpu frame.
- MJPEG cards need our own decode step.
