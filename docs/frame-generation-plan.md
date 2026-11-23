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

- **Frame generation** (GLOSSARY.md) is opt-in, off by default.
  **ffx-quality and rife ship** (the user, 2026-11-24, after the live
  session): neither is perfect (buildings and signs glitch), both are good
  enough as an opt-in, and their stutter bursts are worked on first. It is
  never turned on by itself, not by the wizard either. The setting's form
  (`FRAMEGEN=off|ffx-quality|rife` or similar) is decided with #84.
  - It is meant for dedicated gaming GPUs. On an integrated GPU it is
    refused at startup, with a message saying so.
  - It is a live change: `g`, with the Tab overlay open, switches it
    (off ↔ on, or through the shipped methods), the overlay row shows its
    value, and `w` saves it.
  - Only 2x: one generated frame between two new frames. A later 4x for
    high-refresh monitors is the reason the value names a multiplier.
  - A generated frame only ever replaces a **duplicate frame**, and only
    while the cadence is a steady 2:2 (a 30 fps source in a 60 Hz signal).
    A 60 fps source, a menu, a frame-rate dip, a scene cut and the first
    frames all show real frames, as today. It never adds more than one
    refresh of delay.
- **Upscaling** (the user, 2026-11-24): its own setting,
  `UPSCALING=off|fsr|nis` (FSR 1 and NVIDIA Image Scaling; the values follow
  the user's verdict between them), the name users know from games. It
  upscales and sharpens when the picture is enlarged; `SCALING`
  (balanced, fast, high-quality) stays the ordinary scaler, unrenamed. The
  wizard turns Upscaling on when it finds a gaming GPU and a monitor that
  enlarges the picture; its own overlay row and key (#85).
- **The wizard and the hardware** (#84, #85): Upscaling is turned on with
  a gaming GPU on an enlarging monitor. Frame generation is only suggested:
  the wizard measures the shipped generators on the user's GPU, headless and
  paced, and says which fit with room and what they cost in delay; the user
  decides, and it stays off by default. An integrated GPU, a GPU without
  the room, or a 60 fps source get the reason instead.
- **The README describes the feature** (#84): what it does and does not do,
  its delay, where it looks worse, Upscaling, GameMode's GPU opt-in, and
  what could come later (the "Later" list below and round two's findings).

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
| #64 | Dev recorder and replay | `cargo build --features dev` only: `--record <name> --seconds N`, in the normal window since #79 (the user plays while it records), raw frames to `/var/tmp/protohdmicapture-clips/` (on disk; `/tmp` is RAM-backed on many desktops). Keepers are copied by hand to `~/.local/share/protohdmicapture/clips/`. `--replay <file>` plays a clip through the normal window. |
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
| #70 | `up-fsr1` | FSR 1 as `SCALING=sharp` (round one; now `UPSCALING=fsr`, #85) |
| #71 | `up-nis` | NVIDIA Image Scaling as `SCALING=sharp` (round one; now `UPSCALING=nis`, #85) |

Branches stay local.

**Recordings.** The first clip was recorded headless, the character
standing still. Since #79 a recording keeps the normal window, so the user
plays while the agent records (there is no loop-out screen at the test
machine). Each recording reports its share of new frames and its
brightness per second, so a clip that went idle or dark is flagged.

**Judging, in three stages** (decided 2026-11-23). The user judges by eye;
there is no objective score, since GTA 6 gives no 60 fps ground truth.
1. **The kept clip (first check, the agent's).** Each branch replays
   `gta6-standing-03` (`--replay`): GPU ms per generated frame or upscale
   (`--stats`), generated, missed and held counts, and screenshots of the
   same moments. A branch that fails here goes no further.
2. **A live card run per approach (the agent's, the user lines the game
   up).** The same numbers on the live PS5 signal, plus real latency:
   `fg-ffx` passes if real frames' kernel → presented median stays within
   44.9 ms (`docs/testing.md`) + 16.7 ms + its GPU time.
3. **The user's hands-on session, last.** The user plays with each
   approach live and picks the winners.

**The losers.** Their issues close as not planned, with the scores kept
on them. More than one frame generator may win (the user, 2026-11-23).

## Round two: picture quality on gaming GPUs

Decided 2026-11-23, after stage 1 showed every round-one approach using
under 1 ms of a 16.7 ms frame. The user's aim: use the gaming GPU's
headroom for the picture.

| # | Branch | Approach |
|---|---|---|
| #72 | on main | Measurement: the GPU section of the dashboard and `--stats` (busy, clocks, VRAM, power, temperatures, fan, protoHDMIcapture's own engine time), shown only while an enhancement is on |
| #73 | `ffx-quality` (on `fg-ffx`) | Full-resolution flow, a wider search, two-way flow with occlusion-aware blending, a static-HUD mask (≤ 5 ms) |
| #74 | `fg-rife` | RIFE v4.6, a learned interpolator, in WGSL compute (≤ 10 ms) |
| #75 | `up-ultra` | A learned 2× upscaler in WGSL compute as `SCALING=ultra`, judged with `sharp` |
| #77 | on main | GameMode while an enhancement is on |

**Paced and back-to-back GPU time differ.** With amdgpu's power level on
`auto`, the driver clocks the GPU only as high as the paced 60 Hz load
needs. The ML upscaler takes 7.2 ms back to back and 12.4 ms paced
(#75). Budgets are judged paced, as the window runs. #77 asks GameMode
for performance while an enhancement is on; the GPU part of that stays
the user's opt-in in `/etc/gamemode.ini` (gamemoded ignores `[gpu]` in
`~/.config/gamemode.ini`).

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
