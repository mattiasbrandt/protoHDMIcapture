# Graphics, window and UI: Rust practice vs protoHDMIcapture

Research note, 2026-11-17: modern Rust practice (2025-2026) against
protoHDMIcapture, one of three ([concurrency](rust-concurrency.md),
[graphics](rust-graphics.md), [capture and audio](rust-capture-audio.md)).
Findings, not decisions: each verdict is a proposal for the grilling and
the issues that follow. Line numbers are from `14a5301`. Nothing here was
measured on a card; every "measure first" still needs its run
([`docs/testing.md`](../testing.md)).

Legend: **keep** (no change), **consider** (experiment first, adopt only if measured better), **change** (do it). `[verified]` = read at a primary source this session (docs, or the pinned crate source in `~/.cargo/registry`). `[secondary]` = search summary or third-party page, not opened. `[unverified]` = not checked here.

## Versions: pinned vs latest published

| Crate | Pinned (Cargo.lock) | Latest seen | Source |
|---|---|---|---|
| winit | 0.30.13 (`0.30`) | **0.30.13** is the latest stable (2026-03-02). 0.31.0-beta.4 (2026-10-24) is the newest pre-release; no 0.31.0 final. | [releases](https://github.com/rust-windowing/winit/releases), [crate](https://docs.rs/crate/winit/latest) `[verified]` |
| wgpu | 30.0.1 (`30`) | **30.0.2** (2026-11-12). No 31.x. | [crate](https://docs.rs/crate/wgpu/latest) `[verified]` |
| egui, egui-wgpu | 0.36.2 | **egui-wgpu 0.37.0** (2026-10-15), needs `egui 0.37.0` and `wgpu "30.0"`. | [egui-wgpu crate](https://docs.rs/crate/egui-wgpu/latest), [0.37.0 Cargo.toml](https://docs.rs/crate/egui-wgpu/0.37.0/source/Cargo.toml) `[verified]` |
| wayland-client | 0.31.15 | 0.31.15 (2026-07-22), current. | [crate](https://docs.rs/crate/wayland-client/latest) `[verified]` |
| wayland-protocols | 0.32.13 | 0.32.13 (2026-06-19), current. | [crate](https://docs.rs/crate/wayland-protocols/latest) `[verified]` |
| windows | 0.62.2 | 0.62.2 (2025-10-06) per docs.rs; docs.rs may lag. | [crate](https://docs.rs/crate/windows/latest) `[verified]` |

Upgrade notes:
- **egui 0.36.2 to 0.37.** egui-wgpu 0.37.0 requires `wgpu "30.0"`, the same major as ours, so the egui bump needs no wgpu change (source above). The 0.37.0 `Renderer::new`, `update_buffers`, `render` and `update_texture` signatures match the 0.36.2 ones in the docs `[verified]`. The egui changelog I read (about half of it) lists no breaking change to `run_ui` or the wgpu renderer `[verified in part]`. Cost: small, and the overlay needs a look.
- **wgpu 30.0.1 to 30.0.2.** Consider, not change. A secondary summary of the wgpu changelog says the 30.0.1 and 30.0.2 entries touch none of the topics here `[secondary]`. I did not read the 30.0.2 entries themselves. Read them before bumping. Cost: `cargo update -p wgpu` and a build.
- **winit 0.31.** Not stable, so not now. Breaking changes from a summary of the releases page `[secondary]`: `ApplicationHandler::user_event` becomes `user_wake_up` and carries no payload; `EventLoopProxy::send_event` becomes `wake_up`; `ActiveEventLoop` and `Window` become traits; `create_window` returns `Box<dyn Window>`; surface creation moves to `can_create_surfaces()` and `can_destroy_surfaces()`; `WindowEvent::Resized` becomes `SurfaceResized`; many enums become `#[non_exhaustive]`. The 0.31.0-beta.4 `Window` is a trait `[verified]` ([docs](https://docs.rs/winit/0.31.0-beta.4/winit/window/trait.Window.html)). Cost here: `UserEvent`/`What` (app.rs:188-200) carries a payload through `user_event`, so the wake would become a flag plus a queue. The thread-local event loop reused across windows (app.rs:414, 449) needs a second look. A real migration, not a find-and-replace.

---

## 1. winit 0.30 ApplicationHandler best practices

**Current best practice**
- Create the window and graphics surface only after the first `resumed`. Winit says some platforms do not allow a render surface before then `[verified]` ([ApplicationHandler](https://docs.rs/winit/0.30.13/winit/application/trait.ApplicationHandler.html)).
- Draw in response to `WindowEvent::RedrawRequested`, requested with `Window::request_redraw`. Winit calls this "the strongly encouraged method of redrawing windows", and merges duplicate requests `[verified]` ([Window](https://docs.rs/winit/0.30.13/winit/window/struct.Window.html), [WindowEvent](https://docs.rs/winit/0.30.13/winit/event/enum.WindowEvent.html)).
- `about_to_wait` is "not an ideal event to drive application rendering from" `[verified]` ([ApplicationHandler](https://docs.rs/winit/0.30.13/winit/application/trait.ApplicationHandler.html)).
- `pre_present_notify`: call after drawing, before presenting. On Wayland it "Schedules a frame callback to throttle `WindowEvent::RedrawRequested`" `[verified]` ([Window](https://docs.rs/winit/0.30.13/winit/window/struct.Window.html)).

**What this codebase does**
- The window and surface are created in `init` (app.rs:601), called from `resumed` (app.rs:1440-1452). This matches the docs.
- `user_event` only marks what came: a frame wake clears `frame_wake` and calls `pacing.arrived()` (app.rs:1463-1495). It does not draw. The comment says why: a 0.9 s draw under WARP left a wake queued ahead of the keys (#18).
- Drawing happens in `about_to_wait` (app.rs:1532-1563): `drain()`, then `render()` if `pacing.wake` says so, then `ControlFlow::WaitUntil` or `Wait`. The comment at app.rs:1541-1545 records that a wake from another path kept frames a refresh behind (55 ms against 41). docs/testing.md has the 2026-10-17 entry where `27c31d3` (draw in `about_to_wait`) matched the baseline.
- `window_event` has no `RedrawRequested` arm (app.rs:1497-1530).
- `pre_present_notify` is called at app.rs:1429.

**Verdict: keep the draw placement; change the `pre_present_notify` call (remove or justify).**

- **Draw placement: keep, with low expected value from an experiment.** Winit's docs prefer `RedrawRequested`, but the measured choice is backed by the repo's own history. The experiment (request a redraw on the frame wake, draw on `RedrawRequested`) is predicted to show no gain. On Wayland, winit's redraw is gated by the same frame callback our pacing already waits on, and the HOLD and overdue rules in `Pacing::wake` (pacing.rs:108-119) would have to move into the handler. That is the path that produced the 55 ms regression. Consider only if there is spare measurement time.

- **`pre_present_notify`: change.** It has no `RedrawRequested` consumer here. In the pinned winit 0.30.13 source, the call only does `request_frame_callback()` (winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:301-303) `[verified]`. When that callback is `done`, winit emits `RedrawRequested` only if a redraw was requested (winit-0.30.13/src/platform_impl/linux/wayland/event_loop/mod.rs:486-505), but it always calls `AboutToWait` after the dispatch (mod.rs:515) `[verified]`. So here the call adds at most one wake per present, and nothing else. Our own frame callback (present/wayland.rs:71) already makes the socket readable once per present (inference from the same wake path; measure). Action: remove the call, then measure with `--stats --seconds 120` against the docs/testing.md baseline (44.9 ms median, p99 53.7, 2 repeated refreshes, 2026-10-16). Keep it only if the numbers or the repeated-refresh count get worse. Trade-off: a future `RedrawRequested` user would need it back.

---

## 2. wgpu 30 frame pacing, present mode, latency

**Current best practice**
- `PresentMode::Fifo` is the mode every platform supports. `Mailbox` replaces a waiting frame with the newest, which lowers latency where it exists. `Immediate` is "Most platforms except older DX12 and Wayland" `[verified]` ([PresentMode](https://docs.rs/wgpu/30.0.1/wgpu/enum.PresentMode.html)).
- `desired_maximum_frame_latency` is a hint. Vulkan uses it as `+1` swapchain images, clamped. DX12 passes it to `IDXGISwapChain2::SetMaximumFrameLatency`. Metal sets `maximumDrawableCount`. The default from `get_default_config` is 2. For Mailbox on DX12 it caps the frame rate at `latency × monitor Hz` `[verified]` ([wgpu-types SurfaceConfiguration 30.0.2](https://docs.rs/wgpu-types/latest/wgpu_types/struct.SurfaceConfiguration.html)).
- DX12's default for the waitable object is `Wait` `[verified]` (wgpu-types-30.0.1/src/backend.rs, `#[default] Wait`). The HAL waits on the handle in `acquire` (wgpu-hal-30.0.1/src/dx12/mod.rs:1734-1739) `[verified]`.
- `get_current_texture` returns `CurrentSurfaceTexture` (wgpu 29 on), and `Queue::present(frame)` replaces `SurfaceTexture::present()` (wgpu 30) `[verified]` ([wgpu CHANGELOG](https://raw.githubusercontent.com/gfx-rs/wgpu/trunk/CHANGELOG.md)).
- On Linux, `Backends::PRIMARY` is Vulkan, Metal, DX12 and WebGPU. GL is `SECONDARY` `[verified]` ([Backends](https://docs.rs/wgpu/30.0.2/wgpu/struct.Backends.html)).

**What this codebase does**
- Adapter `HighPerformance`, backends `BACKENDS` (DX12 on Windows, `PRIMARY` elsewhere) (app.rs:346, 641-650).
- Mailbox if the surface offers it, else Fifo (app.rs:686-690). `desired_maximum_frame_latency = 1` (app.rs:691).
- Acquire with the `CurrentSurfaceTexture` match, reconfigure on `Outdated` or `Lost`, count `Timeout` and `Occluded` as not acquired (app.rs:1290-1301). `queue.present(frame)` at app.rs:1430.
- Wayland pacing: one present in flight, released by the frame callback, with a 100 ms timeout (pacing.rs:21, 126-135). A held frame goes out fresh or overdue (pacing.rs:108-119).

**Verdict: keep.** Mailbox with latency 1 matches the docs. On Vulkan, latency 1 gives two images (`+1`, clamped; the actual count depends on the surface's minimum). On DX12 Mailbox it caps the rate at one monitor refresh, which is the intended limit for a card running at the monitor's rate.
- Raising latency to 2 would only add a frame. Pacing already comes from the frame callback on Wayland and from the waitable object on DX12, so no gain is expected `[unverified]`.

---

## 3. wgpu YUYV to RGB shader conversion

**Current best practice**
- V4L2 `YUYV` (YUY2) is packed 4:2:2, with byte order `Y'0, Cb0, Y'1, Cr0` for two pixels, then `Y'2, Cb2, Y'3, Cr2`. Chroma is subsampled 2:1 horizontally, and each chroma pair belongs to a pair of pixels `[verified]` ([kernel packed YUV formats](https://docs.kernel.org/userspace-api/media/v4l/pixfmt-packed-yuv.html)).
- Upload the packed bytes as a texture with half the width and four bytes per texel (RGBA8), then decode in WGSL with `textureLoad` (no filtering). Other projects do this `[secondary]` ([Rerun YuvPixelLayout](https://ref.rerun.io/docs/rust/head/re_renderer/resource_managers/enum.YuvPixelLayout.html), [VLC issue 26712](https://code.videolan.org/videolan/vlc/-/issues/26712)).
- Matrix: BT.709 for HD with limited range. BT.709-6 section 3.2 gives `E'Y = 0.2126 E'R + 0.7152 E'G + 0.0722 E'B`. Section 3.4 quantises 8-bit luma as `219·E' + 16` and chroma as `224·E' + 128` `[verified: PDF read locally]` ([ITU-R BT.709-6](https://www.itu.int/dms_pubrec/itu-r/rec/bt/R-REC-BT.709-6-201506-I!!PDF-E.pdf)).
- Whether wgpu has a packed 4:2:2 texture format: `[unverified]`, not checked against the TextureFormat list.

**What this codebase does**
- The YUYV texture is `w/2` wide, `Rgba8Unorm`, created at app.rs:722, uploaded with `bytes_per_row = w*2` at app.rs:1307.
- `convert_shader` (app.rs:296-318) loads texel `x/2`. Even pixels take `Y0` with their own chroma. Odd pixels take `Y1` with the average of this and the next macropixel's chroma, clamped at the edge. Nearest texel plus linear chroma for odd pixels.
- The matrix follows mpv for raw frames: BT.709 when width ≥ 1280 or height > 576, else BT.601 (picture.rs:128-130). Limited range is the default (picture.rs:145-150). The luma weights at picture.rs:130 match BT.709-6 §3.2.
- The output is an `Rgba8Unorm` texture at capture size (app.rs:723-727). A second pass scales it (scale.rs:1-30).

**Verdict: keep.** The decoder matches the documented byte layout and the BT.709 coefficients. The RGB intermediate is needed: the filters sample several RGB taps, so a per-tap YUV decode would cost much more, and the screenshot reads RGB at capture size.
- **Consider, measure first:** for `SCALING=fast` (1:1 bilinear), fusing convert and scale into one shader would skip a full-frame write and read. `--stats` already reports the convert and scale GPU times (gputime.rs:19-118). Keep the intermediate for the other scalers and for `s`.
- **Unverified:** whether the card tags its raw frames BT.601 or BT.709, and the chroma siting the MS2109 and MS2130 use. The code's choice (even pixel keeps its own chroma) is not checked against the card. A coloured-bars test pattern on hardware would settle the matrix and siting.

---

## 4. egui-wgpu overlay integration with winit

**Current best practice**
- `egui_wgpu::Renderer::new(device, format, options)`. The output format should be gamma-space (`Rgba8Unorm` or `Bgra8Unorm`) `[verified]` ([egui-wgpu 0.36.2 Renderer](https://docs.rs/egui-wgpu/0.36.2/egui_wgpu/struct.Renderer.html)).
- Each frame: `update_texture` for texture deltas, then `update_buffers`, which returns command buffers from user callbacks, then `render` in a pass whose lifetime is forgotten. `render` panics unless `update_buffers` ran first `[verified]` ([same page](https://docs.rs/egui-wgpu/0.36.2/egui_wgpu/struct.Renderer.html), [0.37.0](https://docs.rs/egui-wgpu/0.37.0/egui_wgpu/struct.Renderer.html)).
- `Context::run_ui(RawInput, |ui| …)` is the 0.36 entry point `[verified]` ([egui Context](https://docs.rs/egui/0.36.2/egui/struct.Context.html)).
- Input is usually fed through egui-winit `[unverified: not checked here]`.

**What this codebase does**
- `Osd` owns an `egui::Context` and an `egui_wgpu::Renderer` (osd.rs:329-337). It builds `RawInput` by hand with `native_pixels_per_point = 1` (osd.rs:352-357). Input is handled by the app's own key code (keys.rs), so egui-winit is not used.
- `draw` runs `run_ui` (osd.rs:359) and `tessellate` (osd.rs:370). It submits the returned command buffers before the main encoder (app.rs:1418). It draws in a `LoadOp::Load` pass over the picture (osd.rs:374-388), called from render at app.rs:1413.
- `scene_text` is built on every `render` (app.rs:1269). While the overlay or the banner is up, that includes each card frame.

**Verdict: keep** the hand-built input and the render path, which match the docs.
- **Consider, measure first:** cache the tessellated output when the scene is unchanged. While the overlay is open, each card frame re-runs egui's layout and tessellation. Measure the CPU cost first, because `--stats` does not report it today. Cost: a `Scene` key that must be complete, or stale text appears.
- **Upgrade:** egui-wgpu 0.37.0 with egui 0.37 (see the versions table). Check the overlay look afterwards.

---

## 5. Wayland presentation-time (`wp_presentation`) in Rust

**Current best practice**
- Request feedback with `wp_presentation.feedback(surface)`. It "associates with the wl_surface.commit", so request it just before the commit that presents the frame `[verified]` ([presentation-time](https://wayland.app/protocols/presentation-time)).
- Each feedback object ends in exactly one of `presented` or `discarded`. `presented` gives the time the update "turned into light the first time". `refresh` is a prediction in nanoseconds, or 0 if none. `flags` include `fixed_rate` and `variable_rate`. Timestamps are `tv_sec_hi`, `tv_sec_lo`, `tv_nsec` on the clock named by `clock_id`, which must be readable with `clock_gettime` `[verified]` ([presentation-time](https://wayland.app/protocols/presentation-time)).
- `wl_surface.frame` "is a good time to start drawing a new frame". It takes effect on the next commit, and the server should leave the client time to draw after the callback `[verified]` ([wayland](https://wayland.app/protocols/wayland), offset 100000).
- Staging protocols: `wp_commit_timing_v1` (target presentation time; the table lists Hyprland 0.52.1, but the table's markers are not explained) and `wp_fifo_v1` (barriers; the page says frame callbacks or timestamps are still needed for throttling) `[verified in part]` ([commit-timing-v1](https://wayland.app/protocols/commit-timing-v1), [fifo-v1](https://wayland.app/protocols/fifo-v1), the second read through a summary).

**What this codebase does**
- `Presentation::new` wraps winit's `wl_display` and the window's `wl_surface` (present/wayland.rs:42-57). The clock must be `CLOCK_MONOTONIC` or the feed goes unpaced (app.rs:533-548).
- `request` asks for feedback and a frame callback, then flushes (present/wayland.rs:69-73). It is called just before `queue.present` (app.rs:1427-1430).
- `drain` reads and dispatches without blocking (present/wayland.rs:78-90). `presented` and `discarded` become `Outcome`s (present/wayland.rs:135-161). The frame callback frees the in-flight present (pacing.rs:152).

**Verdict: keep** the protocol usage, which matches the spec.
- **Change (comment):** present/wayland.rs:76-77 says "with EGL, Mesa reads it during swap". Linux uses Vulkan, not EGL (section 2). `git log -S` shows the line was introduced in `4e2a260` (2026-10-14, #7, libmpv inside the Rust viewer, the EGL era), so the comment is probably stale `[inference from git history]`. Whether Mesa's Vulkan WSI also reads the socket during present: `[unverified]`. Reword the comment to say what is true for the Vulkan path, after checking Mesa, or drop the claim.
- **Change (manifest tidy-up only):** Cargo.toml enables wayland-protocols `staging`, and no module in `src/` uses it. But winit enables it too (winit-0.30.13/Cargo.toml:285-288) `[verified]`, so removing ours changes nothing in the build. Remove it for clarity only, or keep it and say why.
- **Unverified (matters for feedback ordering):** wgpu's Vulkan path makes the commit inside the present call. In wgpu, `Queue::present` calls the HAL present synchronously (wgpu-core-30.0.1/src/present.rs:350-358), and the Vulkan swapchain calls `queue_present` directly (wgpu-hal-30.0.1/src/vulkan/swapchain/native.rs:615) `[verified]`. Whether Mesa commits the surface synchronously inside `vkQueuePresentKHR` is `[unverified]`. If it does not, feedback could attach to a later commit. Check with a feedback-count run (`--stats`), not from the code alone.
- **Consider (later):** `wp_fifo_v1` could replace the frame-callback pacing, and `wp_commit_timing_v1` could target a present time. Both are staging, support varies by compositor, and each would need a measured pacing change. Not near-term.

---

## 6. DXGI frame statistics in Rust (Windows)

**Current best practice**
- `IDXGISwapChain::GetFrameStatistics` works only for flip-model or full-screen swapchains. It returns `DXGI_ERROR_FRAME_STATISTICS_DISJOINT` on the first call and after a mode change `[verified]` ([GetFrameStatistics](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-getframestatistics), [DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model)).
- The statistics "are not reliable in many multiple monitor scenarios, as well as scenarios where other fullscreen apps are running. It may be necessary to call DwmFlush before frame statistics are updated on systems that support Hardware Flip Queue" `[verified]` ([GetFrameStatistics](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-getframestatistics)).
- `GetLastPresentCount` returns the present ID, which "is the value of the PresentCount member" of the frame statistics `[verified]` ([DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model)). But the `DXGI_FRAME_STATISTICS` remarks say presents to the monitor "is not necessarily the same as the number of times that you called Present" `[verified]` ([DXGI_FRAME_STATISTICS](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ns-dxgi-dxgi_frame_statistics)). The two pages do not settle the numbering under Mailbox `[unverified]`.
- The frame latency waitable object: set the flag, call `SetMaximumFrameLatency`, and wait on the handle before rendering each frame, including the first `[verified]` ([GetFrameLatencyWaitableObject](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject)).
- wgpu-hal creates a `FLIP_DISCARD` swapchain with the waitable flag, calls `SetMaximumFrameLatency`, and waits on the handle in `acquire` (wgpu-hal-30.0.1/src/dx12/mod.rs:1681-1686 and 1734-1739) `[verified]`. The default is `Wait` (section 2).
- `DXGI_FRAME_STATISTICS_MEDIA` lives in `dxgi1_3.h` per the Learn search result `[secondary]` ([Learn, dxgi1_3](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/ns-dxgi1_3-dxgi_frame_statistics_media)). The Learn fetch for it returned 404, so the search result is what I have. Its fields are PresentCount, PresentRefreshCount, SyncRefreshCount, SyncQPCTime, SyncGPUTime, CompositionMode and ApprovedPresentDuration, and the mode constants are COMPOSED 0, OVERLAY 1, NONE 2, COMPOSITION_FAILURE 3 `[verified in the windows 0.62.2 bindings]` (windows-0.62.2/src/Windows/Win32/Graphics/Dxgi/mod.rs:395-399, 411-419).

**What this codebase does**
- `present/dxgi.rs`: `Settle` (lines 43-144) turns readings into outcomes. It is platform-free and tested (tests at 248-344). The Windows `FrameStatistics` is at lines 149-245. Requests take `GetLastPresentCount + 1` (217-224). `DISJOINT` resets the state (231). `MAX_PENDING = 240` (22).
- The media statistics are read through `IDXGISwapChainMedia` (185-213). `CompositionMode + 1` gives the flags in the code's own encoding (197), which matches the bindings above.
- `paces()` returns false (242-244), which is consistent with the default `Wait` (section 2).
- The waitable object is not used directly; wgpu owns it, as the doc comment at present.rs:8-12 says. `frame_statistics` is installed after the first configure (app.rs:571 and 696).

**Verdict: keep.** The design uses the documented API and handles the first read and `DISJOINT` as the docs say. Three things to check, in order:
1. **Consider:** the DwmFlush note. The code does not call `DwmFlush`. On the target machine, compare the timing with the docs/testing.md baseline measured on the same monitor. If the readings lag or repeat, try a `DwmFlush` after `Present`. Trade-off: it blocks the thread until the next DWM frame, which is latency.
2. **Consider:** the multi-monitor warning. The stats are unreliable with several monitors or other full-screen apps. docs/testing.md pins the monitor for latency runs, so most measurements are in scope. Record the monitor count in each Windows result.
3. **Unverified:** the `PresentCount` numbering under Mailbox (see above). The unit tests check the arithmetic, not the hardware.
- **Upgrade:** the windows crate is current per docs.rs (0.62.2). Nothing to do.

---

## Other repo parts

- **scale.rs: keep.** It runs the filters on the gamma-encoded frame, not in linear light, and says so in its header (scale.rs:1-30, "Not ported: mpv's sigmoid-light upscaling and linear-light downscaling"). That is a picture-quality gap against mpv, not a graphics-practice error. The fixed 256-entry LUT and the bounded tap count (`MAX_TAPS = 16`, scale.rs:211) are sound. The intermediate is `Rgba16Float` (scale.rs:312), so the filter pass keeps precision that the 8-bit RGB frame lacks.
- **gputime.rs: keep.** Timestamp queries are read back without a wait, and frames are skipped rather than stalled while a readback is pending (gputime.rs:1-3, 103-117). That fits the no-stall rule.
- **`run_app_on_demand` (app.rs:509): keep.** Winit 0.30 runs one loop per window, and the thread-local (app.rs:414, 449, 524) keeps one loop per process. The 0.31 signature takes `impl ApplicationHandler` `[secondary]`, so this area changes with the migration.

---

## Ranked top 5

1. **Change: remove or justify `pre_present_notify`** (app.rs:1429). It has no `RedrawRequested` consumer in this code, and in winit 0.30.13 it only adds a frame callback that wakes the loop. Predicted: no change in the median. Measure with `--stats --seconds 120` against 44.9 ms. Cost: one measured run pair.
2. **Consider, measure first: cache the overlay's tessellation when the scene is unchanged** (section 4). While the overlay is open, each card frame re-runs egui. Cost: a complete scene key.
3. **Consider: read the wgpu 30.0.2 entries, then bump** (versions table). The egui-wgpu 0.37 bump needs no wgpu change, so it can follow the overlay check.
4. **Consider, Windows: verify the DXGI assumptions on hardware** (section 6): the DwmFlush note, the monitor-count note, and the `PresentCount` numbering under Mailbox. Cost: one `--stats` run on the Windows machine per change.
5. **Tidy: fix the stale EGL comment at present/wayland.rs:76-77 and drop the redundant `staging` feature** (section 5). Neither changes timing. Also the RedrawRequested experiment (section 1) is low priority, predicted to show no gain.

Not recommended now: winit 0.31 (beta only; migration, see the versions table). `wp_fifo_v1` and `wp_commit_timing_v1` (staging; compositor support varies; section 5).

## Unverified or not opened

- Mesa's Vulkan Wayland WSI: whether it commits inside `vkQueuePresentKHR`, and whether it reads the socket during present (the EGL comment question). `[unverified]`, Mesa source not checked.
- Mesa's `VK_EXT_present_timing` on Wayland, and the Mesa 26.1 and 26.2 version claims. The [VK_EXT_present_timing proposal](https://docs.vulkan.org/features/latest/features/proposals/VK_EXT_present_timing.html) was read (explicit target times, timing feedback, no Wayland text) `[verified]`. The Mesa claims are `[secondary]` search summaries, not opened.
- Whether wgpu 30 exposes `VK_EXT_present_timing`: `[unverified]`.
- winit's Wayland frame-callback wake behaviour, beyond the source lines cited: `[inference]`. Verify with the wake count in a run.
- The card's YUV matrix, range and chroma siting: `[unverified]`, needs a test pattern on hardware.
- wgpu 30.0.2 changelog entries, and the egui changelog past about half of its length: `[unverified]`.
- Whether `wgpu` has a packed 4:2:2 texture format: `[unverified]`.
- The GitLab Wayland XML sources were blocked (bot protection), so wayland.app was used.

## Sources (opened this session)

- winit: https://docs.rs/winit/0.30.13/winit/application/trait.ApplicationHandler.html ; https://docs.rs/winit/0.30.13/winit/window/struct.Window.html ; https://docs.rs/winit/0.30.13/winit/event/enum.WindowEvent.html ; https://docs.rs/crate/winit/latest ; https://github.com/rust-windowing/winit/releases ; https://docs.rs/winit/0.31.0-beta.4/winit/window/trait.Window.html
- wgpu: https://docs.rs/wgpu-types/latest/wgpu_types/struct.SurfaceConfiguration.html ; https://docs.rs/wgpu/30.0.1/wgpu/enum.PresentMode.html ; https://docs.rs/wgpu/30.0.2/wgpu/struct.Backends.html ; https://docs.rs/crate/wgpu/latest ; https://raw.githubusercontent.com/gfx-rs/wgpu/trunk/CHANGELOG.md
- egui: https://docs.rs/egui/0.36.2/egui/struct.Context.html ; https://docs.rs/egui-wgpu/0.36.2/egui_wgpu/struct.Renderer.html ; https://docs.rs/egui-wgpu/0.37.0/egui_wgpu/struct.Renderer.html ; https://docs.rs/crate/egui-wgpu/latest ; https://docs.rs/crate/egui-wgpu/0.37.0/source/Cargo.toml ; https://raw.githubusercontent.com/emilk/egui/main/CHANGELOG.md
- Wayland: https://wayland.app/protocols/presentation-time ; https://wayland.app/protocols/wayland ; https://wayland.app/protocols/commit-timing-v1 ; https://wayland.app/protocols/fifo-v1 ; https://docs.rs/crate/wayland-client/latest ; https://docs.rs/crate/wayland-protocols/latest
- Microsoft: https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model ; https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-getframestatistics ; https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ns-dxgi-dxgi_frame_statistics ; https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject ; https://docs.rs/crate/windows/latest
- Video: https://docs.kernel.org/userspace-api/media/v4l/pixfmt-packed-yuv.html ; https://www.itu.int/dms_pubrec/itu-r/rec/bt/R-REC-BT.709-6-201506-I!!PDF-E.pdf (read as a local PDF)
- Local pinned sources (`~/.cargo/registry/src`): winit-0.30.13, wgpu-hal-30.0.1, wgpu-core-30.0.1, wgpu-types-30.0.1, windows-0.62.2, egui-wgpu-0.36.2
- Secondary: https://ref.rerun.io/docs/rust/head/re_renderer/resource_managers/enum.YuvPixelLayout.html ; https://code.videolan.org/videolan/vlc/-/issues/26712

## Repo references

- app.rs: 296-318 (convert_shader), 346 (BACKENDS), 414 and 449 and 524 (thread-local event loop), 509 (run_app_on_demand), 533-548 (feedback), 571 and 696 (frame_statistics), 601 (init), 641-650 (adapter), 686-691 (present mode and latency), 722 and 723-727 (textures), 1262-1301 (render, acquire), 1269 (scene_text), 1307 (write_texture), 1413 (osd.draw), 1418-1419 (submit), 1427-1430 (feedback request, pre_present_notify, present), 1440-1452 (resumed), 1463-1495 (user_event), 1497-1530 (window_event), 1532-1563 (about_to_wait).
- present.rs:8-12. present/wayland.rs: 42-57, 60-62, 69-73, 71, 76-77, 78-90, 135-161. present/dxgi.rs: 22, 43-144, 149-245, 185-213, 197, 217-224, 231, 242-244, 248-344.
- pacing.rs: 21, 29, 108-119, 126-135, 152, 161-174.
- picture.rs: 128-130, 145-150. scale.rs: 1-30, 211, 312, 319. gputime.rs: 1-3, 19-118. osd.rs: 329-337, 342-395, 352-357, 359, 370, 374-388.
- Cargo.toml: `winit = "0.30"`, `wgpu = "30"`, `egui = "0.36.2"`, `egui-wgpu = { version = "0.36.2", default-features = false }`, `wayland-protocols = { version = "0.32.13", features = ["client", "staging"] }`.
- docs/testing.md: "Measuring latency" (baseline 44.9 ms median, p99 53.7, 2 repeated refreshes, 2026-10-16; `27c31d3` on 2026-10-17).
