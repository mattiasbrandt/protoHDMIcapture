# Frame generation and upscaling for console sources

Research note, 2026-11-23. The question: can protoHDMIcapture show a 30 fps
console game (GTA 6 on PS5, launched 2026-11-19) at 60 fps, and upscale it,
as the Windows "frame enhancer" tools do? AMD on Linux is the first test bed;
the feature must not be tied to it. Findings, not decisions: the plan that
follows from them is [`docs/frame-generation-plan.md`](../frame-generation-plan.md) (#63).
Nothing here was run on a card.

Five research passes went into this note. Their claims were spot-checked
against the cloned sources where they decide something: the licences, the
lsfg-vk present-mode override, the FidelityFX licence split, AMF FRC's
platforms, and Veyra's and SmoovPlay's READMEs.

Verification key: **[fetched]** = read at the source this session (page,
repo file or cloned source); **[snippet]** = search-result text only;
**[unverified]** = not confirmed; **[derived]** = our own arithmetic.

## In short

- **Every route works on the finished picture, and no vendor's game feature
  applies.** DLSS, FSR 2/3/4, FSR frame generation and XeSS all need the
  game's motion vectors and depth. A capture card delivers neither, on any
  GPU. Only the video-oriented features apply: spatial upscalers (FSR 1, NIS),
  optical-flow interpolation (AMD's FidelityFX Optical Flow, NVIDIA's optical
  flow hardware, AMD AMF FRC) and ML interpolation (RIFE).
- **The tools people use are Windows-only and mostly NVIDIA-only.** Veyra,
  SmoovPlay, NitLink and NeuralScreen wrap NVIDIA's Video Frame Generation or
  DLSS-G, or RIFE on TensorRT. Veyra alone offers AMD (AMF FRC) and Intel
  paths. None runs on Linux.
- **The AMD route on Linux exists and is open:** AMD's FidelityFX Optical
  Flow shaders are MIT-licensed. Two MIT Vulkan layers, afmf-linux and
  openfg, already interpolate with them on RADV, at about 0.6 ms of GPU time
  per generated frame at 3440×1440 (RX 9070 XT). Ported to WGSL, they
  would run on every GPU wgpu drives.
- **Duplicate frames come first.** A 30 fps game in a 60 Hz HDMI signal
  reaches the card as repeated frames. Base PS5 also misses its 30 fps slot
  under load. An interpolator fed the card's frames as they come interpolates
  between identical pictures. So any route, the external layers included,
  first needs to know which card frames are new.
- **Cost: about one refresh (16.7 ms) of added latency, plus compute.**
  [derived] It is a refresh, not a full 33 ms game frame, because the
  generated frame takes the slot where the repeat would have shown (below).
- **Upscaling adds no wait**, only its own GPU time. FSR 1 and NIS are MIT,
  spatial and small enough to port to WGSL next to the existing scalers
  (`src/scale.rs`).

## The latency arithmetic [derived]

A 30 fps game in a 60 Hz signal: the card delivers A, A, B, B, C, C… one per
16.7 ms. Without frame generation, B shows when it arrives and stays two
refreshes. With 2× interpolation, the frame between A and B needs B, so:

| refresh | card delivers | shown today | shown with interpolation |
|---|---|---|---|
| t | B (new) | B | A½B (generated from A and B) |
| t + 16.7 | B (repeat) | B | B |
| t + 33.3 | C (new) | C | B½C |

New game content reaches the screen one refresh (16.7 ms) later than today,
plus the time to compute the generated frame. The repeat's arrival is the
clock for showing B, which suits protoHDMIcapture's card-paced presenting
(`src/pacing.rs`). The figure is the method's minimum. It is not measured.
Published frame-generation latencies are taken at high base frame rates and
do not isolate it: AFMF about 8–13 ms, LSFG 3 about 10 ms at 40→80 [snippet],
Tom's Hardware and reviewer overlays. Today's baseline to compare against is
kernel → presented median 44.9 ms, p99 53.7 ms (`docs/testing.md`, MS2130,
2026-10-16).

Extrapolation (predicting the next frame without waiting for it) would add
no wait. Intel's ExtraSS research claims "without introducing additional
latency" at the cost of disocclusion ghosting
([project page](https://poiw.github.io/ExtraSS) [fetched]). No shipped
product does this for video, so it is a research option only.

## The source: GTA 6 on PS5, and what the card sees

- **30 fps, no 60 fps mode.** Rockstar North's Rob Nelson, via
  [TweakTown](https://www.tweaktown.com/news/113337/rockstar-says-gta-6-will-run-at-30-fps-and-its-tech-team-is-still-unsure-about-a-60-fps-mode/index.html)
  [fetched, paraphrased]. Digital Foundry's coverage says 1440p internal on
  PS5 and PS5 Pro, with PSSR on Pro [snippet; the DF article was not
  reachable]. Per DF as quoted by
  [VGC](https://www.videogameschronicle.com/news/gta-6-base-ps5-performance-analysis-shows-struggle-to-maintain-30-fps-at-all-times/)
  [fetched], Pro's frame pacing is "perfect" and base PS5 "does struggle to
  hold 30fps" in stress areas. So the cadence is a clean 2:2 on Pro, with
  breaks on base PS5.
- **Output settings** (Sony's
  [Video Output guide](https://www.playstation.com/en-us/support/hardware/ps5-4k-resolution-guide/)
  [fetched]): 120 Hz output, VRR and ALLM toggles. The project's PS5 source
  profile already sets 1080p, 120 Hz off and VRR off (`sources/ps5.md`). With
  120 Hz on, a 30 fps game would arrive as 4 repeats per frame
  [derived, unverified on a PS5].
- **The cadence is not documented anywhere** we found. The 2:2 repeat is
  arithmetic. A card can also drop or repeat frames on its own, and V4L2's
  sequence counter does not show drops inside the card
  ([kernel V4L2 buffer docs](https://docs.kernel.org/6.17/userspace-api/media/v4l/buffer.html)
  [fetched]). Detection must therefore be by content, not by counting.
  FFmpeg's `mpdecimate` is a reference design: 8×8-block differences against
  a `hi`/`lo` threshold, and the fraction of changed blocks
  ([FFmpeg 8.0 docs mirror](https://ayosec.github.io/ffmpeg-filters-docs/8.0/Filters/Video/mpdecimate.html)
  [fetched]).
- **HUD and scene cuts.** A whole-frame interpolator cannot mask the HUD
  (Digital Trends on LSFG 3 [snippet]).
  openfg adds a "static-UI mask" for still HUD text
  ([openfg README](https://github.com/sky-is-green/openfg) [fetched]).
  FidelityFX Optical Flow includes a scene-change detector (luma pyramid,
  scene-change detector, coarse-to-fine 8×8 block search, per the afmf-linux
  README [fetched]).

## The named tools

From each tool's own repo or store page [fetched unless marked].

| Tool | What it is | Frame generation | Upscaling | Platform | Licence | Latency claim |
|---|---|---|---|---|---|---|
| Veyra ([Likely7/Veyra-NRVideo](https://github.com/Likely7/Veyra-NRVideo)) | D3D12 capture-card / PS5 Remote Play player; also an OBS filter | NVIDIA Video FG, DLSS-G, AMD AMF FRC, XeSS | RTX VSR, FSR | Windows (OBS filter on Linux) | GPL-3.0 | "10–20 ms less than OBS" [snippet] |
| Veyra-NRVideo | the same repo's name. `veyranrvideo/VeyraNRVideo` is a sourceless lookalike: avoid | — | — | — | — | — |
| SmoovPlay ([smoovplay/smoovplay](https://github.com/smoovplay/smoovplay)) | PS5 Remote Play first, capture card experimental | RIFE on TensorRT, or NVIDIA VFG | FSR 1, RTX VSR | Windows | MIT | +35.4 / 35.8 ms median, self-measured |
| NeuralScreen ([perseval-BLR/NeuralScreen](https://github.com/perseval-BLR/NeuralScreen)) | window/desktop overlay, no capture input | DLSS-G | NVIDIA NR | Windows, RTX | PolyForm Strict (not open source); bundles a leaked NVIDIA DLL per its README | 25–30 ms (README) vs 40–60 ms (GameGPU) |
| NitLink ([nitlink-dev/nitlink](https://github.com/nitlink-dev/nitlink), Steam 5346980) | capture viewer; the Steam build adds FG | NVIDIA VFG (subscription) | — | Windows, RTX 3080+ | MIT viewer, EULA'd FG | none |
| NAIMAN | no primary source found | — | — | — | — | — |
| Lossless Scaling (Steam 993090) | screen-capture overlay over any window | LSFG (proprietary, optical flow) | LS1, FSR, NIS, Anime4K, … | Windows; Linux via lsfg-vk | proprietary | ~10 ms at 40→80 [snippet] |

Veyra's README carries a Pump.fun token address.

"DLSS 5" exists: NVIDIA's "3D-guided neural rendering", the last stage of
a game's pipeline, on RTX 50. It is one frame in and one frame out, and it
takes the game's colour and motion vectors
([NVIDIA, 2026-09-01](https://nvidia.com/en-us/geforce/news/dlss-5-3d-guided-neural-rendering/)
[fetched]). It is not upscaling and not frame generation, and NVIDIA makes
no latency claim. The "DLSS5" these tools advertise is not that product.
NeuralScreen loads a leaked neural-rendering DLL (`nvngx_dlssnr.dll`), and
Veyra loads NR runtimes and says its "DLSS5 image enhancement" is not an
official integration [fetched, both READMEs].

Pattern: two-frame interpolation from NVIDIA's video SDKs or RIFE, Windows
first, latency self-reported.

## Lossless Scaling and lsfg-vk on Linux

- **lsfg-vk** is a Vulkan implicit layer that runs LSFG's shaders from the
  user's own `Lossless.dll` (the user must own Lossless Scaling on Steam).
  v1.0.0 was MIT. v2 (2026-09-05) is **CC BY-NC-ND 4.0**: no commercial use,
  no derivatives ([lsfg-vk.dev blog](https://lsfg-vk.dev/blog/important-changes-to-lsfg-vk/)
  [fetched]). Nothing of it may be copied into protoHDMIcapture. A user may
  run it as a separate program.
- It hooks swapchain creation and present. It forces FIFO present mode
  (`override_present_mode`, default true) and adds swapchain images
  [fetched, v1 source and v2 docs]. protoHDMIcapture asks for Mailbox and
  presents once per card frame (`src/app.rs`, `src/pacing.rs`).
- **Run under it unchanged, protoHDMIcapture would not give a correct
  30→60** [inferred from source, not run]. The layer would interpolate the
  identical frame pairs. Its extra presents would also commit the Wayland
  surface, so the presentation feedback protoHDMIcapture counts
  (`src/present/wayland.rs`) would describe generated frames.
- **On Windows**, Lossless Scaling captures borderless or windowed windows
  ([Steam page](https://store.steampowered.com/app/993090/Lossless_Scaling/)
  [fetched]). It would capture protoHDMIcapture's window, with the same
  duplicate problem [unverified].
- afmf-linux and openfg are MIT layers of the same kind, with the same
  constraints. Once protoHDMIcapture presents only new game frames, any of
  the three becomes a cheap quality experiment, with nothing bundled.

## Techniques, by what they need

| Technique | Needs | On video alone | Added wait | GPU cost (published) | Licence | Vendors / OS | Path into protoHDMIcapture |
|---|---|---|---|---|---|---|---|
| FSR 1 (EASU + RCAS) | the frame | yes | none | 4K ≤ 0.40 ms on RX 6800 XT ([GPUOpen](https://gpuopen.com/fidelityfx-superresolution/) [fetched]) | MIT | all, all OSes | port to WGSL |
| NVIDIA Image Scaling 1.0.3 | the frame | yes | none | not published | MIT (whole SDK) [fetched] | all (fp32 path is vendor-neutral) | port to WGSL |
| FidelityFX Optical Flow + interpolation (as afmf-linux / openfg) | two frames | yes, with scene-change detection | one refresh | 560–610 µs per generated frame at 3440×1440, RX 9070 XT; ~3× on RX 7800 XT ([afmf-linux docs](https://github.com/serialexperimentslainnnn/afmf-linux) [fetched]) | MIT (SDK v1.1.x; the shader files keep MIT headers in v2) | any GPU with subgroup ops; tested on RADV only | port GLSL to WGSL compute; wgpu 30 has `Features::SUBGROUP` |
| Frame blending (mpv `--tscale`) | two frames | yes | 1–2 frames | trivial | port the idea, no code | all | WGSL; ghosts on motion |
| RIFE v4 | two frames | yes | one refresh + inference | 68 ms at 1080p on a TITAN X Pascal (2020 paper); ~11 ms per frame on RTX 3070 Ti (forum) [snippet] | MIT | NVIDIA (TensorRT), all via ncnn-Vulkan; no AMD figure | ncnn in its own Vulkan context, or a WGSL port of the network; heaviest |
| AMD AMF FRC | two frames | yes | at least one refresh; "one extra frame time" with look-ahead on | not published | MIT headers, runtime ships in the AMD driver | AMD, **DX11/DX12 only** ([AMF_FRC_API.md](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/doc/AMF_FRC_API.md) [fetched]) | Windows only; D3D interop |
| NVIDIA Optical Flow (`VK_NV_optical_flow`, OF SDK) | two frames | yes | one refresh | not published | SDK licence forbids distribution and open-source use (clauses 4(b), 4(e)) [fetched] | NVIDIA Ampere+ in Vulkan | a raw Vulkan side path; licence review first |
| NVIDIA RTX Video Super Resolution / Video FG | the frame(s) | yes | — | not published | NVIDIA SDK licence (not read) | NVIDIA RTX, Windows | what the Windows tools use; D3D interop |
| AFMF (AMD driver) | driver-internal | yes | driver-side | — | proprietary | AMD, Windows only [snippet] | the user turns it on; not ours |
| DLSS-G, FSR 3/4 FG, XeSS-FG, FSR 2/3/4 and XeSS upscaling | the game's motion vectors and depth | **no** | — | — | various, mostly binary-only | — | not possible on a capture |

Licence traps, checked in the cloned sources:
- The FidelityFX SDK v2 licence file (`license.md` in its docs folder)
  grants binary-only redistribution by default. Its exception list puts the
  optical-flow sources under MIT: `ffx_opticalflow.h` and the
  `gpu/opticalflow/` shaders, e.g.
  `ffx_opticalflow_compute_optical_flow_v5.h`. SDK v1.1.4's `LICENSE.txt` is
  MIT for the whole SDK
  ([v1.1.4 tag](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/blob/v1.1.4/LICENSE.txt)
  [fetched]). openfg vendors its optical flow from v1.1.4 with the notice
  kept.
- Intel's XeSS SDK is binary-only (Intel Simplified Software License).
- MVTools (SVP's engine) is GPL and libplacebo is LGPL: external process or
  dynamic link only. SVP itself is closed and personal-use only.
- protoHDMIcapture is MIT OR Apache-2.0, so MIT shaders can be vendored with
  their notice. A copyleft or no-derivatives source cannot.

## Capture cards: what makes a good input

- **Uncompressed, 4:2:2, steady.** MJPEG's block noise changes between
  frames, so the optical flow and the duplicate detector both see motion
  that is not there. The MS2130 at YUYV 1080p60 on USB 3 is already the
  right input class for 30→60, and it is proven on this project's Linux
  path (`capture-cards/ms2130.md`). The MS2109 (USB 2, MJPEG only at 60 fps)
  is a poor one.
- **The card's own latency now matters more.** Frame generation adds to the
  chain. A capture card's internal delay is part of that chain, and kernel →
  presented does not see it. The MS2130's "6 to 11 frames" comes from a
  HyperHDR discussion [snippet], not a measurement. The one measured
  pass-through figure found is 7.4 ms for the Elgato HD60 X
  ([thetestedhub](https://thetestedhub.com/reviews/elgato-hd60-x) [fetched],
  one reviewer).
- **Upgrade classes**, from vendor listings, not measured here:

| Card | Link | Uncompressed modes listed | Beyond 1080p60 | Linux |
|---|---|---|---|---|
| MS2130 / MS2131 sticks | USB 3 (5 Gbit/s) | YUYV 1080p60 | none | MS2130 proven here |
| Magewell USB Capture HDMI Gen 2 | USB 3 | YUY2, UYVY, RGB | input caps at 2048×1080 @ 60 | V4L2 and SDK listed |
| AVerMedia Live Gamer ULTRA 2.1 (GC553G2) | USB 3.2 Gen 2 (10 Gbit/s) | YUY2, NV12, P010, RGB24 | 4K144, 1440p240 | not listed |
| Elgato 4K S | USB 3 (5 Gbit/s) | not listed | 1080p120, 2160p60 | not listed |
| Elgato 4K X | USB 3.2 Gen 2 | not listed | 4K144, 1080p240 | a kernel quirk claimed, not confirmed in `quirks.c` |

  Sources: vendor spec pages ([Magewell](https://www.magewell.com/tech-specs/usb-capture-hdmi-gen-2),
  [AVerMedia](https://www.avermedia.com/us/product-detail/GC553G2),
  [Elgato 4K S](https://www.elgato.com/us/en/explorer/products/capture/elgato-game-capture-4k-s-technical-specifications/))
  [fetched]. The kernel's UVC driver maps P010
  (`drivers/media/common/uvc.c` [fetched]). protoHDMIcapture's `FMT` does
  not take it (`settings.rows`), and capture-card profiles take a `Link:` of
  480 or 5000 only. A 10 Gbit/s or HDR card is therefore a separate piece of
  work. For 30→60 at 1080p60 none of these does better than a working
  MS2130 on paper. What a better card buys is lower card latency (unmeasured)
  and headroom for 1440p, 120 Hz or HDR.

## Gaps

- No cadence or latency measurement on a card: the PS5's repeat pattern,
  how often base PS5 breaks it in GTA 6, and the MS2130's internal delay.
- No measured GPU cost on our hardware for any WGSL port (FSR 1, NIS,
  optical flow).
- Digital Foundry's GTA 6 and Lossless Scaling articles were not readable.
  Their figures here are second-hand.
- Whether the FidelityFX Optical Flow shaders need subgroup sizes that some
  wgpu backends (Metal, D3D12 on Intel) do not guarantee.
- RIFE's cost on any AMD GPU.

## Sources

The research passes' full notes, clones and URLs were working files of this
session and are not kept. The primary sources this note rests on are linked
inline above. Others it rests on:
[afmf-linux](https://github.com/serialexperimentslainnnn/afmf-linux) (MIT),
[openfg](https://github.com/sky-is-green/openfg) (MIT),
[FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK),
[AMF](https://github.com/GPUOpen-LibrariesAndSDKs/AMF),
[NVIDIA Image Scaling](https://github.com/NVIDIAGameWorks/NVIDIAImageScaling),
[lsfg-vk](https://lsfg-vk.dev/),
[Practical-RIFE](https://github.com/hzwer/Practical-RIFE),
[rife-ncnn-vulkan](https://github.com/nihui/rife-ncnn-vulkan).
