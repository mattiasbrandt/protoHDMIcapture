# SmoovPlay: what to borrow, what to avoid

Research note, 2026-11-25. Question: what in SmoovPlay
(github.com/smoovplay/smoovplay, MIT, Windows-first, PS5 Remote Play through
a patched chiaki-ng, plus Experimental capture cards) is worth taking for
protoHDMIcapture, and what it means for our 45 ms, our frame generation (#63,
#86), our upscaling, a second source, and our marketing.

Tags used below:
- **[src]** read in source. Their paths are relative to
  github.com/smoovplay/smoovplay at commit 8fe7da1; ours to this repo at
  870bca7.
- **[web]** fetched from their site or GitHub this session. WebFetch returns a
  model's summary, so quoted wording is not verified verbatim.
- **[inferred]** my reasoning from the sources. Not measured.
- **[unverified]** not checked here.

Nothing was built or run. No hardware was touched.


## Decided (the user, 2026-11-25)

- **No second source.** protoHDMIcapture stays a capture card viewer; Remote
  Play and window capture are out of scope (Q4 below is the record of why
  it was weighed).
- **Delay basis.** The README quotes dequeue to presented, the basis software
  viewers use (SmoovPlay, Moonlight's render time, NVIDIA's PC latency),
  from the next `--stats` run. The 45 ms (kernel to presented) stays in
  `docs/how-it-works.md` with both endpoints named. No competitor is named.
- **Live test mode** (idea 1): a dev build first, with the copy and blend
  baselines beside the generator; Experimental for players later if the
  scores hold. #92.
- **Uneven-rate games** (idea 6, 40-60 fps and 30 fps dips): their own
  ticket, #93, unscheduled.
- **FSR 1:** unchanged (AMD's default 0.25 stops, no denoise).
- **Name and tagline:** the name stays. Tagline: "A lightweight, open-source
  capture card viewer. Low delay, nothing else in the way." Subline:
  "Upscaling and frame generation when you want them."
- **Visuals:** our own SVGs only (a logo with a light and a dark version, a
  30 vs 60 fps diagram, card to PC to screen). No game footage, no
  comparison page.
- **README:** a Works-with table (Tested or Untested, by whom) and a "Help
  test it" section.
- **Already ours, nothing to borrow:** the behind rule (`src/framegen.rs`
  schedule), copy and blend as score modes, audio drift (`src/audio.rs`
  plays faster or slower to its target fill), auto-detect without a timing
  run (`docs/frame-generation-plan.md`). Signed updates do not apply: there
  is no updater.

## Summary

- Our 45 ms and their 36 ms (90 fps) / 20 ms (60 fps) are **not comparable as
  stated**. They start at different points and end at different points. Ours
  ends at the compositor's presented time. Theirs ends when their swap or
  Present call returns, with vsync off by default. Details in Q3.
- Their most useful code for us is not the delay story. It is: a **live test
  mode** that scores generated frames against real frames left out of a
  60 fps game; a **"behind" rule** that drops the generated frame and keeps the
  real one on time; the **baseline columns** (repeat, 50/50 blend) in their
  score; and their **side-by-side lag script** as a repeatable way to measure
  delay.
- FSR 1 is the same AMD algorithm in both trees. Ours has no RCAS denoise and a
  sharper default (0.25 stops vs their 0.5). Their RTX VSR and RTX SDK wrapper are NVIDIA-licensed
  and cannot ship in an MIT/Apache tree.
- Remote Play as a second source is feasible only in three ways, and two of them
  carry AGPL obligations. Recommendation in Q4: start with window capture of a
  streaming app, not with a chiaki-ng fork.

### Ranked ideas

| # | Idea | Value to us | Effort | Licence note | Theirs | Ours |
|---|---|---|---|---|---|---|
| 1 | Live test mode: drop every other frame of a 60 fps game, score each generated frame against the real one that was dropped | High: judges #86 and #63 live, no clip needed | Medium | Idea only; MIT code could be ported with its notice | `framegen/sources.py:72-140`, `framegen/pipeline.py:892-930` | `src/score.rs` is headless and dev-only (`score.rs:1-40`); the live path does not exist |
| 2 | "Behind" rule: when the pipeline is late, skip the generated frame, show the real one on time | High: directly the #86 problem (3-7 ms generator misses the deadline) | Low | Idea only | `pipeline.py` `behind_ms` (0.6 tick, ~line 396) and `extra_ok` (~line 400) | Not re-read in this pass; check `src/framegen.rs` schedule |
| 3 | Baseline columns in the score: repeat and 50/50 blend next to the generator | High: says whether the generator beats doing nothing | Low | Idea only | `pipeline.py:892-930` (`psnr_repeat`, `psnr_blend`) | `src/score.rs` (line 780 mentions the branches' copy/blend; not checked whether printed) |
| 4 | Side-by-side lag tool: lag between the card's own output and our window, from 240 fps video | High: a repeatable delay number | Low-medium | MIT, idea and algorithm | `scripts/latency_video.py` (`side` mode, lines 1-40) | We film by hand (`docs/testing.md`, "Measuring latency") |
| 5 | Adaptive hold: the delay follows the recent frames' arrival need | Medium, for uneven sources only | Medium | Idea only | `framegen/cadence.py:362-420` (`FrameTimeline`) | not needed for a fixed 60 Hz card |
| 6 | Even out 40-60 fps and dip smoothing (timed frames, shown evenly) | Medium: outside the 30→60 scope in `docs/frame-generation-plan.md` | Medium-high | Idea only | `cadence.py:283-295` (constants), `cadence.py:324-502` | not in our plan |
| 7 | FSR 1 RCAS denoise and a stop default of 0.5 | Low-medium: our capture is uncompressed | Low; needs a visual check | MIT, FidelityFX (notice exists) | `framegen/fsr.py:67-71`, `settings.py:186-190` | `src/scale.rs:470` (0.25 stops), no denoise (grep for "denoise" in `scale.rs` found nothing) |
| 8 | Audio delay line that resamples by ±0.5% to absorb clock drift | Medium for audio drift; does not fix video drift | Medium | Idea only | `framegen/audio.py:1-12`, `DelayLine` at line 56 | `src/audio.rs` not checked here |
| 9 | Signed updates: RSA-3072 PKCS#1 v1.5 over version and SHA-256, verified by the launcher | Medium-high once releases matter | Medium | None | `installer/launcher/UpdateSignature.cs:1-25`, `installer/tools/SignUpdate.cs` | Releases are release-please; no signing seen (not checked in detail) |
| 10 | "Help test it" section with specific facts asked for (GPU, network, monitor Hz) and a copy-diagnostics button | Medium: better bug reports | Low | Text only | `README.md:223-235` | our README "Troubleshooting" (line 170) |
| 11 | Auto-detect that times the model on the user's GPU and picks settings | Medium, but our plan says no timing run (`frame-generation-plan.md`) | Medium | Idea only | `framegen/autodetect.py:1-15` | `docs/frame-generation-plan.md` (decided against) |
| 12 | Changelog style: plain user-facing lines, grouped by area, issue links, thanks | Low: release-please writes ours | Low | Text only | `CHANGELOG.md:1-40` | release-please generated |
| 13 | Looks (14 presets, shareable short code) | Low: picture grading is outside the viewer's scope | High | MIT | `framegen/looks.py` | out of scope |
| 14 | Instant replay | Low for us: our recorder is dev-only by decision | High | MIT | `framegen/replay.py` | out of scope per `frame-generation-plan.md` (user recording) |

Not borrowed: NVDEC zero-copy, CUDA-GL interop, DXGI flip-model details
(`framegen/dxgi_present.py`), NVIDIA VFG, RTX VSR, the chiaki tap. Reasons in
Q3 and Q4.

---

## Q1. FSR 1, RTX VSR, and where upscaling sits

### FSR 1 (theirs)

- **[src]** `framegen/fsr.py:1-12`: "Two OpenGL fragment-shader passes where
  the frame is drawn to the screen: EASU ... and RCAS". It runs at draw time, so
  real and generated frames get the same treatment. The shader is AMD's
  `framegen/shaders/fsr1/ffx_fsr1.h` (MIT).
- **[src]** `fsr.py:71`: RCAS is built with `FSR_RCAS_DENOISE 1` by default.
- **[src]** `fsr.py:67`: default sharpness 0.5 stops. `settings.py:186-190`
  labels 0.5 "Medium ... The default". `ffx_fsr1.h:662-672`:
  `sharpness = exp2(-stops)`, so 0.5 stops is 0.707 of maximum.
- **[src]** cost: their `DEVELOPING.md` (Upscaling section, around line 235):
  "about 0.1 ms of GPU time per frame at 2560×1440". README line 59 says the same.

### FSR 1 (ours)

- **[src]** `src/scale.rs:457-470`: `UPSCALING=fsr` runs EASU into an
  intermediate texture, then RCAS into the viewport. Only when the output is
  bigger than the input on both axes and not smaller on either (`scale.rs:461-467`).
- **[src]** `scale.rs:470`: `FSR_SHARPNESS_STOPS = 0.25`, the FidelityFX sample
  default. So our RCAS factor is exp2(-0.25) = 0.84, against their 0.71 by
  default. Ours is sharper (fewer stops below maximum).
- **[src]** no denoise. A grep for "denoise" in `src/scale.rs` found nothing.
- **[src]** our FSR is a WGSL port with the MIT notice (`THIRD-PARTY-NOTICES.md`,
  "AMD FidelityFX Super Resolution 1 (FSR 1)").
- **[docs]** our measured cost: `docs/research/frame-generation.md:62`,
  "0.17–0.25 ms at 1440p, measured (branch)". Theirs is 0.1 ms at 1440p on a
  4070 Ti SUPER. Different GPUs, so not a ranking.

### Ordering with framegen

- **Theirs** [src]: RIFE on the 1080p stream, then FSR at draw time, then the
  look (`framegen/looks_gl.py` header: after upscaling). Generated frames are
  upscaled too.
- **Ours** [src, plan]: generation on the capture-size frame, then the scale
  pass (`docs/frame-generation-plan.md`, "The shape"). Same order.

Nothing to change here. Both generate first and upscale second.

### RTX VSR (theirs)

- **[src]** `framegen/rtxvsr.py:1-16`: two paths. `VfxVsr` uses NVIDIA's
  `nvidia-vfx` package, the one NVIDIA VFG comes in, which the user downloads
  after accepting NVIDIA's licence. `RtxVsr` uses the RTX Video SDK through a
  ctypes wrapper.
- **[src]** `native/rtxvsr/rtxvsr.cpp:1-25`: the wrapper includes NVIDIA's
  `nvsdk_ngx.h`. Its own comment says it "links a library from the SDK, so it
  can't ship in the MIT app".
- **[src]** `rtxvsr.py:1-10` and `vfg.py:1-12`: NVIDIA's licence "forbids
  publishing benchmark results", so timings "stay on the user's PC".
- **[src] observation**: their `DEVELOPING.md` (Upscaling section) prints
  "1.3–4.9 ms per frame at 1440p" for the RTX SDK. That sits uneasily with the
  licence note in `rtxvsr.py`. I did not read the SDK licence text
  **[unverified]**, so I do not conclude it is a breach.
- **[src]** their VSR runs on every drawn frame and "can cost some of the extra
  in-between frames" at 90 and 120 Hz (`DEVELOPING.md`, Upscaling).

### What is better on their side

Nothing material in the upscaling itself. Their RCAS denoise matters for
compressed input (Remote Play). Our capture is uncompressed YUYV, so it matters
less for us. Their VSR is out of reach for us: NVIDIA's licence and an
NVIDIA-only path. Our NIS port (`src/nis.rs`, MIT, from NVIDIA Image Scaling)
already covers a vendor-neutral alternative.

Concrete step (idea 7): try RCAS denoise and a 0.5-stop default on the same
clip, judged by eye. Low effort, MIT notice already present.

---

## Q2. Frame generation

### Model, scale, multi-frame

- **Model** [src]: RIFE 4.22 lite (`framegen/interpolator.py:34-39`, `MODELS`,
  default `"4.22.lite"` at line 62). Only that model is offered
  (`framegen/settings.py:119-128`). Weights from HolyWu/vs-rife (MIT).
- **Ours** [src]: RIFE v4.6, embedded (`src/framegen/rife.rs:1-12`, weights
  10.6 MB fp16), in wgpu compute, not TensorRT.
- **Flow scale**: theirs defaults to full resolution [inferred]. `DEVELOPING.md:319`:
  "half-resolution flow (`--scale 0.5`) saves about 3 ms but costs 1.5–2 dB".
  Ours runs at 0.25 (`src/framegen/rife.rs:124-137`). "0.25 costs 0.94 dB on
  #81's skip-frame score (25.01 against 25.95 dB) and 25 % more tear blocks."
  Different tests, so the dB figures are not comparable, but both show that
  flow resolution costs quality.
- **Timing, ours** [src]: paced at 60 Hz on an RX 9070 XT, 1080p, fp16:
  5.87 ms at scale 0.25 with GameMode, 7.33 ms without (`rife.rs:130-133`).
- **Timing, theirs** [src, web]: "about 9 ms" per 1080p in-between frame on a
  4070 Ti SUPER, RIFE 4.22 lite (`README.md:56`). Their tick measurement from a
  recorded clip, interpolation step only: 12.2 ms p50 (`DEVELOPING.md`, table
  under "Results"). Different GPUs and scales, so no ranking.
- **Multi-frame**: theirs uses a time argument [src]. `interpolator.py:246-262`,
  `interpolate(newest_slot, t)`. The 90 Hz path uses t = 1/3 and 2/3
  (`pipeline.py`, `show_early(1 / 3)` and `mid_t = 2 / 3`); the 120 Hz path uses
  0.25, 0.5 and 0.75. Ours has `TIMESTEP = 0.5` as a constant (`rife.rs`, around
  line 122). Adding ×3 would be a small change, but our product is ×2 only by
  decision (`docs/frame-generation-plan.md`).

### Repeat detection

- **Theirs** [src] `framegen/cadence.py:82-180` (`DuplicateDetector`): 16×16
  block means (`avg_pool2d`), the largest block difference against 2.0. Then,
  for Remote Play, a **learned two-band rule** over the last 16 frames
  (`_learned`, line 124), and a **rhythm lock** for the 30 fps "N r N r"
  pattern (`_keep_rhythm`, line 137).
- Their reported accuracy, Remote Play over Ethernet: "99.5-100%" new frames
  and "93-99%" repeats right; 83% repeats over Wi-Fi (`DEVELOPING.md`, "The
  scheduler" section). Over Remote Play the repeats are not exact copies, which
  is why they learn the threshold.
- **Ours** [src] `src/duplicate.rs:5-34`: mpdecimate's 8×8 luma test, sum of
  absolute differences per block against the last new frame, with thresholds
  from mpdecimate's defaults.
- **Assessment** [inferred]: a capture card's repeats are exact or near exact,
  so our fixed threshold is adequate. The learned rule is for compressed streams
  and is not worth porting.

### Scene cuts

- **Theirs** [src] `cadence.py:230-262`: two tests. A jump in mean difference
  (threshold 25, with the previous-frame difference subtracted so only sudden
  jumps count), and a **structure** test, `corr_dist` jump above 0.75 with
  mean difference over 6. The second catches cuts between dark shots that the
  first misses (the comment at line 230-236 says so).
- **Ours**: not re-read in this pass [unverified]. `docs/frame-generation-plan.md`
  says a cut shows the real frame.

### Scheduling, even-out, dips

- **Core** [src] `cadence.py:1-30`: at 60 Hz, a 30 fps game arrives as
  `A A B B C C`; output is `A A mid(A,B) B mid(B,C) C`, one tick late. Ours does
  the same 2:2 cadence (plan). Same design.
- **Even out 40-60 fps** [src] `cadence.py:283-285` (`UNEVEN_ON = (39, 56)`,
  `UNEVEN_OFF = (35, 58)`, dwell 60 ticks) and `FrameTimeline` (line 362).
  Frames are timed from the recent average spacing, and the delay adapts
  (`_want_delay`, line 418). Costs about one tick of delay (README:215-216).
  [inferred] a capture card at 60 Hz shows a 45 fps game with the same uneven
  repeats, so the timing idea applies to us too. It is a scope question, not a
  bug.
- **Dip smoothing** [src] `cadence.py:291-295` and `DipWatch` (line 324):
  for 30 fps games that dip below 30, timed frames as they arrived, up to three
  ticks of delay (`DIP_MAX_DELAY = 3.0`). Off by default and marked Experimental
  (`settings.py`, `even_dips`). Their own numbers: "17 to 50 ms more delay"
  (`settings.py`, impact text) and "median delay went from 37 to 66 ms" in a
  fake-PS5 test (`DEVELOPING.md`, "Features in detail", Dip smoothing, around line 415-422).

### Game frame rate

- **Theirs** [src] `framegen/pipeline.py:73-90` (`GameRate`): new frames over the
  last 120 ticks, divided by ticks per second. `RATE_NOTE_S`, `RATE_TOAST_S` at
  line 112. `rate_advice` (line 117) tells the user to switch Simulate off or
  pick "30 fps" in Game frame rate.
- **Ours**: the dashboard reports the source's frame rate from duplicate
  detection (`docs/how-it-works.md`, "The debug log and the dashboard"). Same
  idea, fewer automatic actions.

### Test mode and quality score

- **Theirs, live** [src]: "Simulate a 30 fps game" (`framegen/sources.py:72-140`,
  `TestMode`) drops every other frame of a 60 fps source, keeps the dropped
  frames as ground truth, and scores each generated frame during play
  (`pipeline.py:892-930`, `_score`).
- The score: PSNR of the generated frame, the repeat and the blend, all against
  the real frame. Also PSNR over still pixels (`STILL_LEVELS = 3`,
  `pipeline.py:20`), the share of "visibly off" pixels (`VISIBLE_LEVELS`,
  `pipeline.py:29`), and PSNR inside a HUD mask (`hud_mask`, line 45:
  unchanged across frames, on a brightness edge with `HUD_EDGE = 128`). Only
  "clean 2-tick steps" are scored (line 898).
- Report in plain words, "can this PC keep up" and "how close are the AI frames",
  rewritten every 10 s (`pipeline.py:278-300`, `results()`).
- **Ours**: `src/score.rs:1-40` is headless, on recorded clips, dev builds only:
  PSNR and tear blocks (16×16 blocks over a threshold), plus a rectangle for a HUD.
  It has the three-frame skip test with the middle frame as truth, which is the
  same ground-truth idea as theirs, but offline.
- **Assessment** [inferred]: the live test (their approach) is what we lack.
  For a capture card, a 60 fps game can be decimated in the viewer exactly as
  they do, so it needs no clip. The repeat and blend baselines (idea 3) cost
  almost nothing and make a score interpretable.

### Stutter and presentation pacing

- **Their pacer** [src] `pipeline.py:637-646` (`present`): only for 90 and 120
  Hz. Each present is due `0.95/hz` after the last due time; a late present goes
  at once and the schedule restarts. At 60 Hz there is no pacer: each tick is
  shown when it is ready.
- **"Behind" rule** [src] `pipeline.py` (`behind_ms = 1000 * TICK * 0.6`, around
  line 396; `behind` and `extra_ok` around lines 630-636): if a tick's frames
  started more than 60% of a tick late, the extra generated frame is skipped,
  and the 60 Hz frame goes out. The docstring says it: "a tick skips the extra
  frame(s) and shows what 60 Hz would". Documented in `DEVELOPING.md`: "When the
  graphics card falls behind for a moment, SmoovPlay drops to 60 fps rather than
  add delay."
- **Display phase** [src]: no display-phase logic anywhere in their tree.
  `framegen/sync.py` is a CUDA capture lock, nothing more (`sync.py:1-7`).
  `Clock60` (`framegen/sources.py:143-211`) is a software beat for variable
  sources (window capture, Moonlight), not for the monitor.
- **Consequence for #86** [inferred]: they have the same card-versus-monitor
  drift problem and do not solve it. Their repeats at 60 Hz on a 59.97 Hz screen
  are not measured in their docs. Our REFRESH=match (Hyprland) is ahead of theirs
  here. Their own docs say the user must pick Hz ("Screen: any", README:101).
- **Audio drift, for contrast** [src] `framegen/audio.py:1-12`: the sound plays
  "half a percent faster or slower" to absorb clock drift, without a delay
  creeping. Video drift at 0.05% (60.003 against 59.973 Hz) is far below that,
  so this does not help the video problem.

### Concrete, portable, for #86 and #63

1. The behind rule (idea 2). Ours likely already has a deadline; check that a late
   generator drops its in-between frame, not the real frame.
2. A live test mode (idea 1) to judge the generator on the user's live game.
3. Baselines in the score (idea 3).
4. Keep our 0.25 flow scale unless the baseline test shows its cost. Their 0.5 costs
   1.5–2 dB on their test; ours costs 0.94 dB at 0.25 on #81. Not the same test.

---

## Q3. Low delay

### What they measure

Two numbers, two meanings. Both are on `t_arrival` to `t_shown`
(`framegen/pipeline.py`, around lines 800-815):

- **Start** [src] `framegen/io.py:26-29`: `t_arrival` is the perf_counter when
  the frame reached this PC. For Remote Play it is when chiaki-ng got the frame,
  before decoding. `framegen/chiaki_tap.py:168-192`: the reader ages the tap's
  QPC arrival stamp into perf_counter time. `chiaki_tap/frametap.h:26`: slot
  field `qpc_arrival`.
- **End** [src] `pipeline.py` `t_shown = time.perf_counter()` is taken after
  `present(out)` returns, which returns after `sink.write`. `framegen/io.py:1279-
  1330` (`GLWindowSink.write`): the frame goes to a GL texture and is drawn; the
  swap is `swap_buffers` or `DxgiPresenter.present` (`io.py:893-910`). That is a
  **submit** time, not scanout. With vsync off (default, `settings.py:268`:
  `vsync` default `False`), the swap returns at once and the picture tears in.

What is included: decode, RIFE, the hold for the next tick, upload and draw,
the Present call.
What is excluded: the PS5's encode, the network before chiaki-ng receives the
frame, the display's scan-out (their README says "The console, the network and
your screen add their own delay", `README.md:194-196`).

### The numbers, with their sources

| Figure | Source | Measured how |
|---|---|---|
| 36 ms at 90 fps (median 35.4 and 35.8 ms in two live sessions) | `README.md:191-196`, `DEVELOPING.md:298-306` | live Remote Play, 30 fps PS5 game in test mode / native, Ethernet, RTX 4070 Ti SUPER, 1080p. Start: tap arrival. End: `t_shown` |
| 20 ms at 60 fps | `README.md:191`, "The 60 fps figure comes from a recorded clip" | a recorded clip replayed through `FileSource`, OpenGL window, paced at 60 Hz. Start: the file's tick time |
| 20.8 ms p50, real frame arrival to presented | `DEVELOPING.md:330` | same clip run, 1,202 ticks, 20 s, same definitions |
| 22.2 ms of the 36 ms | `DEVELOPING.md:298-299` | "the hold any 3× interpolator needs" |
| 22 ms hold, 14 ms rest at 90 fps | web, `smoovplay.github.io/how-it-works/` | web summary; not in the repo |
| 0.1 ms FSR, 9 ms RIFE, 11.1 ms per frame at 90 fps | `README.md:56-59`, `README.md:177` | a timing on one GPU |

The 20 ms number is a file replay, not a live stream. Their live 60 fps number
is absent from the repo. The 36 ms number is live, at 90 fps. So the README's
"20 ms at 60" is not directly comparable to its own live "36 ms at 90"
[src, inferred].

### Apples to apples with our 45 ms

Ours [src]:
- Start: the V4L2 kernel capture timestamp, the first USB packet of the frame
  (`src/capture.rs:113-116`; `src/present.rs:30`).
- End: Wayland `wp_presentation` "presented" feedback, which is when the frame
  reached the screen (`src/present/wayland.rs:109-132`; `src/stats.rs:284-339`).
- Our baseline: "kernel → presented median 44.9 ms, p99 53.7 ms, 2 repeated
  refreshes", MS2130 at 1080p60, DP-1 at 59.97 Hz, 2 min (`docs/testing.md`,
  "The baseline"; `src/stats.rs:22-23`).
- Included: the frame's USB transfer (an uncompressed 1080p YUYV frame is about
  4 MB [inferred from the format]; the transfer runs after the first packet), the
  upload, our draw, the compositor, and the wait for the refresh.
- Excluded: the card's own internal latency, which our docs put at 6 to 11 frames
  (`docs/how-it-works.md`, "Why it is fast"; `capture-cards/ms2130.md:21`).
- Frame generation adds one refresh, 16.7 ms (`src/stats.rs:24-26`,
  `FRAMEGEN_EXTRA_MS`).

**The honest answer:** a like-for-like number needs the same two endpoints.
Theirs ends at a submit with no vsync wait. Ours ends at scanout. Their 36 ms
at 90 fps with vsync off is therefore a lower bound on what their picture takes
to reach the screen [inferred]. We do not know by how much, since neither tree
measures scanout on their side. Our 45 ms cannot be compared with their 20 ms at
60 fps without the same endpoints.

To make the comparison honest without a new stopwatch: report ours at the same
two points we already have (dequeue to present is in `src/stats.rs`, and
present is the submit time in `src/present.rs:4-8`), and state that our 45 ms
includes the scan-out wait. Do not put a headline comparison in the README.

### Techniques, one by one

- **NVDEC zero-copy, CUDA-GL interop**: [src] `framegen/chiaki_tap.py:431-456`
  (`cuda_frame_to_bgr`), `framegen/glcuda.py:1-12`. Remote Play decodes H.264/HEVC,
  so zero-copy saves the CPU round trip. Ours captures uncompressed YUYV and does
  no decode. Not applicable, except the MS2109's MJPEG (CPU decode 2–5 ms,
  `docs/how-it-works.md`) where a GPU JPEG decode would be NVIDIA-only. **Not
  borrowed.**
- **DXGI flip model, max frame latency 1**: [src] `framegen/dxgi_present.py:
  108, 344-372` (`SetMaximumFrameLatency(1)`, `FLIP_DISCARD`). Ours already sets
  `desired_maximum_frame_latency = 1` and Mailbox where available (`src/app.rs:
  837-841`), and wgpu's DX12 flip-model swapchain on Windows (`src/present.rs:
  14-19`). **Already have it.**
- **Low delay in a window**: [src] their DXGI path exists because a windowed OpenGL
  present "goes through the Windows compositor as a copy", about 12 ms (`dxgi_present.py:
  1-6`, `settings.py:251-256`). Ours presents through the Wayland compositor, which is
  measured by feedback. Our window mode is `borderless`/`windowed` in `docs/how-it-works.md`
  table. **Relevant on Windows only**, and we are Linux-first.
- **Hold-back**: [src] theirs holds one tick by design (`cadence.py:1-30`); ours holds
  one refresh for generated frames (`FRAMEGEN_EXTRA_MS`). Same cost. Their "hold for
  the next refresh only within 5 ms" is ours (`src/pacing.rs:27-30`, `HOLD = 5 ms`).
  **Already have it.**
- **Present timing**: [src] they use no present feedback. Ours does (`wp_presentation`).
  Ours is ahead here.
- **MMCSS and GPU priority** (Windows): [src] `framegen/winutil.py:156-170`
  (`AvSetMmThreadCharacteristicsW`, "Games" task, high priority) and `:203-206`
  (`D3DKMTSetProcessSchedulingPriorityClass`). Linux analog: RTKit or
  `RLIMIT_RTTIME`, which we already use for audio (`src/main.rs:111`,
  `src/audio.rs:1385`). **Check whether our capture and render threads get
  realtime too** [unverified].
- **Delay test with a controller press**: [src] `framegen/delaytest.py:1-40`:
  presses Options via chiaki-ng, times the answer. Needs a controller path.
  **Not for a capture card**, which has no controller.
- **Side-by-side lag tool**: [src] `scripts/latency_video.py` (`side`, lines 1-40):
  two boxes, one for the PS5's own output and one for the viewer, filmed at 240 fps;
  reports how far behind the second box is. **Borrow the method** (idea 4). Our
  `docs/how-it-works.md` asks for the same film by hand.

### Their delay test as a feature for us

Not the PS5 press version, which needs a controller. The **side** mode is a
feature we could ship: a dev-only tool that takes two videos and reports the
lag. It turns our hand-filmed number into a repeatable one. Effort: a small
script; no licence issue (MIT, idea only).

---

## Q4. Remote Play as a second source (no capture card)

### How their tap works

- **Writer, in chiaki-ng** [src] `chiaki_tap/apply_patch.py:1-16`: the tap is
  copied into chiaki-ng's `gui/`, with six one-line calls in `streamsession.cpp`,
  a `friend` line in `streamsession.h`, and CMake entries. `chiaki_tap/frametap.c`
  is compiled into chiaki-ng's binary when built.
- **Shared memory** [src] `chiaki_tap/frametap.h:1-40`: a 4096-byte header with
  magic `CFTP`, codec, size, `write_seq`, `session`, `qpc_freq`, plus fields for
  the reader to ask for an IDR, press buttons, and delay the sound. Then slots at
  `4096 + i * slot_size`, each with `seq`, `qpc_arrival`, `size`, `frames_lost`,
  `flags`, then the compressed frame. A second ring carries 16-bit PCM for instant
  replay.
- **Reader, in Python** [src] `framegen/tapsys.py:1-6`: `/dev/shm/<name>` on Linux,
  `CLOCK_MONOTONIC`, and a **futex** on the low 32 bits of `write_seq`. Windows: a
  named file mapping plus an auto-reset event. `framegen/chiaki_tap.py:117-192`
  (`FrameTap.read`) copies a slot out and checks `seq` before and after to detect
  rewrites.
- **Layout is shared by hand** [src] `frametap.h` comments: "must match
  framegen/chiaki_tap.py". Changes need both sides.

### Licence facts

- **chiaki-ng: AGPL-3.0**, read through the GitHub API this session
  (`gh api repos/streetpea/chiaki-ng/license` → `AGPL-3.0`, `COPYING`). SmoovPlay
  says "with the OpenSSL exception" (`installer/THIRD_PARTY_NOTICES.txt`, "Bundled
  as a separate program"); I did not verify that clause [unverified].
- **Moonlight (moonlight-qt) and moonlight-common-c: GPL-3.0**, and
  **Sunshine: GPL-3.0**, read through the GitHub API this session.
- **SmoovPlay**: its own code MIT; the chiaki_tap patch is part of the AGPL
  chiaki-ng binary it ships (`NOTICE`; `THIRD_PARTY_NOTICES.txt`). They ship the
  patch's source (`chiaki\source\`), per their notice.
- **Ours**: `LICENSE-MIT` and `LICENSE-APACHE`, MIT OR Apache-2.0
  (`THIRD-PARTY-NOTICES.md:3`). There is no single `LICENSE` file in our tree.
  The frame-generation plan says it: "Copying code whose licence does not allow
  it. Reading any of it to learn from is fine; a port keeps the source's notice."

### The four options

1. **Unmodified chiaki-ng as a separate process, window-captured.** No code from
   chiaki, no licence change for us. The frame is read from the window, not the
   tap: lower quality (their own "App window" mode, Experimental, "Not yet confirmed
   with a real game stream", their `DEVELOPING.md` "Known limits"). Effort: low to
   medium, if we have a window-capture source (below). **Lowest licence risk.**
2. **Patched chiaki-ng (our tap) as a separate process, shared memory to us.** Our
   fork of chiaki-ng is then AGPL, and distributing it means publishing its source.
   Our viewer only reads shared memory; whether that keeps the viewer MIT/Apache is
   a legal question I cannot settle here [inferred: separate programs talking at
   arm's length, but the tap code itself sits inside chiaki-ng]. Effort: medium.
   **Needs a legal read before anyone builds it.**
3. **Link chiaki's library into our binary.** Makes the whole viewer AGPL. **Not
   acceptable** for an MIT OR Apache project without relicensing it.
4. **Write our own Remote Play client.** Pairing, PSN registration, Sony's
   encryption, and the stream protocol. Reading chiaki to learn the protocol and then
   writing a derivative is the risk the plan warns about, and the work is large.
   **Not recommended.**

### Other alternatives

- **Moonlight/Sunshine**: GPL-3.0. Moonlight is a client to a Sunshine host, on a
  PC or console side we do not control. Same licence issue as option 3 if linked;
  as a separate window it is option 1's pattern.
- **Window or PipeWire screen capture of any streaming app** (their "App window"
  source, which covers the Xbox app, PS Remote Play, Moonlight, browser cloud games):
  [src] `framegen/window.py` (Windows, DDA/ddagrab) and `README.md:74-81`
  (Experimental). On Linux this is the xdg-desktop-portal ScreenCast path through
  PipeWire. **[unverified]** whether our tree has any video path besides the V4L2 and
  Media Foundation capture adapters (`src/capture/v4l2.rs`, `src/capture/mf.rs`).
  This is the most likely fit for us, because it needs no patch and no protocol.
- **Xbox**: no separate path in our tree; the Xbox app is a window, so it is the
  window path.

### PSN registration, HDCP, and the rest

- PSN registration: their README asks users to sign in to PSN in chiaki-ng and
  register the console (`README.md:114-115`, FAQ at 249-252). That is a user step
  we would have to document and support, and it touches Sony's account system.
- HDCP: their FAQ says turn HDCP off on the PS5 for a black picture
  (`README.md:257`). Ours says the same for the capture card (`README.md:83`,
  `README.md:174`). Remote Play does not avoid HDCP on the console side in their
  notes.
- Sony's own terms for Remote Play are not checked here **[unverified]**.

### Fit with our source abstraction

- Our capture adapters yield `Raw` buffers with `captured` and `dequeued`
  timestamps (`src/capture.rs:105-125`). A window or chiaki source would also
  produce frames with a capture time. Fit is plausible [inferred], but `Raw` is
  shaped around USB card buffers (`data`, `mark`, `flagged`, the card's sequence
  gaps). A second adapter means a second notion of "gap" and "damaged".
- `src/sources.rs` is not a source abstraction. It holds the setup-tip profiles
  (`sources/<id>.md`), so no existing seam for a new video input.

### Recommendation

Option 1 first, as an experiment, behind a dev flag: window capture of a streaming
app through PipeWire, judged on the same clip rules as the card. It is the only one
with no licence change and no protocol work. Options 2 and 3 are not worth it until
a legal read says the viewer stays MIT/Apache. Option 4 is out. **Scope decision for
the user** (see Open decisions).

---

## Q5. Branding and marketing

### What their README does

- **Hero** [src] `README.md:1-11`: a logo lockup with a dark variant, then the line
  "Your console. Your GPU. Smoother gameplay." and one sentence of what it does.
- **Action row** [src] `README.md:13-21`: Download, Website, "Watch it at full frame
  rate", and anchors to What it does, Install, How it works, FAQ.
- **Badge row** [src] `README.md:23-29`: status ("early release"), a download counter
  from a stats JSON, Windows, GPU, licence. Shields.io badges, one colour family.
- **Comparison poster** [src] `README.md:31-36`: a still from the comparison page, with
  "See six clips at full frame rate" and the reason in one line: "Most social sites play
  video at 60 fps or less." Plus a credit line for the footage.
- **Diagram** [src] `README.md:38-40`: `docs/media/smoothness.svg`, two dots on a track,
  30 vs 60/90/120 per second. Readable without text.
- **Strips** [src] `README.md:51-53, 107-109, 118-120, 141-143`: four PNG strips of
  screenshots, one per section (overview, setup, play, custom). Each has a one-line
  `alt` text that says what it shows.
- **Works-with table with honest status** [src] `README.md:74-81`: "Tested" versus
  "Experimental" per row, with the tester named. A paragraph after it says how it is
  checked, and which results are simulated (`README.md:83-87`).
- **Get-it table and an SHA-256 check** [src] `README.md:95-137`: "Early release"
  note first, then the requirements table, then steps. A SmartScreen `<details>` block.
- **Help test it** [src] `README.md:223-242`: names the specific facts they need,
  and a test mode for 60 fps games, with a copy-results button.
- **FAQ as details blocks** [src] `README.md:244-315`: short questions that a
  newcomer asks (does it change my console; black picture; AMD support; how is it
  different from DLSS FG or Lossless Scaling).
- **Credits and trademarks** [src] `README.md:341-361`: each third-party licence named,
  and a trademark disclaimer.

### What their website does [web]

- Hero "30 fps Remote Play vs SmoovPlay", with a frame-rate selector (60/90) and
  split, side-by-side and flip modes. The page plays video at its real frame rate, which
  a social feed cannot.
- "Drive and chase", and "Every frame, slowed 3×" sections, and one live recording of a
  real game, labelled as live.
- The footage is labelled **simulated** (a published gameplay clip run through their
  pipeline as a stand-in stream) or **live** (their own PS5). That labelling is the
  honesty device.
- Keyboard hints: T to flip, F for fullscreen.

### What is theirs, not ours to reuse

- **Footage**: their comparison clips are Rockstar's GTA VI footage (`README.md:35`).
  Their Looks samples are screenshots of several games (`README.md:352-356`). Not
  reusable under MIT. We must not copy either.
- **Logo and brand SVGs** (`docs/brand/`): their own artwork, MIT only for the code. Not
  to be copied.
- **Their copy**: prose can be read for structure. Their wording is theirs.

### What we already have

- Our README has the same skeleton: why, contents, install, first run, keys, GPU
  enhancements, capture cards, troubleshooting, settings, more, license
  (`README.md:19-249`).
- A status vocabulary already exists for capture cards (`capture-cards/`, `Status:`
  lines in `src/sources.rs`), and our README has the card table (`README.md:153-160`).
- A clear "not for competitive play" warning is not in our README. Their "Smoother, not
  faster" (`README.md:188-190`) is the equivalent, and we need one for the 30 fps mode.

### Concrete proposals

- **Name**: keep protoHDMIcapture. It is the product, it is generic, and it already
  has releases and a binary name. Tagline: "A low-delay viewer for any HDMI capture
  card: the picture and the sound, nothing recorded." It matches our CLAUDE.md
  ("the picture and the sound, nothing recorded or streamed").
- **Logo**: our own mark, no game or brand imagery. Their mark (a dot and a track) is a
  clean pattern, but do not copy it. A simple monochrome mark of a frame with a
  sound wave would fit the product.
- **Hero**: a live split, our own footage only. The footage must be ours: the viewer's
  test pattern, the MS2130's loop-out, or the user's own recording of a non-copyrighted
  test scene. No game footage in a public repo unless the user says it is theirs to
  show.
- **Diagram**: one SVG of the path: card → USB → viewer → compositor → screen, with the
  45 ms span marked from first packet to presented. That also documents the endpoint
  (Q3). Our `docs/how-it-works.md` already has the table; a picture helps.
- **Comparison page on GitHub Pages**: worth doing for frame generation, since
  the README cannot show 60 fps motion. Host our own MP4s at their real frame rate and
  label each clip "simulated" or "live", as they do. Needs footage (above).
- **Works-with table**: our card table with a status column, as theirs: Tested (with
  date and setup), Experimental, Not tested. We have the data in `capture-cards/`.
- **FAQ**: borrow the questions, answer them for us: "Does it change my console?" (no,
  the card only listens), "The picture is black" (HDCP, from `README.md:83`), "Why is
  there a delay?" (the card's own latency is outside our 45 ms), "Does it work with my
  GPU?" (the dedicated-GPU rule for frame generation).
- **Help test it**: copy their "tell us" list: GPU, card, monitor Hz, connection, what
  happened.

---

## Q6. Anything else worth borrowing

Ranked by value to us, highest first. The ranking is in the table at the top; these
are the notes behind it.

1. **Live test mode and baselines** (ideas 1, 3). Our biggest gap for #86 and #63 is
   a live, honest judge.
2. **Behind rule** (idea 2). Protects the real frame when the generator is late.
3. **Lag tool, side mode** (idea 4). Turns our 45 ms into a repeatable measurement.
4. **Help-test-it list and copy-diagnostics** (idea 10). Cheap, better issues.
5. **Signed updates** (idea 9). RSA-3072 over version and SHA-256, checked by the
   launcher before it runs an update (`installer/launcher/UpdateSignature.cs:1-25`). Our
   release path is release-please; signing would need a CI step and a key kept off GitHub.
   Only worth it once releases go to users outside the author's circle.
6. **Audio delay line with drift resampling** (idea 8). Only for audio drift.
7. **Auto-detect with a timing run** (idea 11). Conflicts with our decided plan. A
   scope question.
8. **Changelog style** (idea 12). Plain, grouped, thanked. Low value: release-please
   writes ours from commit subjects.
9. **Installer and launcher**: their WPF launcher with a settings page and live link to
   the running viewer (`framegen/remote.py`, `installer/launcher/`). Large; out of scope.
10. **Looks, instant replay**: out of scope for the viewer, by our own decision.

Not borrowed, with reasons:
- Steady HUD (`framegen/steady.py`): our `ffx` already has a static-HUD mask
  (`THIRD-PARTY-NOTICES.md`, "openfg", `src/framegen/ffx/hud_mask.wgsl`).
- Pacing detail for DXGI (`dxgi_present.py`): Windows-only, and our present path uses
  wgpu.

---

## Open decisions for the user

1. **Remote Play as a second source.** Options: none; window capture of a streaming app
   (recommended, experimental, no licence change); a patched chiaki-ng fork (AGPL,
   needs a legal read); a link to chiaki (not acceptable); our own client (not
   recommended). Recommendation: window capture as a dev-flag experiment.
2. **How we state the 45 ms.** Recommendation: keep it, name both endpoints in the
   README (card's first packet to the compositor's presented time), and add the
   dequeue-to-present figure beside it. Do not compare it with SmoovPlay's 36 ms in
   public.
3. **Live test mode for shipped builds or dev only.** Recommendation: a dev build first,
   then ship it as an Experimental option if the scores hold up.
4. **Even out 40-60 fps.** It is outside the 30→60 scope. Do we want it? Recommendation:
   not yet; revisit after #86.
5. **Auto-detect timing run.** Our plan decided against it. Recommendation: keep the
   decision.
6. **Signed updates.** Who holds the key, and when do we start signing? Recommendation:
   before the first release to users other than the author.
7. **Footage for the README and a Pages comparison.** Only our own material: test
   pattern, loop-out, or the user's own non-copyrighted content. Confirm before any game
   footage is published.
8. **Logo and name.** Keep the name. Commission or make a mark ourselves.
9. **FSR RCAS denoise and sharpness default.** Judge by eye on a clip: 0.25 stops (ours) or
   0.5 (theirs), with denoise on or off. Recommendation: test, then decide.
10. **Licence hygiene.** No code from SmoovPlay is needed for any idea here; every idea is
    a reading-only borrowing. If we do port anything (for example the lag script's
    algorithm), it keeps MIT and its notice.
