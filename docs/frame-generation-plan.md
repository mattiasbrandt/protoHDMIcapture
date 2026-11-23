# Frame generation and upscaling: plan

Decided 2026-11-23 in a grilling on #63. The findings this rests on, with
sources, are in [`docs/research/frame-generation.md`](research/frame-generation.md).

## Goal

A 30 fps console game, GTA 6 on PS5 first, shown at 60 fps and optionally
upscaled, inside protoHDMIcapture. The first test bed is AMD on Linux with
the MS2130 card. The design serves every dedicated GPU wgpu drives, on Linux
and Windows, and any capture card the project supports.

Out of scope:
- Anything that needs the game's motion vectors or depth: DLSS, FSR 2/3/4,
  FSR frame generation, XeSS. A capture card never delivers them.
- Copying code whose licence does not allow it. Reading any of it to learn
  from is fine; a port keeps the source's notice.
- protoHDMIcapture legacy: neither feature is brought to view.sh.
- Recording for users. The dev recorder (#64) exists only in dev builds,
  and its clips are development files in `/var/tmp`. If recording ever
  becomes a user feature, its files go to the user's Videos folder
  (`XDG_VIDEOS_DIR` on Linux, the Videos library on Windows), under
  `protoHDMIcapture/`.

## What ships (once a winner exists)

- **Frame generation** (GLOSSARY.md) is opt-in: `FRAMEGEN=off|2x`, off by
  default.
  - It is meant for dedicated gaming GPUs. On an integrated GPU it is
    refused at startup, with a message saying so.
  - It is a live change: `g`, with the Tab overlay open, switches it
    off ↔ 2x, the overlay row shows its value, and `w` saves it.
  - Only 2x: one generated frame between two new frames. A later 4x for
    high-refresh monitors is the reason the value names a multiplier.
  - A generated frame only ever replaces a **duplicate frame**, and only
    while the cadence is a steady 2:2 (a 30 fps source in a 60 Hz signal).
    A 60 fps source, a menu, a frame-rate dip, a scene cut and the first
    frames all show real frames, as today. It never adds more than one
    refresh of delay.
- **Upscaling:** one new `SCALING` preset named `sharp`, which `c` cycles
  to after high-quality. FSR 1 and NVIDIA Image Scaling are both tried;
  only the winner lands.

## The shape

```
card frame ─► duplicate-frame detection ─► schedule ─┬─► generated frame (new frame in a 2:2 cadence)
                                                     ├─► held real frame (the duplicate that follows)
                                                     └─► real frame (anything else)
                                                                  │
                                                                  ▼
                                                  scale (SCALING) ─► present
```

The card's own cadence is the clock: protoHDMIcapture still presents once
per card frame (`src/pacing.rs`); only what it presents changes.
Generation runs on the convert pass's RGB frames at the capture's size,
before scaling.

## Groundwork, on `main` first

| # | What | Notes |
|---|---|---|
| #64 | Dev recorder and replay | `cargo build --features dev` only: `--record <name> --seconds N`, headless, raw frames to `/var/tmp/protohdmicapture-clips/` (on disk; `/tmp` is RAM-backed on many desktops). Keepers are copied by hand to `~/.local/share/protohdmicapture/clips/`. `--replay <file>` plays a clip through the normal window. |
| #65 | Duplicate-frame detection | `mpdecimate`'s 8×8 luma block test on the capture thread; the source's frame rate, cadence and cadence breaks on the dashboard; the kind travels on the frame's tag. |
| #66 | The frame-generation seam | The pure schedule, the generator trait, the window keeping the previous new frame, the integrated-GPU refusal, a `copy` test generator and a new-frames-only mode for external layers. Driven by a dev flag. |

The `FRAMEGEN` setting, its key, overlay row and README entry land
together with the winning approach, so an automatic release never shows a
setting that does nothing yet.

## Round one: local branches, judged on the same clips

| # | Branch | Approach |
|---|---|---|
| #67 | `fg-blend` | Blend the two frames: the baseline, and a check that the seam works |
| #68 | `fg-ffx` | AMD FidelityFX Optical Flow (SDK v1.1.4, MIT) ported to WGSL, plus an interpolation pass: the main candidate |
| #69 | `fg-layer` | No code: openfg, afmf-linux and lsfg-vk over the new-frames-only mode, enabled by environment variable for protoHDMIcapture only |
| #70 | `up-fsr1` | FSR 1 as `SCALING=sharp` |
| #71 | `up-nis` | NVIDIA Image Scaling as `SCALING=sharp` |

Branches stay local.

**Recordings.** The first clips are headless: the user sets up GTA 6 with
the character standing still, and the agent records. Each recording
reports its share of new frames and its brightness per second, so a clip
that went idle or dark is flagged. The fallback is the user playing while
the agent records.

**Judging.** The user judges by eye; there is no objective score, since
GTA 6 gives no 60 fps ground truth. The agent replays the same clips
through each branch and tabulates:
- GPU ms per generated frame (`--stats`);
- generated and missed frames;
- screenshots of the same moments (HUD, a fast pan, a scene cut).

Then one card session: the user watches each branch live and picks.
Latency is measured in the same session. `fg-ffx` passes if real frames'
kernel → presented median stays within 44.9 ms (`docs/testing.md`) +
16.7 ms + its GPU time.

**The losers.** Their issues close as not planned, with the scores kept
on them.

## Later, only if round one falls short

- **Vendor fast paths:**
  - AMD AMF FRC on Windows (DX11/DX12);
  - NVIDIA optical flow through the driver (`VK_NV_optical_flow`), not
    the SDK, which cannot ship with an open-source app;
  - RTX Video Super Resolution.
- **ML interpolation (RIFE via ncnn):** first a timing test on the AMD
  GPU.
- **Extrapolation:** it avoids the wait, but it is research only.

## Capture cards

The card decision waits until after round one. The MS2130 delivers
uncompressed YUYV 1080p60 over USB 3, the right input for 30 → 60. A
better card buys lower card latency (unmeasured) and headroom for 1440p,
120 Hz or HDR. A 10 Gbit/s or HDR (P010) card also needs project work
first: capture-card profiles take a `Link:` of 480 or 5000 only, and `FMT`
has no P010.
