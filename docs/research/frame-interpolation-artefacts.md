# Frame interpolation artefacts: disocclusion, flow confidence, HUD masks

Research note, 2026-11-23. The question: the ffx-quality generator (#73) shows
blocky shredding where a camera turn uncovers ground, jagged curb lines,
ragged tree-trunk edges, and a hard rectangular seam through crosswalk stripes
beside the third-person car. What do primary sources and open source do about
disocclusion, low-confidence flow and fallback, where do 8x8 block-matching
artefacts come from, how is a static HUD separated from a static car, and which
learned-interpolator ideas fit in about 2 ms? The background is in
[`docs/research/frame-generation.md`](frame-generation.md) (the tools, the
card, the latency arithmetic, the licences) and
[`docs/frame-generation-plan.md`](../frame-generation-plan.md) (#63). Nothing
here was run on a card or built.

Verification key: **[fetched]** = read at the source this session (a page,
a PDF, or a file in a clone at the tag named); **[snippet]** = search-result
text only; **[unverified]** = not confirmed; **[derived]** = our own arithmetic
or inference from fetched text. Clones were read, never built or run:
FidelityFX-SDK at tag `v1.1.4` (shallow), openfg at `main` (shallow). Paper
text came from the arXiv PDFs with `pdftotext`. The ffx-quality code was read
from the local branch `73-ffx-quality` (`git show`, no checkout).

## In short

- **FSR 3 does not solve disocclusion without depth.** Its disocclusion mask
  is built from dilated depth and the game's motion vectors
  (`ffx_frameinterpolation_disocclusion_mask.h`, `ComputeDepthClip`). Its
  optical-flow path has no forward-backward check, no occlusion flag and no
  confidence-weighted blend: the search runs one way (previous to current), and
  the confidence it computes is never used. [fetched]
- **FSR 3's fallbacks are structural, not photometric.** An off-screen side is
  dropped; a pixel with no valid side is filled from a mip pyramid of the
  interpolated frame; a whole-frame scene change (9-section histogram KL
  divergence) zeroes the flow. [fetched]
- **The most useful open reference is openfg, not FSR 3.** Its interpolation
  uses a cross-error visibility weight (each side is compared with the other
  frame at the output position) and soft confidence ramps, and its scene-cut
  guard is a global threshold. It has no published GPU budget beyond
  "0.98 ms per 1440p pair" (its whitepaper). [fetched]
- **ffx-quality's shredding is probably a hard choice, not a bad flow.** Per
  pixel it takes the best of four candidate vectors by colour mismatch, with a
  hard switch to one side or the other (`interpolate_two.wgsl`, `fs`). Adjacent
  pixels therefore pick different sources. Soft weights over the four
  candidates it already samples cost almost nothing. [fetched source reading;
  the cause is [derived], not yet measured]
- **The seam beside the car is a mask rule, not the car.** The HUD mask marks
  any 8x8 block that stays identical and high-contrast for three pairs, and a
  pixel whose vector reaches a masked block is replaced by an unwarped 50/50
  blend (`interpolate_two.wgsl`, the `masked(y ± ½v)` test). A screen-locked
  car is exactly that, and so are the stripes within half a vector of it.
  [fetched source reading; [derived] mechanism, unmeasured]
- **Learned ideas that fit are the visibility weights and the fusion mask**,
  not the networks. RIFE's IFNet alone is 7 ms on a TITAN X Pascal at 640x480.
  A correction to the existing note: the RIFE paper gives 16 ms at 640x480 and
  about 3 GB for 1080p, not the "68 ms at 1080p" that
  [`frame-generation.md`](frame-generation.md) cites. [fetched]

## Anchors in our code (what ffx-quality does now)

Read from branch `73-ffx-quality` [fetched, `git show`]. Commit messages give
the timings.

- Search: 8x8 blocks, radius 16 at full resolution, 4x4 refinement, two-way
  flow (`src/framegen/ffx.rs`, `search_wide.wgsl`, `refine.wgsl`).
- Candidates per pixel (`interpolate_two.wgsl`, `fs`): `-nearest_n`,
  `nearest_p`, `-vn` (bilinear forward), `bilinear_p`. For each, `attempt`
  samples both ends and tests agreement: the flow from one end must land
  within a 2-luma-pixel tolerance of the other end's vector (`tolerance =
  2 * scale`), and `|v| <= max_motion`. The candidate with the smallest colour
  mismatch among both-agreeing ones wins; blend 50/50.
- Fallbacks in that order: one-sided copy from the agreeing end; `one_way`
  (average along the forward flow); no inpainting.
- HUD: `hud_mask.wgsl` marks a block when at least 4 of 64 pixels are equal
  in both frames (`< 0.012`) and locally contrasty (`> 0.12`), after three
  such pairs in a row (`HUD_FRAMES = 3`); a block dilation follows. The
  interpolation then (a) keeps a pixel unmoved when it is equal in both frames
  and something moves, and (b) returns a 50/50 blend of the unwarped frames
  wherever `masked(y)`, `masked(y - ½v)` or `masked(y + ½v)`.
- Timing: ffx 0.43 ms, ffx-quality 4.23 ms median at 1920x1080 on the RX 9070
  XT, of which the mask is +0.04 ms (commit f58cf51). Budget left under the
  5 ms ceiling: about 0.8 ms.

## 1. How the reference interpolators handle disocclusion, low confidence and fallback

### FidelityFX Frame Interpolation (FSR 3), SDK v1.1.4

Source: `sdk/include/FidelityFX/gpu/frameinterpolation/` and the technique
doc `sdk/docs/techniques/frame-interpolation.md` (FFX Frame Interpolation
1.1.3) in the cloned tag [fetched].

- **Two inputs of different quality.** The game-vector path uses the upscaler's
  dilated depth and motion vectors. The disocclusion mask is the depth-clip
  test of the temporal upscaler (`ComputeDepthClip`, a Ksep depth separation
  between the reprojected depth and the current depth). This needs depth and
  game vectors; we have neither. The doc says so: the setup pass consumes "the
  dilated depth and motion vectors surfaces from the `Fsr3Upscaler`".
- **The optical-flow path (what we would port) is blended by bounds, not
  consistency.** `computeOpticalFlowVectorField` (in
  `ffx_frameinterpolation_optical_flow_vector_field.h`) takes a 3x3
  neighbourhood of block vectors, weights each by `max(0, dot(avg, v))^1.25`
  times a length term (`len_factor`), and writes the result. The confidence
  is read twice (`fConfidenceFactor`) and not used in either weight. [fetched]
- **`computeInterpolatedColor`** (`ffx_frameinterpolation.h`, lines ~83–165):
  - samples the flow at both ends (`SampleOpticalFlowMotionVectorField`,
    `ffx_frameinterpolation_common.h` line ~375); `bPosOutside` and
    `bNegOutside` only test whether an endpoint is on screen;
  - `ofT = 0.5` when both ends are on screen, `0` or `1` when only one is, so
    the on-screen side wins;
  - a colour similarity `NormalizedDot3(prev, curr)` (a dot product of the two
    samples, each scaled by the larger norm: 1 for equal colours, lower with
    brightness change) biases the choice between the game-vector result and the
    optical-flow result: `fGameMvBias = sim_game / sim_OF`, blended in;
  - holes (both samples off screen, or both zero-weight) raise an inpainting
    weight.
  This is a single, unweighted colour blend when both ends are valid. Where the
  flow is wrong but on screen, nothing marks the pixel. [fetched]
- **Inpainting** (`ffx_frameinterpolation_inpainting.h`, `ComputeInpainting`):
  a 10-level mip chain of the interpolated colour, holes excluded from each
  level, the levels blended with weight `(1 - m/10)^3`. This is the only
  disocclusion fallback that fills from the visible surroundings.
  [fetched]
- **HUD-less mode** (`computeInpainting`, and `CalculateStaticContentFactor` in
  `common.h` line ~199): where the interpolation source differs from the
  present frame, the output is pulled toward the present colour by a
  per-channel min/max ratio with a 0.1 ramp. It needs a HUD-less back buffer
  the game provides. We do not have one. [fetched]
- **Scene change** (`ffx_opticalflow_compute_scd_divergence.h`): 9-section
  luma histograms, smoothed, compared with the previous frame by a symmetric
  KL-style term, and a threshold. A detected change sets the flow to zero
  (`IsSceneChanged` early-out in `ffx_opticalflow_compute_optical_flow_v5.h`)
  and makes interpolation copy the current back buffer. [fetched]
- **Official statements** (the SDK doc and GPUOpen): the interpolator takes
  "both performance and quality improvements" from its optical flow, which "has
  been developed from AMD Fluid Motion Frames"
  ([GPUOpen, FSR 3 technical details](https://gpuopen.com/news/fsr3-in-games-technical-details/)
  [fetched]). No page describes a confidence-based fallback or occlusion
  handling for the flow path. [fetched, negative result]

### AMD Fluid Motion Frames (AFMF): driver level, closed

- The driver-level pipeline is not published. AMD's AFMF pages state what
  they do, not how. The 2023 preview notes say AFMF "preserves image quality
  by dynamically disabling frame generation during fast motion", that it
  "may introduce additional latency", and that the preview "requires the game to
  be played in fullscreen mode with VSYNC disabled"
  ([AMD release notes RN-RAD-WIN-23-30](https://www.amd.com/en/resources/support-articles/release-notes/RN-RAD-WIN-23-30-AFMF-TECH-PREVIEW.html)
  [fetched]). The AFMF product page is client-rendered and gave no text when
  fetched [fetched, empty].
- AFMF 2 notes add a "High" search mode for fast motion (AMD's release
  notes, [snippet]). Nothing in them describes occlusion, UI or fallback.
- **Answer: AFMF's internals, its handling of disocclusion, its confidence and
  its UI treatment are not documented by AMD.** Its fallback is the only one
  stated: turning frame generation off during fast motion. [fetched, negative
  result]

### Lossless Scaling (LSFG): closed, and what the vendor says

- LSFG 2.0 (developer news, 8 May 2024) says it reduces "motion ghosting, edge
  distortion, and image blurring", that base rates of 30/40 fps at 1080p/1440p
  are the minimum, that GPU load rises by 1.5–2x, and "in cases where artifacts
  persist, it is advisable to increase the base frame rate"
  ([losslessscaling.com](https://losslessscaling.com/introducing-lsfg-2-0/)
  [fetched]).
- Developer news ([Steam](https://store.steampowered.com/news/app/993090/view/3874849112275096626),
  [snippet]) says LSFG 2.2 "reduces the level of over-detection" in its UI
  detection, LSFG 3 disables generation below 10 fps during loading screens,
  and X3 and X4 modes target patterned textures and dark scenes. None describes
  the method.
- The third-party lsfg-vk docs list the options of the same LSFG engine:
  `flow_scale` ("the resolution scale at which the motion vectors are
  calculated", lower means faster and worse) and `performance_mode` (a lighter
  model). So LSFG computes motion vectors at a reduced resolution, as our
  ffx path does at half resolution ([lsfg-vk configuration options](https://lsfg-vk.dev/docs/configuration/configuration-options/)
  [fetched, third-party reading of the vendor option]). The docs say nothing on
  occlusion or UI.
- **Answer: LSFG's disocclusion, confidence and fallback are not documented by
  the vendor.** Its UI handling is described only as detection that has been
  tuned for over-detection. [snippet / fetched, negative result]

### TV motion-compensated frame-rate conversion (MEMC)

- No standard specifies the algorithm. The classic TV reference is the
  recursive block-matching estimator of de Haan et al., "True-motion estimation
  with 3-D recursive search block matching", IEEE Trans. CSVT 3(5), 1993 (DOI
  10.1109/76.246088). Its abstract reports bidirectional convergence,
  convergence accelerators and eight candidate vectors per block ([snippet];
  the paper was not fetched).
- **Halo** is the MEMC term for our disocclusion artefact: "a halo artifact is
  generated" when the reveal region is filled from the frame that covered it
  ([US 9148622, Hong Kong ASTRI](https://patents.justia.com/patent/9148622)
  [snippet, patent text not fetched]). Its fix assigns hybrid bidirectional
  vectors to covered regions, and it notes that the computational load "may be
  excessive". US 8537283 uses a hierarchical block-to-pixel search with a
  halo-reduction filter ([snippet]). EP2224738 notes that some halo methods need a
  future frame and add delay ([snippet]).
- Papers that the patents cite but that were not read: Mertens and de Haan,
  "Motion Vector Field Improvement for Picture Rate Conversion with Reduced Halo"
  (VCIP 2001); Wittebrood et al., "Tackling Occlusion in Scan Rate Conversion
  Systems" (ICCE 2003); Bellers et al., "Solving Occlusion in Frame Rate Up
  Conversion" (ICCE 2007) [unverified].
- **Answer: the TV references exist as patents and papers, not as standards.
  Their fix for the halo is the same as openfg's: a visibility decision per
  region, from the two directions.** [snippet]

### openfg (open, MIT): the closest primary reference we could read

openfg is an open Vulkan layer whose default backend is the FidelityFX
flow path, with its own interpolation and a static-UI mask. Its whitepaper
(its whitepaper, OPENFG-WHITEPAPER.md in openfg's docs folder) and shaders were read in a shallow clone
[fetched].

- **Visibility by cross-error** (whitepaper §6.1). With flow `v` and time `t`,
  the samples `c_a = A(u - tv/s)` and `c_b = B(u + (1-t)v/s)` are compared with
  the other frame at the output position: `e_a = |c_a - b_0|`, `e_b = |c_b -
  a_0|`. Each side gets a confidence `κ = 1 - smoothstep(0.03, 0.25, e)`, and
  weights `w_a = (1-t)(0.05+κ_a)/(e_a+0.02)`, `w_b = t(0.05+κ_b)/(e_b+0.02)`.
  The 0.05 floor keeps a side from getting zero weight; the ratio is clamped to
  [0.5, 2], so the blend stays between 1/3 and 2/3. The whitepaper says
  this "was chosen to suppress doubled edges" and calls it "a deliberate
  quality/ghost trade-off and a known limitation" at 3x and 4x.
- **Static detail** (§6.2): where the two unwarped samples differ by less than
  0.004, the output is forced to the current frame.
- **Cut guard** (§6.4): a global luma residual score over the flow-compensated
  frames; above 0.08 the whole pair is replaced by the current real frame. Used
  on video: a hard cut moved from 12.21 dB to 99 dB. FidelityFX's variant reads
  the SCD history bits instead.
- **Fallbacks** (§4.7; and the FidelityFX fallback paragraph in §5): the layer
  falls back to a plain present when it cannot run; FidelityFX falls back to the
  OpenFG flow pipeline when subgroup features are missing.
- **Cost**: "0.98 ms per synthetic frame pair at 1440p" for the FFX backend,
  against 1.56 ms for its own flow backend (whitepaper, measured on the author's
  hardware [fetched, not our GPU]).

## 2. Confidence measures and fallbacks: what they cost

Per-pixel costs are [derived] from the operations in the cited code, at 1080p
(2.07 Mpx). A texture fetch is one bilinear or nearest read.

| Measure | Where it comes from | What it tells you | Cost per pixel | Notes |
|---|---|---|---|---|
| Forward-backward consistency: `|w_f(x) + w_b(x + w_f(x))|² < α1(|w_f|² + |w_b|²) + α2` | UnFlow, arXiv 1711.07837, eq. (1), α1 = 0.01, α2 = 0.5 [fetched] | occlusion flag, per pixel | one backward-vector fetch plus arithmetic; needs a second search, which is the main cost | ffx-quality uses a fixed 2-px tolerance (`attempt`), not the relative form |
| Photometric cross-error (`e_a`, `e_b`) | openfg §6.1 [fetched] | visibility per side, sees occlusion | 4 fetches (two unwarped, two warped) | the best measure we have for the blend weight |
| Census transform (ternary, 3x3) | UnFlow §3 [fetched] | robust to gain and gamma changes | 9 reads, or precomputed once per frame (8 compares per pixel) | an inpainting-free way to make the photometric test brightness-robust; not used by FSR 3 |
| Colour similarity (cosine-like `NormalizedDot3`) | FSR 3 `common.h` [fetched] | cheap confidence | 3 multiplies, two normalisations | scale-sensitive in a way openfg's error is not |
| Matching cost (SAD minimum) | FSR 3 search pass [fetched] | how well the best block matches | free from the search | fails in flat or repeated texture (the crosswalk) |
| Flow smoothness, 2nd order | UnFlow §3 [fetched] | discourages isolated outliers | a 3x3 read on the block field | cheap at block resolution |
| Scene change, 9-section histogram KL | FSR 3 `ffx_opticalflow_compute_scd_divergence.h` [fetched] | whole-frame cut | a histogram per frame | global, not per pixel |

| Fallback | Source | Cost per pixel | Artefact it leaves |
|---|---|---|---|
| Plain blend, `0.5(a+b)` | ffx-quality `one_way`; mpv `--tscale` | 2 fetches | ghost (doubled edges), no tear |
| Nearest real frame (copy) | FSR 3 reset and out-of-rect path; openfg cut guard | 0 | a jump in time, sharp |
| One-sided copy from the agreeing end | ffx-quality `one_sided`; FSR 3 `ofT = 0 or 1` | 1 fetch | hard edge where the choice changes |
| Inpainting from a mip pyramid, weight `(1 - m/10)^3` | FSR 3 `ComputeInpainting` [fetched] | 10-level pyramid, about one pass each | smooth fill; blurs the hole, no tear |
| Soft weights from cross-errors | openfg §6.1 [fetched] | 4 fetches plus a few ops | ghost where unsure, no hard edge |
| Learned softmax importance `exp(α·error)` | Niklaus and Liu, arXiv 2003.05534, eq. (13)–(14) [fetched] | scatter with atomics; see §5 | depends on splatting |

Plain blending never tears because it has no hard choice per pixel; the tears
come from choosing one source per pixel on a noisy confidence. That is the
mechanism in ffx-quality's `fs` (section "Anchors" above) [derived].

## 3. Block-matching artefacts and the standard fixes

**Causes, in the FSR 3 flow as read [fetched source; cause analysis derived]:**

- **Constant vector per 8x8 block.** The vector is constant over 64 pixels, so
  a curb or a trunk edge that crosses a block takes one wrong vector on part of
  its length: a step, which reads as jaggedness.
- **Integer vectors.** The search is over whole luma pixels
  (`ffx_opticalflow_compute_optical_flow_v5.h`, the `SearchRadius` of 8 and the
  `DecodeSearchCoord` offsets). A sub-pixel motion is rounded, and bilinear
  sampling at a half-pixel offset then doubles or smears edges.
- **Aperture and repetition.** SAD over 8x8 cannot tell apart the periodic
  crosswalk stripes, or a flat road, so the minimum is a tie and the wrong
  one can win. The zero-motion fallback (`FFX_LOCAL_SEARCH_FALLBACK` in the same
  file) sets the vector to zero when the zero-motion SAD is no worse than the
  best candidate. That prefers a static answer exactly where the stripes are
  ambiguous, which is a candidate cause of the seam beside the car [derived; not
  measured].
- **Disocclusion.** A region the camera uncovers has no true match in the
  previous frame. The search returns the best unrelated block, and the
  neighbourhood then looks consistent (a 3x3 median of wrong vectors is still
  wrong). That is the shredding [derived].
- **Hard selection per pixel** (ffx-quality's `fs`): the four candidates are
  close, so a small change in colour changes the winner; neighbours differ.
  This is a separate cause from the flow itself [derived].

**Standard fixes, and what FSR 3's flow already does:**

| Fix | In FSR 3's flow | Source | Cost (1080p, derived) |
|---|---|---|---|
| Vector median, 3x3 (the vector with the least summed squared distance to the others) | yes: `FilterOpticalFlow`, `ffx_opticalflow_filter_optical_flow_v5.h`; the docs call it a "3x3 median filter" | [fetched] | about 0.02 ms on the block field |
| Hierarchical search, predict from the coarser level, 4 candidates per block on the way up | yes: `ScaleOpticalFlowAdvanced` picks the lowest of 4 candidates by 4-row SAD; 7 levels, max 512 px | [fetched] | part of the 0.43 ms |
| Neighbourhood vote, weight `dot(avg, v)^1.25` | yes, in the interpolator's `computeOpticalFlowVectorField` (3x3, the confidence unused) | [fetched] | about 0.02 ms |
| Overlapped block motion compensation (OBMC): each pixel uses a window-weighted mix of the neighbouring blocks' vectors | no: the interpolator reads the block vector with a bilinear splat (`computeOpticalFlowFieldMvs`, 4 neighbours, priority by `InterlockedMax`) | Orchard and Sullivan, IEEE TIP 3(5), 1994 (DOI 10.1109/83.334974) [snippet; not fetched] | 4 vector fetches per pixel; the warp count stays at 2 |
| Smoothness / regularisation of the field | partly, by the vote and the median; no global smoothness term | UnFlow §3 [fetched] | small |
| Edge-aware or joint-bilateral upsampling of the flow | no | Kopf et al., "Joint bilateral upsampling", SIGGRAPH 2007 [unverified; not searched] | 4 to 9 fetches per pixel |
| Sub-block refinement (4x4, or per pixel) | not in FSR 3's flow; ffx-quality has `refine.wgsl` | [fetched for our branch] | per-pixel 3x3 search at full res, about 0.3–0.6 ms (derived) |

FSR 3's interpolator gets the block field and bilinearly splats it; the
interpolator's per-pixel vector is therefore a weighted block average, not a
per-pixel estimate. The curb and trunk artefacts most likely come from the
block step, not from the median.

## 4. Static HUD detection and the static car

**How FSR 3 separates UI.** It does not detect it. The game gives two buffers:
the picture with the UI, and a HUD-less copy (`currentBackBuffer_HUDLess` in the
dispatch description, `frame-interpolation.md`). The interpolator then checks
where the two differ and pulls those pixels to the present frame
(`CalculateStaticContentFactor`). The GPUOpen note says the HUD is drawn on
both interpolated and real frames on the RHI path, "for artefact-free
clarity" [fetched]. A capture has one buffer, so this route is closed to us.

**What game-agnostic tools do.**

- **openfg** [fetched, `src/openfg/shaders/ui_mask.comp`, whitepaper §6.3]: an
  8x8 block is static when at least 4 of its 64 samples satisfy `|A - B| <
  0.012` (identical in both source frames) and `max(|dLx|, |dLy|) > 0.12`
  (high local contrast). Blocks are dilated by one block. The interpolator
  blends toward the unwarped pair over the masked area and samples the mask at
  both warped source positions. Its own text says the mask "covers static text
  only" and "does not detect moving or fading UI". The per-pixel static guard
  (`< 0.004`) runs in the flow backend only.
- **LSFG** [snippet]: UI is detected by a separate step, which the developer
  says over-detected at one point and was relaxed in 2.2. The method is not
  documented.
- **FSR 3 scene change**: a KL divergence of luma histograms. Not a UI test.

**The pitfall with a third-person car.** openfg's rule, and ffx-quality's copy of
it, detects "identical in both frames and contrasty". A third-person car is
screen-locked, so its pixels are identical from frame to frame except where
its wheels turn, its lights change or a reflection moves. The rule therefore
marks the car. Consequences [derived]:

1. Inside the car, the mask is harmless: zero flow gives the same pixels, and
   the output is right either way.
2. The harm is at the mask edge. A pixel whose vector reaches a masked block
   (`masked(y ± ½v)`) is replaced by the unwarped 50/50 blend. Road stripes
   within half a vector of the car, moving under it, are therefore frozen into
   a blend, while stripes further out are warped. The boundary runs along the
   block grid and is one block wider than the car's mask, after the dilation.
   That is the rectangular seam in the clip.
3. The "unmoved" test also applies to pixels that happen to be equal in both
   frames (`hud` rule in `fs`), so flat patches of road or sky on a moving
   scene are kept unwarped if they match exactly.

A HUD is also screen-locked, so the rule cannot tell a HUD from the car by
stillness alone. What separates them is that a HUD is fixed and the car's
pixels, which are part of the scene, are often lit by a moving scene. Neither
is reliably measured from two frames. The practical answer is to stop asking
the mask to decide what is HUD, and let it only stop warping.

## 5. Learned interpolators: what transfers within about 2 ms

| Idea | Paper and figure | Transfers? | Why, cost |
|---|---|---|---|
| Bidirectional flow with a soft fusion mask `M`, output blended as `M·warp_0 + (1-M)·warp_1` | RIFE (arXiv 2011.06294, §3.2–3.3, eq. 3–4) [fetched] | the mask, yes; the network, no | a mask from our cross-errors is a few ops; the network is the expensive part |
| Coarse-to-fine intermediate flow, 3 blocks at scales (4, 2, 1) | RIFE §3.2 [fetched] | the schedule, yes | our pyramid already does this |
| Residual refinement `Δ` added to the warped result | RIFE §6.1 [fetched] | no | needs a CNN; RIFE's refine block alone is a learned step |
| Privileged distillation (a teacher given the true middle frame, used only in training) | RIFE §3.3 [fetched] | no at runtime | training-only, needs ground truth; the teacher is discarded at inference |
| Intermediate feature refinement with a geometry-consistency term | IFRNet (arXiv 2205.14620) [fetched] | no | needs a learned feature pyramid |
| Scale-agnostic shared-weight pyramid; Gram-matrix loss for inpainting | FILM (arXiv 2202.04901) [fetched] | the pyramid idea, yes; the rest no | training-side and network-side |
| Inter-frame attention, flow-free | EMA-VFI (arXiv 2303.00440) [fetched] | no | transformer-scale cost; their ablation runtimes are 26–30 ms (their resolution and GPU per their text, [unverified] for ours) |
| Softmax splatting: forward warping with `Z = α·|I_0 - backward(I_1)|` and `exp(Z)` weights | Niklaus and Liu, CVPR 2020 (arXiv 2003.05534, eq. 13–14) [fetched] | the weighting, yes; the splatting, costly | their CUDA splat is 3.7 ms on a TITAN X at full HD, the backward warp 1.1 ms (their PyTorch) [fetched]; scatter with atomics is heavy on our budget |
| RIFE runtime, for scale | RIFE Table 1 and Table 4 [fetched] | not usable | IFNet 7 ms and RIFE 16 ms per 640x480 pair on a TITAN X Pascal, 9.8 M parameters; about 3 GB at 1080p. Linear scaling gives about 108 ms at 1080p [derived] |

The transferable part is the weighting, not the model. Softmax splatting's
importance `Z` is the photometric error of the backward warp, which is what
openfg's `e_a`, `e_b` already compute. A closed-form `exp(-e/σ)` weight over
ffx-quality's four candidates is the cheap version of that idea.

## Recommendations for #73, ranked

Budget: ffx-quality is at 4.23 ms of the 5 ms ceiling [fetched, commit
f58cf51], so every change below is either free or priced against about 0.8 ms.
Each row says what the #81 skip-frame score would show. #81 generates the
middle of N0 and N2 and compares it with the real N1, so the gap is twice the
live one and the disocclusion counts are high; that is the setting to read the
rows against. Tear blocks are 16x16 blocks with mean absolute luma error above
24, and the static-HUD rectangle counts are the same measure inside the mask.

1. **Soft visibility weights over the four candidates, in place of the hard
   argmin and one-sided copy.** In `interpolate_two.wgsl` `fs`, replace the
   `best`/`one_sided` selection with weights `w_i = exp(-mismatch_i / σ)` times
   an agreement term (both ends within tolerance), normalised across the four
   candidates, and a floor so no candidate has zero weight; keep the openfg-style
   clamp of the blend. Make the 2-px tolerance proportional to the vector length
   (UnFlow's `α1 |v|² + α2` form) so long vectors are not rejected for a
   sub-pixel miss.
   - **Why:** the candidates are already sampled, so the weights add no fetches.
     The hard switch between adjacent pixels is the most likely cause of the
     shredding (section 2 and "Anchors" [derived]); openfg uses soft ramps for
     the same reason, and FSR 3 weights its vote [fetched].
   - **Artefact:** blocky shredding where the camera turn uncovers ground; also
     ragged tree-trunk edges.
   - **Cost:** +0.01–0.03 ms (per pixel: four exponentials, one normalise) [derived].
   - **#81:** tear blocks should fall on the camera-turn triples. Expect the
     largest drop in whole-frame tear blocks on the turn, and no change on the
     static triples. If the count does not move, the cause is the flow, not the
     choice, and item 3 is next.

2. **Fix the mask rule so it stops warping and stops blending neighbours.**
   Keep the masked blocks at zero flow (so the car and the HUD stay exact),
   drop the `masked(y ± ½v)` test that replaces a pixel with the unwarped 50/50
   blend, and feather the mask over one block (a linear weight) instead of a
   hard switch. A pixel next to the mask then warps with its own vector, which
   is correct for the moving stripes beside the car.
   - **Why:** the mask only needs to stop a HUD from being dragged by a moving
     background. Zero flow inside the mask does that; the blend rule was added
     for text and makes a large static object cut a seam through its neighbours
     (section 4). openfg's whitepaper frames the rule as text-specific and says
     it does not handle moving or fading UI [fetched].
   - **Artefact:** the seam beside the car. Also protects the HUD.
   - **Cost:** about 0 ms (removes a branch and a sample; the mask itself is
     already costed at +0.04 ms) [derived].
   - **#81:** the static-HUD rectangle's tear blocks should fall toward zero on
     the driving clip; the existing `gpu_ffx_quality_keeps_a_static_hud` test
     must still pass (its 31 ghost pixels to 0). Add a case: a screen-locked
     block over a moving striped background, expecting zero tear blocks at its
     edge.

3. **Replace the one-way fallback with inpainting from the visible surroundings,
   and add a camera-turn guard.** Where no candidate agrees, FSR 3's
   `ComputeInpainting` fills the pixel from a 10-level mip chain of the agreeing
   pixels, weight `(1 - m/10)^3`. And when the fraction of pixels with no agreeing
   candidate exceeds a threshold (say 30%, tuned on the clip), show the 50/50
   blend for the whole pair, the openfg cut-guard idea applied to the
   disagreement fraction.
   - **Why:** FSR 3 and openfg both fill holes from the surroundings
     [fetched]; the current `one_way` fallback averages across the uncovered
     region, which is the halo case. A global guard limits the damage when a
     turn makes most of the frame ambiguous.
   - **Artefact:** shredding and halo at the turn; the worst triples.
   - **Cost:** +0.10–0.20 ms (a mip chain of an RGBA16F 1080p image, about 1.33
     times one pass over the frame, in two or three compute passes) plus a
     reduction for the guard [derived].
   - **#81:** the worst-K triples at the turn should lose their tear blocks;
     the global guard should show as a PSNR rise on those triples with no tear
     change elsewhere.

4. **Keep the existing 3x3 neighbourhood vote as a fifth candidate, and check
   the port against FSR 3's filter and vote.** FSR 3 votes over the block field
   before use (`computeOpticalFlowVectorField`) and median-filters the raw
   vectors (`FilterOpticalFlow`). ffx-quality has the filter pass
   (`Direction`'s `filter`); confirm the vote is in the interpolator. The vote
   is the smooth field that a jagged edge needs.
   - **Why:** block steps are the source of jagged curbs and ragged trunks
     (section 3). The vote is cheap and already in FSR 3.
   - **Artefact:** jagged curb lines and ragged tree-trunk edges.
   - **Cost:** +0.01–0.02 ms [derived].
   - **#81:** tear blocks along straight edges (curb, trunk) should fall; the
     crosswalk triples should not change.

5. **Per-pixel vector refinement near edges, bounded to ±1 px.** The 4x4
   refinement in `refine.wgsl` is block-level; a per-pixel ±1 px search at
   full resolution (9 candidates of a 5x5 SAD) gives sub-block detail where the
   curb crosses a block. Read `refine.wgsl`'s cost first; it may already do this.
   - **Why:** the vector is constant over a block (section 3); a 9-candidate
     search per pixel at the edge is the cheapest sub-block fix [derived].
   - **Artefact:** jagged curb lines and ragged trunk edges.
   - **Cost:** +0.3–0.6 ms if added at full resolution [derived]; much less if
     applied only where the 4 candidates disagree (the edge mask).
   - **#81:** the same straight-edge tear blocks as item 4, measured after it.

6. **Measure before widening the search.** The full-resolution flow and the
   16-px radius are the expensive parts of the 4.2 ms, and the commit does not
   split them. Time each pass in isolation at 1080p. Keep full resolution only if
   items 1 to 5 leave the curbs and trunks worse than ffx.
   - **Why:** the budget is 0.8 ms; the search is the likely owner of most of the
     4.2 ms [derived].
   - **Cost:** 0 (a measurement).
   - **#81:** a per-step GPU table, next to the tear-block counts.

7. **Not in budget, only as a later experiment: a learned or splatting weight.**
   Softmax splatting's `exp(α·error)` weight with forward splatting, or a small
   fusion network. Their cost (3.7 ms for splatting on a TITAN X; RIFE at 7 ms
   for IFNet alone) is above what is left [fetched costs; derived scaling].
   - **Cost:** 1–3 ms or more [derived].
   - **#81:** only worth scoring if items 1 to 3 leave the same tears.

**Do not**: add a depth-free "static" rule beyond zero flow, add a backward
search without a consistency test that uses it (the current two-way flow
already has that test; a second one adds nothing), or copy any AFMF or LSFG
claim into the design (their internals are closed; section 1).

## Gaps and what could not be verified

- **AFMF's and LSFG's algorithms, and their disocclusion and UI handling:**
  not published by the vendors. The statements in section 1 are the whole
  public record we could find [fetched, negative].
- **The seam's cause is inferred** from `interpolate_two.wgsl` and `hud_mask.wgsl`
  on the branch. It has not been measured: no mask dump on `gta6-driving-01`,
  and no run with the 50/50 rule removed. Item 2 is a hypothesis with a test.
- **The zero-motion fallback** (`FFX_LOCAL_SEARCH_FALLBACK`) is in FSR 3's
  search source; whether our port has it was not checked.
- **Costs** are [derived] from operation counts and the two measured anchors
  (ffx 0.43 ms, ffx-quality 4.23 ms). No per-pass timing exists.
- **Papers not read**: Mertens and de Haan 2001, Wittebrood 2003, Bellers 2007,
  de Haan 1993 (abstract only), Orchard and Sullivan 1994 (abstract only), Kopf
  2007, Astola et al. 1990 (vector median; not searched). Their claims above
  are [snippet] or [unverified].
- **The RIFE 68 ms figure** in [`frame-generation.md`](frame-generation.md) is not
  in the RIFE paper we read (arXiv 2011.06294). That note should be corrected
  or sourced; this note did not change it.
- **ffx-quality's shader cost per pass, and whether the port's search includes
  FSR 3's zero-motion fallback,** were not measured or checked against the SDK
  line by line.

## Sources

Primary, with the tag or version read:

- FidelityFX SDK v1.1.4 source and docs,
  [github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK) (tag `v1.1.4`):
  `sdk/include/FidelityFX/gpu/frameinterpolation/ffx_frameinterpolation.h`,
  `..._optical_flow_vector_field.h`, `..._disocclusion_mask.h`, `..._inpainting.h`,
  `..._common.h`, `..._callbacks_hlsl.h`;
  `sdk/include/FidelityFX/gpu/opticalflow/ffx_opticalflow_compute_optical_flow_v5.h`,
  `..._filter_optical_flow_v5.h`, `..._scale_optical_flow_advanced_v5.h`,
  `..._compute_scd_divergence.h`;
  `sdk/src/components/opticalflow/ffx_opticalflow.cpp`;
  `sdk/docs/techniques/optical-flow.md` (1.1.2), `frame-interpolation.md` (1.1.3).
  License: MIT (`LICENSE.txt`, [fetched in the earlier note](frame-generation.md)).
- GPUOpen, FSR 3 in-game technical details (fetched, HTTP 200):
  [gpuopen.com/news/fsr3-in-games-technical-details](https://gpuopen.com/news/fsr3-in-games-technical-details/).
- AMD, AFMF technical-preview release notes (fetched):
  [amd.com RN-RAD-WIN-23-30-AFMF-TECH-PREVIEW](https://www.amd.com/en/resources/support-articles/release-notes/RN-RAD-WIN-23-30-AFMF-TECH-PREVIEW.html).
  AMD's AFMF product page was client-rendered and returned no text
  ([amd.com/en/technologies/fluid-motion-frames](https://www.amd.com/en/technologies/fluid-motion-frames)).
- Lossless Scaling, LSFG 2.0 news (fetched):
  [losslessscaling.com/introducing-lsfg-2-0](https://losslessscaling.com/introducing-lsfg-2-0/).
  Steam news for LSFG 2.x and 3 [snippet]:
  [store.steampowered.com news 993090](https://store.steampowered.com/news/app/993090/view/3874849112275096626).
- lsfg-vk configuration options (third-party reading of the vendor option names, fetched):
  [lsfg-vk.dev/docs/configuration/configuration-options](https://lsfg-vk.dev/docs/configuration/configuration-options/).
- openfg (MIT), `main` shallow clone, read: its README and its whitepaper (OPENFG-WHITEPAPER.md in its docs folder)
  §4.7, §5.x, §6.1–6.4, `src/openfg/shaders/ui_mask.comp`,
  [github.com/sky-is-green/openfg](https://github.com/sky-is-green/openfg).
- Papers, arXiv PDFs read with `pdftotext`:
  - RIFE, Huang et al., arXiv [2011.06294](https://arxiv.org/abs/2011.06294)
    (ECCV 2022 version): §3.2–3.4, §6.1, Tables 1, 3, 4.
  - IFRNet, Kong et al., CVPR 2022, arXiv [2205.14620](https://arxiv.org/abs/2205.14620).
  - FILM, Reda et al., arXiv [2202.04901](https://arxiv.org/abs/2202.04901).
  - EMA-VFI, Zhang et al., CVPR 2023, arXiv [2303.00440](https://arxiv.org/abs/2303.00440).
  - Softmax Splatting, Niklaus and Liu, CVPR 2020, arXiv [2003.05534](https://arxiv.org/abs/2003.05534):
    §3.1, eq. (13)–(14).
  - UnFlow, Meister et al., AAAI 2018, arXiv [1711.07837](https://arxiv.org/abs/1711.07837):
    §3, eq. (1), α1 = 0.01, α2 = 0.5.
- Patents (cited for the MEMC halo mechanism; snippets only):
  [US 9148622](https://patents.justia.com/patent/9148622);
  [US 8537283](https://patents.google.com/patent/US8537283);
  [EP 2224738](https://data.epo.org/publication-server/rest/v1.2/patents/EP2224738NWA1/document.pdf).
- De Haan et al., "True-motion estimation with 3-D recursive search block
  matching", IEEE Trans. CSVT 3(5), 1993, DOI 10.1109/76.246088 [snippet].
- Orchard and Sullivan, "Overlapped block motion compensation: an
  estimation-theoretic approach", IEEE TIP 3(5), 1994, DOI 10.1109/83.334974 [snippet].
- Project's own sources (read via `git show`, no checkout): branch
  `73-ffx-quality`, commits 9790cf4, 2d9b00e, f58cf51; `src/framegen/ffx/interpolate_two.wgsl`,
  `hud_mask.wgsl`; issue #73 and #81 bodies (`gh issue view`).
