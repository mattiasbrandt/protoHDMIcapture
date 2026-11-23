# Upscaling 1080p game footage on modern monitors

Research note, 2026-11-23. The question: what is the best picture a 1080p60
card feed can get on a 1440p, 4K or ultrawide monitor inside about 1–3 ms of
GPU time, and which upscaler, sharpener, aspect treatment and chroma method
fit `SCALING` and the letterbox. It builds on
[`docs/research/frame-generation.md`](frame-generation.md) (the card, the
latency arithmetic, the licences) and
[`docs/frame-generation-plan.md`](../frame-generation-plan.md) (#63, round
one `sharp` = FSR 1 on `up-fsr1`, round two `ultra` = learned 2x on
`up-ultra`). Nothing here was built or run on a card. Timings below are the
repo's own (`docs/testing.md`, the branch commit messages) or a [derived]
estimate.

Verification key: **[fetched]** = read at the source this session (a page or
a file in a repo); **[snippet]** = search-result text only; **[unverified]** =
not confirmed; **[derived]** = our own arithmetic or inference. Timing
comparisons in this note are for an RX 9070 XT at the sizes stated.

## In short

- **For 4K (exactly 2x), FSR 1 (EASU, then RCAS) is the best cost-to-picture
  choice in reach.** It is open (MIT), a single edge-adaptive pass plus one
  sharpening pass, and costs about 0.4–0.6 ms at 4K [derived from GPUOpen's
  published bounds and our NIS measurement]. A learned 2x net is measured at
  12.4 ms paced at 1080p input without GameMode (f16; 13.4 ms f32) and
  8.6–8.7 ms paced in the window with GameMode (#77). That is three to
  twelve times the 1–3 ms budget. Its quality on game footage is unmeasured.
- **For 1440p-class targets (1.333x), keep `balanced` as the default and
  offer `sharp` as the sharper choice.** Both cost about 0.2 ms. No objective
  source says which looks better on game footage; the plan's by-eye judging
  decides.
- **An integer (pixel-exact) 2x path is not a quality win.** Nearest-neighbour
  doubling is blocky, and at exactly 2x a good kernel has the same four phases
  everywhere, so "integer" only saves weight lookups [derived]. Sharpening
  matters more than the integer ratio.
- **The driver-level upscalers (NVIDIA RTX Video Super Resolution, AMD Video
  Upscale, Intel's VSR) act inside browsers and players.** No API lets a
  third-party viewer call them, and none has Linux evidence. They are out.
- **Ultrawides: keep black pillarboxing as the default.** A 16:9 source has
  no 21:9 form to fill: PS5, Xbox and (unverified) Switch 2 output 16:9 for
  ordinary games. Stretch distorts; crop-to-fill throws away 26% (3440x1440) to
  50% (5120x1440) of the picture [derived]. Blur-filled bars are a viewer
  convention we found no primary source for [unverified].
- **Chroma: a cheap win is a better chroma filter in the convert pass** (the
  4:2:2 chroma planes are half the width). CfL and KrigBilateral are the
  known better options, but they are a port with LGPL terms (KrigBilateral)
  and an unverified cost. Measure the cheap one first.
- **Upscale after frame generation.** Interpolation runs at the card's size,
  as the plan already says; at 1080p it costs about 0.4x the same method at
  3440x1440 (pixel ratio 2.07 / 4.95 MP) [derived].

## Question 1: the best quality within 1–3 ms

### What each candidate is, and what evidence exists

| Candidate | Kind | Licence | Quality evidence | Cost evidence | Verdict for game footage |
|---|---|---|---|---|---|
| bilinear (`fast`) | sampler | n/a | the lowest score in the one test below [fetched, artoriuz] | free | too soft; keep as the cheapest preset |
| spline36 / mitchell (`balanced`) | separable kernels, libplacebo | libplacebo, see section 6 | no game-footage metric found | 0.20 ms at 1440p, measured here | the baseline to beat |
| ewa_lanczossharp (`high-quality`) | polar (EWA) Lanczos | libplacebo, see section 6 | the table below: scored under plain Lanczos [fetched] | not measured in this repo's docs | not clearly better than plain Lanczos on the one metric source |
| FSR 1, EASU + RCAS (`sharp`, #70) | edge-adaptive upscale + contrast-adaptive sharpen | MIT [fetched] | no primary metric found; AMD gives costs only | 0.17–0.25 ms at 1440p, measured (branch) | the best open spatial candidate here |
| NVIDIA Image Scaling (NIS, #71) | 6-tap scaler with directional sharpen, one pass | MIT [fetched] | none found; NVIDIA gives no numbers or sharpness range [fetched] | 0.22–0.28 ms at 1440p, ~0.5–0.6 ms at 4K, measured (branch) | an equal alternative; FSR 1 is the first try |
| SGSR 1 (Snapdragon GSR, spatial) | one-pass upscale + sharpen | BSD-3-Clause per the repo README [fetched]; the LICENSE file not read | none; no numbers in the repo [fetched] | none | an untested option; no advantage over FSR 1 we can show |
| RAVU (`ravu-lite-ar`, `ravu-zoom-ar`) | trained filter, RAISR-inspired, as an mpv shader (not a runtime net) | LGPL-3.0 [fetched] | 3rd-4th of 10 on the luma table below [fetched] | no GPU numbers given [fetched] | would beat Lanczos on that test; LGPL, so porting is out for an MIT/Apache project |
| NNEDI3 (mpv prescaler) | neural-edge interpolator, trained for mpv | not checked | none in the sources we read (the artoriuz post names it, gives no numbers) | not measured | too heavy for 1–3 ms at 4K by its design [unverified] |
| FSRCNNX | small CNN (x2, mpv shader) | not confirmed; the repo has no FSRCNNX licence file found [unverified] | none in the sources read | none | a candidate for a later test only |
| ArtCNN (C4F16 / C4F32 / R-models) | small CNN; C = real-time, R = non-real-time | MIT (link text) [fetched]; weights' licence not read | best luma doubler on the one test below [fetched] | not measured; estimates in section 2 | trained on anime; game transfer unverified |
| Anime4K v4 | anime-only shader set | MIT [fetched] | none | Vega 64 timings, no resolution given [fetched] | do not use for game footage: it targets native 1080p anime encodes [fetched] |
| SuperUltraCompact (2x, #75) | Real-ESRGAN Compact, WGSL | WTFPL (per branch commit) | none measured | 12.4 ms paced, 7.2 back to back at 1080p input, f16 (repo) | too slow to keep, per #75 |

**The one quality test we found [fetched].** Artoriuz's
"Mathematically Evaluating mpv's Upscaling Algorithms" (kumiko.png, a single
anime-style image, luma halved then doubled, gamma light). Mean as printed,
luma doubling:

| Rank | Filter | PSNR | SSIM | Mean |
|---|---|---|---|---|
| 1 | ArtCNN_C4F32 | 43.47 | 0.9923 | 1.0000 |
| 2 | ArtCNN_C4F16 | 42.68 | 0.9916 | 0.9522 |
| 3 | ravu-zoom-ar-r3 | 39.39 | 0.9880 | 0.7313 |
| 4 | ravu-lite-ar-r4 | 39.66 | 0.9878 | 0.7309 |
| 8 | lanczos | 36.51 | 0.9799 | 0.3934 |
| 9 | polar_lanczossharp | 36.07 | 0.9786 | 0.3171 |
| 10 | bilinear | 34.16 | 0.9735 | 0.0000 |

Read it with its limits: one image, an anime-leaning source, no game footage,
MAE/PSNR/SSIM/MS-SSIM averaged by an undefined rule, and the author's own
"very unscientific" caveat [fetched]. It does say two things we can use: a
learned doubler beats the hand-made kernels on this image, and the polar
Lanczos that `high-quality` uses did not beat plain Lanczos here. The author
also notes that the gaps shrink at lower scale factors.

**FSR 1's published costs [fetched, GPUOpen].** Upper bounds per GPU class:
4K 0.40 ms (RX 6800 XT / RTX 3080), 0.60 ms (RX 6700 XT / RTX 3060 Ti), 1.0 ms
(RX 5700 XT / RTX 2060 Super); 1440p 0.20, 0.30, 0.50 ms for the same
classes. Our measured 0.17–0.25 ms at 1440p on the 9070 XT is consistent with
the top class. The page gives no sharpness range or recommended value; the
branch uses RCAS at 0.25 stops, the sample's value, per #70's commit message.
The page says FSR "work[s] best in perceptual color space" and should follow
tone mapping [fetched]. Our `sharp` runs on the gamma-encoded frame, which is
where the convert pass leaves it, so this is the input AMD expects.

**NIS's published facts [fetched, NVIDIAGameWorks/NVIDIAImageScaling].**
MIT. NVScaler is "a 6-tap scaling filter combined with 4 directional scaling
and adaptive sharpening filters" in one pass. No timings, no test GPU, no
sharpness range or default. A GLSL Vulkan sample exists; WGSL is not
mentioned.

**SGSR 1 [fetched, SnapdragonGameStudios/snapdragon-gsr].** BSD-3-Clause per
the README; it is "upscaling and sharpening in one single GPU shader pass",
with no numbers and no stated sharpness parameter. It ships GLSL and HLSL
versions per Qualcomm's blog [snippet]. We have no evidence it beats FSR 1.

**Driver-level and platform upscalers [snippet unless stated].**
- *NVIDIA RTX Video Super Resolution.* Works in Chrome, Edge, Firefox and VLC
  (NVIDIA blog, snippet). Input from 360p up to 1440p, output up to 4K
  (Wikipedia and NVIDIA blog, snippet). GeForce RTX 20-series and newer
  (1.5 update, snippet). No Linux support found; a developer-forum thread says
  it is missing on Linux and NVIDIA's own page was not reachable [fetched
  404]. Apps reach it only through the RTX Video SDK (DX11, DX12, Vulkan and
  CUDA, RTX 20 and newer; RTX 50 in SDK 1.1; snippet). The SDK's licence was
  not read [unverified]. Verdict: not usable by a Linux viewer.
- *AMD Video Upscale.* Radeon RX 7000 with Adrenalin 24.1.1, a CAS-based
  filter extended to 4K, on Windows 11 with "Process Video Automatically" on
  (snippets from Tom's Hardware / Yahoo Tech). Browser list not found; an AMD
  support page and the 2026 AMD technical article timed out [unverified].
  Verdict: no app API found; not usable.
- *Intel VSR.* Chromium browsers only, unofficial in 2023, 10th-gen-and-newer
  iGPUs and Arc (snippets). No current status found. Verdict: not usable.

### The 1–3 ms answer, by target

- **1440p (1.333x), `balanced` or `sharp`:** both fit with room. The
  best-scoring learned option we measured (12.4 ms) does not, and the
  ArtCNN-class nets would need the 1080p-to-2160 network to be run and then
  filtered down [derived, ~12 ms before the filter]. Not worth it here.
- **4K (2x):** `sharp` (EASU+RCAS) is the best candidate inside 1–3 ms. A
  C4F16-class net (12k parameters) might fit 1–3 ms at 1080p input, but
  this is [derived] from parameter count (about one MAC per parameter per
  LR pixel, so roughly 25 GMAC or 50 GFLOP per 1080p frame at 60 fps, at a
  guessed 10–25% of 9070 XT FP32 efficiency). It is unmeasured.

## Question 2: 4K specifics

**Integer 2x against a fractional scaler.** At exactly 2x every output pixel
sits at one of four phases relative to its source texels. A kernel can
therefore use four precomputed weight sets, which saves lookups but does not
change the picture [derived]. Nearest-neighbour doubling (the literal
"pixel-exact" option) gives 2x2 blocks and stair-steps on diagonal edges; it
is not a candidate for game footage [derived]. The quality difference in
practice comes from the kernel and the sharpener, not from the ratio being
an integer. FSR 1 at 4K is the same EASU kernel that runs at 1440p, with
the sharpened result at 2160 rows.

**Does a learned 2x become worth it at exactly 2x?** The measurements we
have say no for the budget: 12.4 ms paced for the 2x-SuperUltraCompact net
at 1080p input (repo), against a budget of 1–3 ms. The quality case is open:
the one test we found [fetched] favours a learned doubler (ArtCNN) over
every hand-made kernel, but it used an anime-leaning image, and ArtCNN is
trained on anime [fetched]. An honest next step is a dev-only timing and
a judged clip of a C4F16-class net on the kept clip (`gta6-standing-03`),
not a port.

**The cost figures, two conditions.** The branch's headless timing test gives
12.40 ms paced and 7.21 ms back to back for f16, and 13.39 / 8.51 ms for f32,
with the GPU's power level on `auto` (commit `65c5b93`; `docs/testing.md`).
In the window with GameMode raising the power level (#77), `ultra` measured
8.61 ms paced on the still clip and 8.67 ms on the driving clip, and still
repeated 30 and 120 refreshes (#63's round-two stage-1 comments). Either way
it is over the budget; this note quotes both.

## Question 3: ultrawide and odd aspects

**Facts, by source.**
- A 16:9 feed fitted into 3440x1440 is 2560x1440 with 440 px bars each side;
  the repo's `letterbox` test asserts exactly that [fetched, `src/app.rs`].
- 2560x1080 holds 1920x1080 at 1:1: no scaling, bars of 320 px each side at
  1920x1080 (no filter runs, so `sharp` is the frame itself) [derived].
- 5120x2160 holds 3840x2160 (2x, the 4K case) with 640 px bars each side.
- 5120x1440 holds 2560x1440 (1.333x) with 1280 px bars each side.
- **PS5:** no official ultrawide support; output is 16:9, so 21:9 screens
  show pillarbox bars (Gfinity, snippet). Death Stranding: Director's Cut's
  "Widescreen Mode" is letterboxed 21:9 inside 16:9, per a Sony spokesperson
  quoted by TechRadar (snippet). 1440p output exists since a 2022 update,
  per BenQ, a monitor vendor (snippet, vendor claim). Sony's own docs were
  not read.
- **Xbox Series X|S:** hands-on reports (2020) found 16:9 output only,
  with pillarbox bars on ultrawides. Individual games may offer 21:9
  views (Starfield, 2023 guide, snippet). Current Microsoft documentation was
  not read.
- **Switch 2:** not researched [unverified].

**The alternatives, and what they cost us.**

| Mode | What it does | 21:9 at 16:9 content | Verdict |
|---|---|---|---|
| Pillarbox (current) | keeps the aspect, black bars | 440 px each side at 3440x1440 | the default; honest, matches the console's own 16:9 |
| Stretch | fills the screen | about 34% distortion of the aspect at 3440x1440 [derived] | not offered |
| Crop to fill | scales to the width, crops top and bottom | 495 px (26%) lost at 3440x1440; 1440 px (50%) at 5120x1440 [derived] | a later option if asked for |
| Edge-aware stretch / blur-fill | stretches the centre and fills the sides from the picture or a blur | not researched | [unverified]; cost and quality unknown |

Black bars stay the default. A blur-fill of the sides is a viewer convention;
we found no primary source, so no claim is made about its quality.

## Question 4: chroma

The card's YUYV is 4:2:2: chroma is half the width of luma and full height.
At 1080p the chroma planes are 960 x 1080. The convert pass interpolates
them linearly today (`src/scale.rs`, its header). Two things to weigh.

- **How much it matters.** The one chroma test we found [fetched] used a
  4:2:0 image (chroma halved both ways, a stronger case than 4:2:2). Its
  PSNR figures: bilinear 34.2, Lanczos 36.4, KrigBilateral 38.6, CfL 40.0,
  ArtCNN chroma 43.7 (the best). Bilinear sat about 6 dB under CfL, the
  largest chroma gap in the table. The author says the non-learned options
  "all suffer from chroma-location issues" when chroma and luma are not
  correlated, and would skip them [fetched]. For 4:2:2 the gap should be
  smaller, but the sources do not say how much [derived].
- **What is cheap.** A spline36 or Lanczos-class chroma filter in the
  convert pass costs a pass over 960x1080 chroma planes: well under 0.1 ms
  on a 9070 XT [derived, unmeasured]. CfL and KrigBilateral are better
  on the same test but need more code, and KrigBilateral is LGPL [snippet
  of its header]. Our reading: try the separable filter first and judge
  colour edges on a clip (red on sky, skin on a red wall) before porting
  anything.

Chroma location: mpv's default chroma-location handling could not be read
(the manual page was truncated before its scaler sections; the search gave
only FFmpeg's default) [unverified]. We need the card's sample siting before
claiming a half-pixel shift is fixed.

## Question 5: order with frame generation

Upscale after interpolation, as the plan already has it. Reasons:
- Interpolation at 1080p costs about 0.4x the same method at 3440x1440
  (2.07 MP against 4.95 MP): the AFMF-class port takes 0.6 ms per generated
  frame at 3440x1440 on a 9070 XT [fetched in
  `docs/research/frame-generation.md`], so about 0.25 ms at 1080p [derived].
- The optical-flow and occlusion work runs on the texels the card delivers.
  Upscaling first would give the flow 2.25x (4K) more texels and the same
  information.
- No primary source compares artefacts in the two orders. The learned
  2x (`ultra`) and `sharp` both sit after the generator in the plan's shape.

A learned upscaler before the generator is the one case with a reason to
reverse: a generator that works badly on soft input. We have no evidence of
that, so the plan stands.

## Question 6: licensing (only where porting is proposed)

- **FSR 1 (`sharp`, #70):** MIT [fetched, GPUOpen page]. Already ported with
  AMD's notice in `src/scale.rs` (#70 commit). Fine.
- **NVIDIA Image Scaling (#71):** MIT [fetched]. Fine if ported.
- **SGSR 1:** BSD-3-Clause [fetched README; LICENSE file not read]. Fine if
  ported; keep the notice.
- **ArtCNN:** MIT on the code (link text, fetched); the model weights'
  licence was not read [unverified]. Trained on anime.
- **Anime4K:** MIT [fetched]. Anime-only; not proposed.
- **RAVU (mpv-prescalers):** LGPL-3.0 [fetched]. Porting the shaders would
  bring LGPL terms into an MIT/Apache-2.0 project. Not proposed.
- **KrigBilateral:** LGPL per its header [snippet]. Not proposed.
- **FSRCNNX, NNEDI3:** licences not confirmed [unverified]. Not proposed.
- **libplacebo filter maths (`scale.rs`, the spline36/mitchell/EWA
  families):** the port follows libplacebo's `src/filters.c` (its header).
  libplacebo's own licence was not read this session [unverified]; check it
  before the next release, since this is existing code, not a new port.
- **The SuperUltraCompact net:** WTFPL per the branch commit (zarxrax's
  pretrain). Not read here; it is not proposed for the ship list.

## Recommendations, ranked

Expected GPU ms: **[measured]** = in the repo or the branch; **[derived]** =
pixel-count scaling of a measured or published figure, or a parameter-count
estimate; the basis is stated each time. Reference GPU: RX 9070 XT.

**1. 4K (3840x2160, exactly 2x).**
- Use: `sharp` (FSR 1, EASU then RCAS, RCAS at 0.25 stops). Keep NIS (#71) as
  the alternative to judge by eye, not a second preset.
- Cost: about 0.4–0.6 ms [derived from GPUOpen's 4K upper bound of 0.40 ms
  for the top class, and our NIS 0.5–0.6 ms to 4K].
- Improves: edge reconstruction and the detail that a fractional kernel
  softens; cheaper than any kernel that reads more texels.
- Fits: `sharp` already cycles after `high-quality`. No new preset. A
  learned 2x would be a dev-only experiment, not a preset, until its cost
  is under ~3 ms and its game footage judged.

**2. 1440p-class (2560x1440 from 1080p; 3440x1440 pillarboxed).**
- Use: `balanced` stays the default. `sharp` for a sharper picture at the
  same cost.
- Cost: `balanced` 0.20 ms and `sharp` 0.17–0.25 ms [measured, 1440p]; NIS
  0.22–0.28 ms [measured].
- Improves: `sharp` adds the sharpening that `balanced` lacks.
- Fits: no new preset; `sharp` is the existing one. Judge by eye on the clip,
  as the plan says. Do not add a learned option at this size (about 12 ms,
  the 2x net alone [measured at 1080p input]).

**3. 5120x2160 (21:9 holds 3840x2160 with 640 px bars).**
- Same as the 4K row: `sharp`, about 0.4–0.6 ms [derived].

**4. 5120x1440 (32:9 holds 2560x1440 with 1280 px bars).**
- Same as the 1440p row: `balanced` or `sharp`, about 0.2 ms [derived].

**5. 3440x1440 and 2560x1080 (ultrawide).**
- Keep pillarbox with black bars as the only aspect mode. Stretch is not
  offered. Crop-to-fill is a later option, only if asked for; it needs a
  setting and its own decision. Blur-fill: no evidence, not proposed.
- 2560x1080 with 1920x1080 content: no filter runs (1:1). `sharp` adds
  nothing there; a sharpen-only pass would cost about 0.1 ms [derived].

**6. Chroma, all targets.**
- Try a separable spline36 or Lanczos-class chroma filter in the convert pass.
  Cost: well under 0.1 ms [derived]. Judge colour edges on the clip. CfL and
  KrigBilateral are the follow-up, with a licence check first.

**7. Driver-level upscalers (RTX VSR, AMD Video Upscale, Intel VSR).**
- Not usable from a Linux viewer. Do not add.

**8. Learned upscalers (ArtCNN-class, SuperUltraCompact).**
- Not for the ship list. ArtCNN C4F16 is worth one dev-only timing and a
  judged clip, as the only learned candidate that might fit 1–3 ms at 4K
  [derived, unmeasured]. Anime4K: no.

**What to test next, in order** (each needs the user's by-eye judgement):
1. `sharp` on the kept clip at 4K output, with its GPU ms in `--stats`.
2. A chroma spline36 pass against the current linear chroma, the same clip.
3. A C4F16 timing at 1080p input, dev-only, if (1) and (2) leave room.

## Unverified and open

- The RX 9070 XT costs for FSR 1 at 4K and for the chroma filter: derived
  from published and measured figures, not measured.
- The learned-net cost at 1–3 ms (C4F16): derived from parameter count; not
  built.
- Game-footage quality of any learned upscaler, and of EASU against
  spline36: no primary metric found.
- Whether the card's chroma sitting matches the convert pass's assumption.
- SGSR 1's LICENSE file, FSRCNNX's and ArtCNN weights' licences, libplacebo's
  licence: not read.
- Switch 2's output options; Sony's and Microsoft's current ultrawide
  documentation: not read (only press and vendor snippets).
- RTX VSR, AMD Video Upscale and Intel VSR on Linux: no evidence either way
  beyond the snippets above.
- The AMD technical article ("Upscale Everything", 2026) and AMD's support
  pages: timed out or 404 on fetch.
- (Resolved by the coordinator: 8.6 ms is ultra paced in the window with
  GameMode, 12.4 ms paced headless without it; see Question 2.)

## Sources

Fetched this session:
- GPUOpen, FidelityFX Super Resolution 1 (MIT, EASU/RCAS, 4K and 1440p
  upper-bound costs): https://gpuopen.com/fidelityfx-superresolution/
- NVIDIAGameWorks, NVIDIA Image Scaling (MIT, NVScaler description):
  https://github.com/NVIDIAGameWorks/NVIDIAImageScaling
- SnapdragonGameStudios, snapdragon-gsr (BSD-3-Clause README, SGSR 1 and 2):
  https://github.com/SnapdragonStudios/snapdragon-gsr
- Artoriuz, "Mathematically Evaluating mpv's Upscaling Algorithms" (the
  luma and chroma tables, the method caveats):
  https://artoriuz.github.io/blog/mpv_upscaling.html
- Artoriuz, ArtCNN (anime-targeted, MIT link text, model table):
  https://github.com/Artoriuz/ArtCNN
- bjin, mpv-prescalers (LGPL-3.0; RAVU, NNEDI3 content; `-ar` default 0.8):
  https://github.com/bjin/mpv-prescalers
- bloc97, Anime4K (MIT; anime-only, 1080p h264/h265/VC-1):
  https://github.com/bloc97/Anime4K
- igv, FSRCNN-TensorFlow (licence list only; FSRCNNX not found there):
  https://github.com/igv/FSRCNN-TensorFlow
- mpv manual (read to the truncation point; scaler sections not reached):
  https://mpv.io/manual/master/
- Repo, read this session: `src/scale.rs` (header and presets), `src/app.rs`
  (`letterbox` and its test), `README.md` (SCALING row, "Display modes and
  refresh"), `docs/frame-generation-plan.md`, `docs/testing.md` (GameMode,
  the learned-upscaler timings), branch commits `1c40f02` (#70), `65c5b93`
  and `87aa63f` (#75), and `docs/research/frame-generation.md`.

Search results only ([snippet]):
- Qualcomm, SGSR 1 GLSL/HLSL availability:
  https://www.qualcomm.com/developer/blog/2024/06/up-your-game-with-snapdragon-gaming-super-resolution
- NVIDIA RTX VSR, browsers, GPU generations, 4K output:
  https://blogs.nvidia.com/blog/rtx-video-super-resolution
  and https://en.wikipedia.org/wiki/Video_Super_Resolution
- NVIDIA RTX Video SDK (APIs, GPUs, SDK 1.1):
  https://developer.nvidia.com/rtx-video-sdk/getting-started
- AMD Video Upscale (Adrenalin 24.1.1, Windows 11 setting):
  https://www.tomshardware.com/pc-components/gpus/amd-video-upscaler-arrives-for-rx-7000-gpu-owners-yearning-for-nvidias-rtx-video
- Intel VSR (unofficial, Chromium, 2023):
  https://www.techpowerup.com/305558/intel-outs-video-super-resolution-for-chromium-browsers-works-with-igpus-11th-gen-onward
- PS5 ultrawide and output (Gfinity; BenQ, vendor): https://www.gfinityesports.com/article/does-ps5-support-ultrawide
  and https://www.benq.com/en-us/knowledge-center/knowledge/ps5-on-ultrawide-monitor
- Death Stranding: Director's Cut widescreen, quoted from Sony:
  https://techradar.com/news/death-stranding-directors-cut-ultrawide-support-on-ps5-isnt-quite-what-it-seems
- Xbox Series X ultrawide, launch-era hands-on:
  https://www.gfinityesports.com/tech/does-xbox-series-x-support-ultrawide
- KrigBilateral header (LGPL), via a gist:
  https://gist.github.com/igv/a015fc885d5c22e6891820ad89555637
- Not fetched, named only: libplacebo's `src/filters.c`, mpv's scaler
  manual sections, Real-ESRGAN Compact, zarxrax's SuperUltraCompact pretrain.
