# Frame generation and upscaling: plan

Proposal, 2026-11-23, tracked in #63. Not grilled yet: the decisions at the
end are open, and no slice starts before they are settled. The findings this
rests on, with sources, are in
[`docs/research/frame-generation.md`](research/frame-generation.md).

## Goal

A 30 fps console game, GTA 6 on PS5 first, shown at 60 fps and optionally
upscaled, inside protoHDMIcapture. The first test bed is AMD on Linux with
the MS2130 card. The design serves every GPU wgpu drives, on Linux and
Windows, and any capture card the project supports.

Out of scope:
- Anything that needs the game's motion vectors or depth: DLSS, FSR 2/3/4,
  FSR frame generation, XeSS. A capture card never delivers them.
- Copying code whose licence does not allow it into an MIT OR Apache-2.0
  project. Reading any of it to learn from is fine; a port keeps the
  source's notice.

## The shape

```
card frame ──► new-frame detection ──► (new)    ──► optical flow ──► generated frame ─┐
                    │                                                                 ├─► scale (SCALING, FSR 1) ─► present
                    └─────────────────► (repeat) ──► the held real frame ─────────────┘
```

Generated frames are presented when a new frame arrives. The real frame
follows when its repeat arrives, one refresh later. That makes the card's
own cadence the clock, the way protoHDMIcapture presents today
(`src/pacing.rs`). When the cadence breaks (a third repeat, a skipped
frame) or a scene cut is detected, the real frame shows as today, and
nothing is generated across the gap.

## Steps

Each step lands on its own and is useful without the next.

### 1. New-frame detection

Tell a new source frame from a repeat by content. Compare each card frame
with the previous one as a block-difference score over a downsampled luma
(the method of FFmpeg's `mpdecimate`). The threshold sits above MJPEG and
noise jitter and below real motion.

- The dashboard shows the source's own frame rate next to the card's
  ("30.0 fps in a 60 Hz signal"), plus a count of cadence breaks.
- A presenting mode that shows only new frames. Step 3 needs it, and a
  user who runs an external layer gets it too.
- **Done when:** unit tests pass on synthetic sequences (2:2, a break to
  2:3, a still scene, noise below the threshold), and one card run of GTA 6
  on PS5 records its real cadence and break rate on the dashboard. The
  card run needs a go.

### 2. Spatial upscaling

FSR 1 (EASU + RCAS) or NVIDIA Image Scaling, ported to WGSL as a fourth
`SCALING` preset, next to the libplacebo-derived filters in `src/scale.rs`.
Both are MIT.

- A 1:1 feed stays the frame itself, as it does for the other presets.
- **Done when:** the preset is selectable in the settings, the wizard and
  the `c` key; a GPU test in `src/scale.rs`'s existing style checks its
  output on a test pattern; and `--stats` GPU time is recorded at
  1080p → 1440p and 1080p → 4K on the AMD GPU.

### 3. Quality experiment with external layers

Before porting anything, find out whether optical-flow interpolation looks
good enough on GTA 6. Run protoHDMIcapture in step 1's new-frames-only
mode under openfg or afmf-linux (both MIT, both built on FidelityFX Optical
Flow). If the user owns Lossless Scaling, run it under lsfg-vk as well.
Nothing is bundled.

- Judge the HUD, fast camera pans and scene cuts by eye on the PS5, and
  read latency from `--stats`. A layer's extra presents may confuse the
  Wayland feedback; if they do, say so.
- **Done when:** a written verdict on #63 says go, no-go, or go with a
  named mitigation (a HUD mask, a lower flow resolution). Card run, needs a
  go.

### 4. Frame generation in protoHDMIcapture

Port FidelityFX Optical Flow (from SDK v1.1.x, MIT) and an interpolation
pass to WGSL compute. afmf-linux's and openfg's portability edits are the
reference. The feature goes behind an off-by-default setting.

- It uses wgpu's `Features::SUBGROUP` where the adapter has it. Without it,
  the setting reports "not available on this GPU" at startup, not mid-run.
- Pacing as in "The shape" above; scene cuts and cadence breaks fall back
  to the real frame.
- **Done when:**
  - pure tests on the pacing schedule pass, including the break and cut
    fallbacks;
  - a GPU test interpolates a moving synthetic pattern within a set error;
  - one card run shows real frames' kernel → presented median within the
    44.9 ms baseline (`docs/testing.md`) + 16.7 ms + the measured compute
    time, with the generated frames' share counted.

  The card run needs a go.

### 5. Vendor fast paths (only if step 4 falls short)

Each is gated by a measurement showing step 4 too slow or too poor on that
vendor:
- AMD AMF FRC on Windows (DX11/DX12 interop; MIT headers, runtime in the
  driver);
- NVIDIA optical flow (`VK_NV_optical_flow`) through the driver, not the
  SDK, which cannot ship with an open-source app;
- RTX Video Super Resolution for upscaling on Windows.

### Not planned

- ML interpolation (RIFE via ncnn on Vulkan): only if step 3 or 4 shows
  optical flow's quality falls short. First a timing test on the AMD GPU at
  540p.
- Extrapolation, which avoids the wait but ghosts: research only.

## Capture cards

No purchase is needed for steps 1–4. The MS2130 delivers uncompressed YUYV
1080p60 over USB 3, which is the right input for 30 → 60, and it already
works on this project's Linux path. A better card buys:

- **Lower card latency.** Frame generation adds to the chain, so the card's
  own internal delay matters more. The MS2130's is reported at 6 to 11
  frames, unmeasured.
- **Headroom:** 1440p, 120 Hz or HDR capture.

The research note's table lists candidate classes. Before buying, check
that the store takes returns and that `v4l2-ctl --list-formats-ext` on the
card lists uncompressed modes. A 10 Gbit/s or HDR (P010) card also needs
project work first: capture-card profiles take a `Link:` of 480 or 5000
only, and `FMT` has no P010.

## Open decisions

- Is the added refresh plus compute acceptable as an opt-in setting? Is it
  a live change on a key?
- FSR 1, NIS, or both, as presets?
- Names: the setting, and glossary terms for a new source frame, a repeat,
  and a generated frame. "Repeated refresh" is already taken for the
  monitor side.
- Legacy: what a new `SCALING` value means for view.sh's mpv
  ([ADR 0002](adr/0002-legacy-kept-best-effort.md): best effort; it may
  read as balanced).
- Buy a card now, or after step 3?
