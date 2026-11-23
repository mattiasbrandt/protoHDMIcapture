# NVIDIA GPUs: frame generation, upscaling, the dashboard and testing without hardware

Research note, 2026-11-24. The question: what would it take to run the GPU
enhancements (#63, #71, #72, #77) on NVIDIA cards, which the user does not
own, and what can be prepared now so a tester can run it later. Nothing was
run on an NVIDIA card or built here. The AMD side is in
[`frame-generation.md`](frame-generation.md), [`upscaling-modern-monitors.md`](upscaling-modern-monitors.md)
and [`frame-generation-plan.md`](../frame-generation-plan.md).

Verification key: **[fetched]** = read at the source this session (page,
repo file, cloned source, PDF text); **[snippet]** = search-result text only;
**[unverified]** = not confirmed; **[derived]** = our own reasoning or arithmetic.

## In short

- **Frame generation on NVIDIA needs no NVIDIA SDK, and the WGSL route
  already covers it.** Our ffx-quality and RIFE ports are vendor-neutral
  WGSL compute, so they run on NVIDIA through wgpu's Vulkan backend with no
  code change [derived]. That is the cheapest NVIDIA path and the one to
  build toward. Its cost on NVIDIA is unmeasured.
- **NVIDIA's hardware 2x interpolator is reachable but awkward.**
  `VK_NV_optical_flow` (Optical Flow SDK 5.0's Vulkan interface) works on
  Linux, but NVIDIA's own app note says the Vulkan interface does not support
  Optical Flow mode on Turing, so it needs Ampere or later [fetched]. wgpu
  cannot enable the extension through its public API; wgpu-hal's device
  callback can add it [fetched source, not built]. The NvOFFRUC library is
  a true 2x frame-rate up-converter, but it is CUDA or DX11 only [fetched],
  so it would mean CUDA interop from wgpu.
- **DLSS Frame Generation, DLSS and Smooth Motion are not usable as
  shipped.** DLSS-G needs the game's depth, motion vectors and HUD-less
  colour [snippet]. **Corrected 2026-11-24:** "needs motion vectors" does
  not mean "out": community tools run DLSS 5 without game integration by
  feeding *estimated* motion vectors, one of them from NVIDIA Optical Flow
  on the picture alone (lower quality by its own account); see 2c.
  Smooth Motion is a driver-level Vulkan present layer for games, with no
  app-side API [fetched, NVIDIA's Linux README]; it could interfere with our
  presentation if a user has it on, so it is a test item, not a feature.
- **Upscaling on NVIDIA is NIS, which is already planned.** NIS is MIT and
  vendor-neutral (#71) [fetched]. RTX Video Super Resolution is not usable
  on Linux as far as any source shows, and its SDK lists Windows only, with
  no licence text read [fetched SDK page, unverified licence].
- **The dashboard can read NVIDIA through NVML, not DRM fdinfo.** The NVIDIA
  kernel module does not set the DRM fdinfo engine counters, so the per-process
  busy number has to come from NVML [fetched source]. nvtop, GPL-3.0, shows the
  calls; we would write our own dlopen bindings from NVML's documented API.
- **GameMode's NVIDIA knobs run through `nvidia-settings` on an X display,
  with Coolbits.** On a Wayland-only session they probably do not apply
  [derived from source; Wayland unverified]. The docs should say so before
  anyone expects them to work.
- **No NVIDIA hardware is needed to prepare most of this.** The test gap is
  real: GitHub's hosted GPU runners are Tesla T4, which is Turing and so
  cannot run the Vulkan optical-flow path [fetched, snippet]. A tester with
  an Ampere-or-newer card is needed for that path.

## 1. Frame generation without game data

### 1a. `VK_NV_optical_flow` (Vulkan)

- **Spec.** Device extension 465, revision 1, status "not ratified", depends on
  `VK_KHR_get_physical_device_properties2` plus Vulkan 1.1 (or Vulkan 1.3),
  `VK_KHR_format_feature_flags2` and `VK_KHR_synchronization2` [fetched,
  Khronos `vk.xml`]. Adds the formats `VK_FORMAT_R16G16_S10_5_NV` and
  `VK_FORMAT_R16G16_SFIXED5_NV` [fetched, docs.vulkan.org].
- **Hardware.** NVOFA hardware is "supported for all Turing GPUs (except
  TU117) and above"; "The Vulkan interface does not support Optical Flow mode
  for GPUs based on the Turing architecture"; the Vulkan interface "adds
  support" in SDK 5.0, and is not available on WSL [fetched, NVOFA application
  note]. ROI and disparity range are "Available on Ampere and later GPUs"
  [fetched, NVOFA programming guide]. So the Vulkan optical-flow route means
  Ampere or later [derived]. Blackwell and Ada support is [unverified] beyond
  that statement.
- **Platforms.** "The NVOF APIs are supported on Windows (Windows 7 and above)
  and Linux" [fetched]. Vulkan on Linux and Windows 10+ [fetched].
- **Driver.** The SDK 5.0 note says "supported on R525 drivers and above"
  [fetched]. Other pages give 520.56.06 or 525.85.05 for Linux [snippet]; the
  R525 figure is the one NVIDIA's note states. Use R525+ and check the release
  notes [unverified].
- **Use.** NVIDIA advises apps to use the SDK's Vulkan API
  (`nvCreateOpticalFlowVk`), not the raw extension [snippet; the page was not
  fetched, so the advice is [unverified]]. The NVOFA guide names
  `VK_KHR_timeline_semaphore` and `VK_NV_optical_flow` as required, and Vulkan
  1.3 [fetched].
- **Can wgpu reach it?** The public wgpu API does not request arbitrary
  device extensions (not confirmed in the public API surface [unverified]).
  wgpu-hal 30.0.2 does: `Adapter::open_with_callback` gives the callback
  `extensions: &mut Vec<&CStr>`, which it may add to, and a `device_features`
  and pNext chain it may extend [fetched, `wgpu-hal-30.0.2/src/vulkan/mod.rs`
  and `adapter.rs` ~L2995]. The path then needs the NVOF feature struct in
  pNext, raw `ash` calls for sessions and execution, and a Vulkan `VkDevice`
  shared with the window's wgpu device. Feasible in principle; nothing built.
- **Cost on our hardware.** Not published. The NVOF flow is hardware, so it
  takes no shader time. The added cost is the blend pass, which we would write
  in WGSL either way [derived].

### 1b. NVIDIA FRUC (NvOFFRUC, libNvFRUC)

- **What it does.** "interpolates new frames using optical flow vectors to
  double the effective frame rate of a video" [snippet, SDK 4.0 blog]. It is
  the 2x video interpolator NVIDIA ships for this case [derived].
- **Platforms and files.** Windows: `NvFRUC.dll`; Linux (Ubuntu 18+):
  `libNvFRUC.so` [fetched, FRUC programming guide]. Driver 511.65+ on Windows,
  510.47.03+ on Linux [fetched].
- **APIs.** Windows: DirectX 11 shared textures via CUDA-DX interop; CUDA
  driver API with `cuDevicePtr` or `cuArray` resources. "DirectX 12 and Vulkan:
  not mentioned for FRUC" [fetched]. Input surfaces NV12 or ARGB [fetched].
- **Verdict.** On Linux, the only API is CUDA device memory. A wgpu Vulkan
  frame would need an external-memory export to CUDA, and CUDA would have to
  be loaded at run time. That is a large dependency for one 2x path, and it
  duplicates the WGSL route. Not recommended now [derived].
- **Licence.** The Optical Flow SDK GitHub repository has no licence
  (`license: null`, last push 2021-07-07) and its README points to the
  download page [fetched]. The app note links an "NVIDIA DesignWorks SDK
  License", which was not read [fetched link text]. The NVIDIA RTX SDKs
  Licence (23 Jan 2023), clause e, says "You may not use the SDK in any manner
  that would cause it to become subject to an open source software license"
  [fetched PDF text]. Whether the Optical Flow SDK is under that licence is
  **[unverified]**. Until it is read, the existing claim that the SDK cannot
  ship with an open-source app stands on this clause alone.

### 1c. DLSS Frame Generation

Out of scope. DLSS-G takes depth, motion vectors and HUD-less colour and UI
colour, passed through Streamline [snippet, NVIDIA developer blogs]. A capture
card provides none of them [derived from [frame-generation.md](frame-generation.md)].

### 1d. Smooth Motion (driver level)

- **What it is.** A driver-level frame generation for RTX 40 and newer
  [snippet, NVIDIA]. On Linux: "The feature supports x86_64 Vulkan
  applications" and "targets ... GeForce RTX 40 Series and newer GPUs"; it
  "applies to games that don't use DLSS Frame Generation"; it is enabled with
  `NVPRESENT_ENABLE_SMOOTH_MOTION=1` at game launch [fetched, NVIDIA's Linux
  driver README, `nvpresent.html`, 580.82.07].
- **Linux history.** RTX 50 support from 575.51.02 (beta) and RTX 40 from
  580.82.07 [snippet, GamingOnLinux, Ubuntu Handbook].
- **Caveats in the same README.** The layer presents from an asynchronous
  compute queue, which "can cause issues with third party overlays"; setting
  `NVPRESENT_QUEUE_FAMILY=1` presents from a graphics queue instead, at a
  cost [fetched].
- **App triggering.** The only documented hook is the environment variable,
  read by the layer at load [fetched]. Whether it takes effect for a process
  that sets it after start is **[unverified]**. An app cannot enable it through
  an API [derived].
- **Windows.** Driver-level for DX11/DX12, works "in any game" [snippet, Tom's
  Hardware]. Not relevant to us except as a user setting.
- **Verdict.** Do not integrate. Treat "Smooth Motion on for protoHDMIcapture"
  as a tester check: if it is on, the frame pacing numbers are not ours.

## 2. Upscaling on NVIDIA

### 2a. NVIDIA Image Scaling (NIS, our port in #71)

- **Licence and content.** MIT, SDK v1.0.3; NVScaler is "a 6-tap scaling filter
  combined with 4 directional scaling and adaptive sharpening filters" in one
  pass [fetched, `NVIDIAGameWorks/NVIDIAImageScaling` README and licence
  file]. The repository's last push was 2022-08-22 [fetched].
- **NVIDIA's own guidance.** Use NVScaler or NVSharpen, not both: "should NOT
  integrate NVSharpen, as NVScaler already includes a sharpening pass"
  [fetched]. Apply it in linear light with care, and put film grain after it
  [fetched]. Set `NIS_HDR_MODE` only for HDR input [fetched]. There is no
  recommended sharpness value [fetched, as in our existing note].
- **Vendor.** The fp32 path is vendor-neutral [fetched, existing note]. Nothing
  NVIDIA-specific is needed; a tester run only confirms the output matches
  NVIDIA's reference shader.

### 2b. RTX Video Super Resolution and the RTX Video SDK

- **What the SDK page says.** Features: "RTX Video Super Resolution" and "RTX
  Video HDR". "The RTX Video model is optimized to run using the powerful Tensor
  Cores found on NVIDIA RTX GPUs". No OS, API, version or licence statement on
  that page [fetched].
- **APIs and GPUs.** The getting-started page lists "API support: DX11, DX12,
  Vulcan, CUDA" (sic), "64-bit Win10+" as the only platform, and "GeForce RTX
  20 series or newer (Turing)"; SDK 1.1 adds RTX 50 [fetched]. An earlier NVIDIA
  blog says Windows 10 and 11 only [snippet].
- **Linux.** No source found says the SDK runs on Linux [unverified, absence
  of evidence]. A forum thread says VSR is missing on Linux [snippet]. NVIDIA's
  Maxine Video Super Resolution is a separate product with a Linux package
  [snippet]; its licence is the NVIDIA software licence, not the RTX Video SDK
  one [snippet, unverified].
- **Licence.** The RTX Video SDK licence was not found [unverified]. The RTX
  SDKs licence clause in 1a applies to the RTX family's terms.
- **Verdict.** Not for the Linux viewer. On Windows, a non-browser app could
  use the SDK, but only after its licence is read and only with a DX or Vulkan
  path; not worth it while FRUC and the WGSL route exist [derived].

### 2c. DLSS upscaling

Needs motion vectors (and colour; depth for some features) from the engine
[snippet]. NVIDIA's developer material for DLSS 5 says it takes each frame's
colour and motion vectors, and NVIDIA has said there will be no driver-level
DLSS 5 override, because a driver cannot supply that data [snippet].

**Corrected 2026-11-24 (the user's challenge):** the requirement is motion
vectors, not the game itself. Community projects run DLSS 5 in games that
never shipped it by substituting estimated inputs [snippet, READMEs]:

- NIGos/dlss5-bridge (a ReShade add-on): DLSS 5 in D3D11 and Vulkan games,
  and "optionally in games with no DLSS via NVIDIA Optical Flow (lower
  quality)": motion estimated from the picture alone.
- jlrouzies-fr/DLSS5-Feeder: a synthetic input set from ReShade's depth
  buffer and estimated motion vectors; notes temporal artefacts in fast
  motion.

Both run inside the game's process. For a capture viewer the relevant one is
the optical-flow path: it needs no game. **We already estimate motion**
(ffx-quality's per-pixel flow), so a temporal upscaler can in principle be
fed our motion field: DLSS super resolution on NVIDIA (#88), and, vendor
neutral and testable here, AMD FSR 2/3 upscaling (MIT). Open: faking depth
(flat), disocclusion without depth, and whether our flow's known failures
(building windows, signs) make the temporal result worse than a spatial
upscaler. Unverified; a spike, not a plan.

## 3. Our code on NVIDIA

What the code does now, from the source:

- **Adapter and fallback.** `score.rs` requests a high-performance adapter with
  no surface and `SHADER_F16` only if the adapter has it [fetched, `src/score.rs`
  ~L265–L275]. `gpustat::source_for` reads whole-GPU numbers for vendor `0x1002`
  on Linux only; NVIDIA gets "whole-GPU numbers are read for amdgpu on Linux
  only so far" [fetched, `src/gpustat.rs`].
- **Timestamps.** `gputime.rs` returns `None` without `TIMESTAMP_QUERY`
  [fetched]. NVIDIA's Vulkan support for it is **[unverified]**; the code
  already degrades.
- **Half floats.** wgpu maps `SHADER_F16` to `VK_KHR_shader_float16_int8` and
  the 16-bit storage extension on Vulkan [fetched, wgpu-hal 30.0.2
  `adapter.rs` ~L437, ~L1254–L1296]. The RIFE branch compiles an f16 variant
  when the device has it [fetched, branch `74-fg-rife`, `rife.wgsl`]. NVIDIA's
  f16 throughput for non-tensor shader code is **[unverified]**; it must be
  measured. A Linux-NVIDIA Dawn note says f16 was added [snippet].
- **Subgroups.** wgpu maps `SUBGROUP` from the Vulkan subgroup operation
  flags [fetched, `adapter.rs` ~L897–L922]. Our ffx port uses no wave
  operations: "workgroup atomics and barriers do it, so the port runs on any
  GPU wgpu drives" [fetched, branch `73-ffx-quality`, `src/framegen/ffx.rs`].
  So subgroup size does not gate it.
- **Subgroup size on NVIDIA.** One NVIDIA-forum report says the Blackwell Vulkan
  driver (595.45.04, RTX 5080/5090) reports min and max subgroup size 32, and
  that Ada reported min 16 [snippet, user report, unverified]. Warp size is 32
  [snippet]. Our workgroups are all multiples of 32 threads (8×8, 16×4, 64×1,
  256×1, 32×8, 4×4×4) [fetched, branch sources], so they map onto whole warps
  on NVIDIA [derived].
- **Cooperative matrix.** wgpu 30 has `EXPERIMENTAL_COOPERATIVE_MATRIX`, native
  only [fetched, `wgpu-types` 30.0.2 `features.rs` L1423–L1425]. It is the way
  to reach tensor cores from WGSL. Not tested here; a RIFE experiment on
  NVIDIA is a candidate [derived].
- **Where the WGSL lives.** On `main`, no `.wgsl` file exists yet; the ffx and
  RIFE shaders are on branches `73-ffx-quality` and `74-fg-rife`, and
  `fg-judge` [fetched].

### Expected timings by GPU class [derived, low confidence]

The anchor is our measurement on RX 9070 XT, the only card measured: the
ML upscaler takes 7.2 ms back to back and 12.4 ms paced at 1080p, f16
[fetched, [upscaling-modern-monitors.md](upscaling-modern-monitors.md)]; the
afmf-linux optical flow takes 0.56–0.61 ms per generated frame at 3440×1440
[fetched, existing note]. Scale by FP32 peak, a crude proxy for compute-bound
kernels:

| GPU | FP32 peak (source) | Ratio to RX 9070 XT (48.7 TFLOPS [snippet, AMD page]) | Expected ML upscaler, paced, 1080p | Confidence |
|---|---|---|---|---|
| RX 9070 XT (measured) | 48.7 TFLOPS [snippet] | 1.0 | 12.4 ms (measured) | measured |
| RTX 5070 Ti | 43.9 TFLOPS [snippet, Puget and HMC; Flopper gives 44] | 0.90 | ≈ 13–14 ms | low |
| Other NVIDIA cards | not fetched (TechPowerUp returned 403) | — | — | none |

NVIDIA's advantage on tensor-core paths is not in this table, and its
per-kernel behaviour on our kernels is unmeasured. Treat the table as a
sanity bound, not a forecast.

## 4. The dashboard's GPU section on NVIDIA

### 4a. NVML: calls and loading

- **Loading.** nvtop dlopens `libnvidia-ml.so.1` first, then falls back
  [fetched, `src/extract_gpuinfo_nvidia.c` ~L754–L770, ~L954]. Symbols are
  resolved with `dlsym`, trying `_v2` names first [fetched].
- **Calls nvtop uses** (names as in its source): `nvmlDeviceGetHandleByIndex`,
  `nvmlDeviceGetName`, `nvmlDeviceGetUtilizationRates` (busy %),
  `nvmlDeviceGetClockInfo` (SM and memory clocks), `nvmlDeviceGetMemoryInfo`
  (VRAM used and total), `nvmlDeviceGetPowerUsage` (mW),
  `nvmlDeviceGetTemperature` and `nvmlDeviceGetTemperatureThreshold` (slowdown,
  shutdown, GPU max), `nvmlDeviceGetFanSpeed` and `_v2`,
  `nvmlDeviceGetProcessUtilization` (per-process SM samples), and
  `nvmlDeviceGetEncoderUtilization` [fetched]. The power limit call
  (`nvmlDeviceGetPowerManagementLimit`) is not in nvtop's list [unverified].
- **How nvtop reads per-process use.** `nvmlDeviceGetProcessUtilization` with
  a last-seen timestamp [fetched]; filtering the samples by PID is
  **[unverified]** (not read in the source).
- **Licence.** nvtop is GPL-3.0 [fetched, `LICENSE`]. We may not copy its code.
  We write our own bindings from NVML's documented signatures.
- **NVML header licence.** The header's NVIDIA notice grants "a nonexclusive,
  royalty-free license to use this code in individual and commercial
  software", disclaims warranties, and requires the disclaimer and U.S.
  Government notice in documentation [fetched, paraphrase of `nvml.h` from the
  nvidia-settings tree]. Redistribution of the header itself is **[unverified]**.
  Plan: do not vendor `nvml.h`; declare the few types we need and dlopen the
  library at run time, so no NVIDIA file ships with the app [derived].

### 4b. DRM fdinfo on NVIDIA

- **Kernel side.** `drm_show_fdinfo` prints `drm-driver`, `drm-client-id` and
  `drm-pdev`, then calls the driver's own `show_fdinfo` hook if it has one
  [fetched, torvalds/linux `drivers/gpu/drm/drm_file.c`]. The engine counters
  (`drm-engine-*`) come only from that driver hook, and `gpustat::process()`
  reads those keys [fetched, `src/gpustat.rs` L55–L95].
- **NVIDIA open module.** Sparse clone of `NVIDIA/open-gpu-kernel-modules` at
  `a4b95f6` (2026-11-18, driver 620.73.04): `kernel-open/` contains no
  `fdinfo`, `drm-engine` or `drm-memory` reference [fetched]. So the open module
  does not provide per-client engine time [derived].
- **Proprietary module.** Not inspectable here [unverified]. The open module is
  the best evidence available.
- **Consequence.** On NVIDIA, `process()` finds no engine counters, so
  `counted` is false and the section must use NVML for both the card and the
  process [derived].

### 4c. Dashboard plan

A second `Source` in `gpustat.rs` for vendor `0x10de` (the seam is already
there, [fetched]), reading NVML, returning `Card` fields; `process` comes from
NVML's per-process samples. If NVML is missing it returns the existing "not
available" text. Testable here: the not-found path, and parsing of the
`Card` struct from fixture numbers. Real readings wait for a tester.

## 5. GameMode on NVIDIA

- **How it applies.** `game_mode_apply_gpu` runs `pkexec gpuclockctl <device>
  set <core> <mem> <powermizer> <per-profile>` for NVIDIA [fetched,
  `daemon/gamemode-gpu.c`]. `gpuclockctl` sets `GPUGraphicsClockOffset`,
  `GPUMemoryTransferRateOffset` and `GPUPowerMizerMode` through
  `nvidia-settings -a` [fetched, `util/gpuclockctl.c`].
- **Requirements.** It needs `DISPLAY` and `XAUTHORITY`: "Setting Nvidia
  parameters requires DISPLAY and XAUTHORITY to be set" [fetched,
  `gpuclockctl.c` L297–L299]. GameMode's example config says the clock offsets
  "Requires the coolbits extension activated in nvidia-xconfig" [fetched,
  `example/gamemode.ini`].
- **Options** (from the example file, names checked against
  `gamemode-config.c`): `apply_gpu_optimisations=accept-responsibility`,
  `gpu_device=<n>`, `nv_powermizer_mode` (0 Adaptive, 1 Prefer Maximum
  Performance, 2 Auto), `nv_core_clock_mhz_offset`, `nv_mem_clock_mhz_offset`
  (the memory key is `nv_mem_clock_mhz_offset`, not `nv_mem_clock_offset`),
  `nv_per_profile_editable` [fetched].
- **Where it is read.** `[gpu]` is refused from user files ("not configurable
  from unsafe config files") and accepted from the shipped and `/etc` config
  [fetched, `gamemode-config.c`, `load_protected`]. This matches
  [testing.md](../testing.md#gamemode-77): only `/etc/gamemode.ini` is realistic.
- **Wayland.** `nvidia-settings` is an X client. Under a Wayland session it
  works only through XWayland, and whether NV-CONTROL answers there is
  **[unverified]**. Expect the NVIDIA clock and PowerMizer knobs not to apply
  on a Wayland-only session [derived]. The CPU part of GameMode (governor,
  renice) does not depend on X.
- **What the docs should say** (#77 section): NVIDIA GPU optimisation needs an
  X display, Coolbits and the user's own `/etc/gamemode.ini`; it is off by
  default; on Wayland it may do nothing; protoHDMIcapture never writes NVIDIA
  settings. No code change.

## 6. Testing without owning the hardware

### 6a. Options

- **GitHub-hosted GPU runners.** Generally available for Linux and Windows,
  one configuration: 4 vCPU, one Tesla T4, 16 GB VRAM [snippet, GitHub docs and
  changelog 2024-07-08]. Larger runners need a Team or Enterprise plan [snippet].
  The T4 is Turing, so it cannot run the Vulkan optical-flow path (1a) but can
  run NIS, ffx-quality, RIFE, NVML reads and the timing tests [derived].
- **A tester with an Ampere-or-newer card** (RTX 30, 40 or 50). The only
  option for `VK_NV_optical_flow` and Smooth Motion checks [derived]. The
  user's network is the cheapest source.
- **Cloud GPU instances.** Plausible; no provider or price was checked
  [unverified]. Any instance needs an NVIDIA driver with Vulkan; no display is
  needed for the harness, because `score.rs` runs with no display handle
  [fetched].
- **Lavapipe (Mesa, CPU Vulkan).** Vulkan 1.3 and some subgroup operations
  [snippet, Mesa release notes]. It proves the WGSL compiles and the harness
  runs in CI with no GPU. It says nothing about timing or NVIDIA behaviour
  [derived]. Whether it provides `SHADER_F16` and timestamps is
  **[unverified]**.

### 6b. What the headless tests need

- `score` (headless, no surface): one adapter, optional f16 [fetched].
- GPU timing (`gputime`): `TIMESTAMP_QUERY` or it reports "no timestamps"
  [fetched].
- Both would run on a T4 runner unchanged, if the runner has the driver and
  Vulkan ICD installed [unverified: not run].

## Recommendations, ranked

| # | What | Why | Cost | Now (no hardware) or waits |
|---|---|---|---|---|
| 1 | Keep the vendor-neutral WGSL path as the NVIDIA frame-generation and upscaling route. Do not add an NVIDIA SDK. | Runs on any wgpu Vulkan GPU; no licence question; already the plan. | None extra | Now (nothing to build beyond #66, #71) |
| 2 | Name the GPU, driver and display server (X or Wayland) in `--stats`, `score` and the dashboard, and mark NVIDIA "not yet measured". | Keeps tester numbers attributable; cheap. | Small | Now |
| 3 | Add an NVML `Source` for vendor `0x10de` in `gpustat.rs`: own dlopen bindings, no vendored header, "not available" on failure. | The dashboard section promises NVIDIA parity with amdgpu, and NVML is the only route. | Medium, one slice | Written and unit-tested now; numbers wait for a tester |
| 4 | Write the GameMode NVIDIA caveats into the #77 section of [testing.md](../testing.md#gamemode-77) and the README: X, Coolbits, `/etc`, Wayland unknown. | Stops a false expectation; no code. | Small | Now |
| 5 | Read the Optical Flow SDK and RTX Video SDK licences from NVIDIA's download pages before any NVIDIA binary or header is bundled. | The licence clause in 1a could block any SDK use. | Small | Now, needs an NVIDIA developer login [unverified] |
| 6 | Tester run plan for NVIDIA: what to run, what to capture. A plan, not a run. | Lets the user line up a tester without a run being scheduled. | Small | Now (write-up); run waits |
| 7 | Spike `VK_NV_optical_flow` on Ampere+ through wgpu-hal's device callback, behind a dev flag. | Only NVIDIA hardware 2x path with no CUDA; big unknowns (raw Vulkan interop, licence). | Large | Waits for an Ampere tester and for the AMD ffx result |
| 8 | Optional lavapipe job for `score` and the timing harness in CI. | Cheap compile and harness coverage on any runner. | Small, changes CI | Ask first |
| - | Not pursued: NvOFFRUC (CUDA-only on Linux), DLSS, DLSS-G, Smooth Motion as a feature, RTX Video SDK. | Each has a blocker in the sources above. | — | — |

## Proposed tickets

1. **NVIDIA GPU section via NVML (dlopen, no vendored header).** Add a
   `Source` for vendor `0x10de` in `src/gpustat.rs` that loads
   `libnvidia-ml.so.1` at run time with our own declarations of the few NVML
   calls the dashboard needs: busy %, SM and memory clocks, VRAM, power,
   temperatures and their thresholds, fan, and per-process utilisation. If the
   library is missing or a call fails, the section says "not available" as it
   does for other drivers. Test here: the not-found path, and the `Card` mapping
   from fixed numbers. Real readings are for a tester. Do not copy nvtop
   (GPL-3.0).

2. **Name the GPU and display server in results.** `--stats`, `score` and the
   dashboard print vendor, driver version and session type (X11 or Wayland), and
   mark a card "not yet measured" until a run exists. Lets tester numbers be
   compared with AMD ones without guessing.

3. **GameMode NVIDIA: document the requirements.** In the #77 section of
   `docs/testing.md` and the README's GPU opt-in: NVIDIA clock and PowerMizer
   settings need an X display (XWayland is unverified), Coolbits, and the
   user's own `/etc/gamemode.ini`; protoHDMIcapture never writes them. No code.

4. **Licence read before any NVIDIA SDK.** Read the Optical Flow SDK and RTX
   Video SDK licence texts from NVIDIA's download pages and record the clauses
   in this note. Blocks any NVIDIA binary, header or SDK use until done.

5. **Spike: `VK_NV_optical_flow` on Ampere and newer (dev flag, deferred).**
   Enable the extension on the window's wgpu device through wgpu-hal's
   `open_with_callback`, create an NVOF session with the SDK's Vulkan
   interface, and time one generated frame at 1080p. Behind a dev flag only.
   Start only if the AMD ffx result falls short and an Ampere tester is found.

6. **NVIDIA tester run plan (plan only, no run).** A one-page checklist for the
   user's tester: card, driver, session type; `--stats` with an enhancement on;
   `score`; NIS output compared with the reference shader; ffx and RIFE
   timings; Smooth Motion off, then on; the dashboard's NVML section. The run
   is scheduled separately, with the card's time and setup stated, as
   the user's no-unasked-hardware-runs rule requires.

7. **Optional: lavapipe headless job.** Add a CI job running `score` and the
   timing tests on lavapipe, to catch compile and harness breakage on every
   change. Changes CI, so it needs approval first.

## Claims I could not verify

- Whether the Optical Flow SDK and the RTX Video SDK are under the RTX SDKs
  licence clause (1a, 2b). Their own licence texts were not read.
- Whether the NVOF Vulkan interface works on Blackwell and Ada, and the exact
  minimum driver for Linux (R525 per NVIDIA's note; 520.56.06 and 525.85.05
  elsewhere).
- Whether NVIDIA advises against using `VK_NV_optical_flow` directly (snippet
  only; the page was not fetched).
- Whether wgpu's public API can request extra device extensions, without the
  hal callback.
- Whether the Smooth Motion layer reads its environment variable after
  process start, and whether it affects our window.
- Whether NV-CONTROL attributes work under XWayland for GameMode's clock
  settings.
- NVIDIA's f16 throughput and timestamp support on Vulkan; subgroup size 32 on
  current NVIDIA drivers (one forum report, conflicting).
- NVML's power-limit call and the header's redistribution terms.
- Tensor-core throughput for RIFE through `EXPERIMENTAL_COOPERATIVE_MATRIX`.
- FP32 peaks for NVIDIA cards other than the RTX 5070 Ti (TechPowerUp returned
  403).
- Lavapipe's f16 and timestamp support, and whether it runs the harnesses.
- Cloud GPU availability and prices; the GitHub runner plan requirement beyond
  the snippet.
- Whether the proprietary NVIDIA kernel module exposes DRM fdinfo engine
  counters (the open module does not).

## Sources

Primary (fetched this session):
- Khronos `vk.xml`, `VK_NV_optical_flow`: https://github.com/KhronosGroup/Vulkan-Docs/blob/main/xml/vk.xml ; spec page https://docs.vulkan.org/refpages/latest/refpages/source/VK_NV_optical_flow.html
- NVIDIA Optical Flow SDK 5.0, NVOFA application note: https://docs.nvidia.com/video-technologies/optical-flow-sdk/nvofa-application-note
- NVIDIA Optical Flow SDK 5.0, NVOFA programming guide: https://docs.nvidia.com/video-technologies/optical-flow-sdk/nvofa-programming-guide/index.html
- NVIDIA Optical Flow SDK 5.0, FRUC programming guide: https://docs.nvidia.com/video-technologies/optical-flow-sdk/nvfruc-programming-guide/index.html
- NVIDIA Optical Flow SDK, repository README: https://github.com/NVIDIA/NVIDIAOpticalFlowSDK
- NVIDIA RTX SDKs Licence (23 Jan 2023), clause e: https://developer.download.nvidia.com/gameworks/NVIDIA-RTX-SDKs-License-23Jan2023.pdf
- NVIDIA RTX Video SDK page: https://developer.nvidia.com/rtx-video-sdk ; getting started: https://developer.nvidia.com/rtx-video-sdk/getting-started
- NVIDIA Linux driver README, Smooth Motion chapter (580.82.07): https://download.nvidia.com/XFree86/Linux-x86_64/580.82.07/README/nvpresent.html
- NVIDIA open GPU kernel modules, commit a4b95f6 (sparse clone): https://github.com/NVIDIA/open-gpu-kernel-modules
- NVIDIA Image Scaling, README and licence: https://github.com/NVIDIAGameWorks/NVIDIAImageScaling
- NVML header (nvidia-settings tree): https://cgit.freedesktop.org/~aplattner/nvidia-settings/plain/src/nvml.h?h=450
- nvtop source (`extract_gpuinfo_nvidia.c`, `LICENSE`): https://github.com/Syllo/nvtop
- GameMode: `daemon/gamemode-gpu.c`, `daemon/gamemode-config.c`, `util/gpuclockctl.c`, `example/gamemode.ini`: https://github.com/FeralInteractive/gamemode
- Linux kernel DRM: `drivers/gpu/drm/drm_file.c`: https://github.com/torvalds/linux
- wgpu 30.0.2 source (local cargo registry): `wgpu-hal` `src/vulkan/adapter.rs`, `src/vulkan/mod.rs`; `wgpu-types` `src/features.rs`, `src/adapter.rs`: https://github.com/gfx-rs/wgpu
- Repo sources read: `src/gpustat.rs`, `src/score.rs`, `src/gputime.rs` (main); `src/framegen/ffx.rs` and `src/framegen/rife/rife.wgsl` (branches `73-ffx-quality`, `74-fg-rife`); `docs/testing.md`, `docs/frame-generation-plan.md`, `docs/research/frame-generation.md`, `docs/research/upscaling-modern-monitors.md`.
- NVIDIA RTX 5070 Ti specs, via search: https://www.pugetsystems.com/parts/Video-Card/NVIDIA-GeForce-RTX-5070-Ti-16GB-16125/ ; https://hmc-tech.com/gpus/nvidia-geforce-rtx-5070-ti

Secondary (search snippets only):
- Smooth Motion Linux timeline: https://www.gamingonlinux.com/2025/04/nvidia-575-51-02-beta-driver-brings-nvidia-smooth-motion-support-to-linux-geforce-rtx-5060-series-announced ; https://ubuntuhandbook.org/index.php/2025/09/nvidia-580-82-07-added-smooth-motion-for-rtx-40-series-gpus/
- NvOFFRUC 2x interpolation and platforms: https://developer.nvidia.com/blog/harnessing-the-nvidia-ada-architecture-for-frame-rate-up-conversion-in-the-nvidia-optical-flow-sdk
- DLSS-G inputs: https://developer.nvidia.com/blog/how-to-integrate-nvidia-dlss-4-into-your-game-with-nvidia-streamline ; https://developer.nvidia.com/rtx/streamline/get-started
- Subgroup-size report (Blackwell, driver 595.45.04): https://forums.developer.nvidia.com/t/blackwell-vulkan-driver-missing-wavesize-16-support-that-windows-d3d12-driver-provides/364204
- GitHub GPU runners: https://github.blog/changelog/2024-07-08-github-actions-gpu-hosted-runners-are-now-generally-available ; https://docs.github.com/en/actions/reference/runners/larger-runners
- Lavapipe: https://docs.mesa3d.org/relnotes/21.3.0.html ; https://docs.mesa3d.org/relnotes/22.2.0.html
- RX 9070 XT FP32 48.7 TFLOPS: https://www.amd.com/en/products/graphics/desktops/radeon/9000-series/amd-radeon-rx-9070XT.html
- NVIDIA Maxine Video Super Resolution (separate product): https://catalog.ngc.nvidia.com/orgs/nvidia/teams/maxine/collections/nvvfxvideosuperres
