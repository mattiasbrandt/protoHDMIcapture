# Video capture and audio: Rust practice vs protoHDMIcapture

Research note, 2026-11-17: modern Rust practice (2025-2026) against
protoHDMIcapture, one of three ([concurrency](rust-concurrency.md),
[graphics](rust-graphics.md), [capture and audio](rust-capture-audio.md)).
Findings, not decisions: each verdict is a proposal for the grilling and
the issues that follow. Line numbers are from `14a5301`. Nothing here was
measured on a card; every "measure first" still needs its run
([`docs/testing.md`](../testing.md)).

No card, PipeWire or cargo test was run.
Crate facts come from the crates.io API (`crates.io/api/v1/crates/<name>`, read 2026-11-17)
and from the cargo registry source in `~/.cargo/registry/src` (the versions in Cargo.lock).

Sources are cited inline. "Unverified" marks anything not checked against a primary source.

## Crate facts (crates.io, 2026-11-17)

| Crate | Pinned/used | Latest on crates.io | Last release | Notes |
|---|---|---|---|---|
| linuxvideo | =0.3.5 (vendored + patch) | 0.3.5 | 2025-07-15 | No release since; repo https://github.com/SludgePhD/LinuxVideo.git. Upstream `main` could not be read (GitHub page shows no source) so whether the timestamp/sequence patch was merged is **unverified**. |
| v4l (libv4l-rs) | not used | 0.14.0 | 2023-05-12 | Stale, 3+ years without release. |
| nokhwa | not used | 0.10.11 | 2026-05-15 | Maintained (0.10.10 2025-11, 0.10.11 2026-05). A cross-platform camera abstraction; its V4L2 and MF internals were **not** checked against the per-buffer fields this codebase needs (timestamp, sequence, ERROR flag, raw MJPEG bytes). Unverified. |
| pipewire (pipewire-rs) | 0.10.1 direct | 0.10.1 | 2026-08-19 | cpal 0.18.2 requires `pipewire` `version = "0.10"` with feature `v0_3_53` (cpal `Cargo.toml` line 231-234), so the direct dependency and cpal share one pipewire 0.10.x. Source: `~/.cargo/registry/src/*/cpal-0.18.2/Cargo.toml`. |
| cpal | 0.18.2 | 0.18.2 | 2026-08-16 | 29M downloads. 0.18.0 (2026-06-06) and 0.18.1 (2026-06-07) were quick follow-ups. |
| wasapi (wasapi-rs) | not used | 0.27.0 | 2026-11-06 | Actively maintained (0.24-0.27 between Aug and Nov 2026). It is a WASAPI wrapper; cpal already sits on WASAPI via the `windows` crate. |
| zune-jpeg | 0.5.15 | 0.5.16-rc3 (pre-release) | 0.5.15 on 2026-03-26; rc on 2026-10-19 | Keep 0.5.15 stable; the rc series is pre-release. |
| windows | 0.62.2 | 0.62.2 | 2025-10-06 | Latest on crates.io as of this session. Newer 0.63 not seen on crates.io (unverified beyond the API). |
| wgpu | 30 (Cargo.lock 30.0.1) | 30.0.2 | 2026-11-12 | Not in scope of this report. |

Sources: https://crates.io/crates/linuxvideo, https://crates.io/crates/v4l, https://crates.io/crates/nokhwa, https://crates.io/crates/pipewire, https://crates.io/crates/cpal, https://crates.io/crates/wasapi, https://crates.io/crates/zune-jpeg, https://crates.io/crates/windows

---

## 1. rust v4l2 capture mmap low latency

### Current best practice (sources)
- The kernel doc for the buffer API defines `V4L2_MEMORY_MMAP`, the `MAPPED` flag and the `sequence` field ("counts the frames (not fields!) in sequence... includes dropped or repeated frames"), and `V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC` ("taken from the CLOCK_MONOTONIC clock") and `V4L2_BUF_FLAG_ERROR` (dequeued but "might have been corrupted"). https://docs.kernel.org/userspace-api/media/v4l/buffer.html (fetched this session).
- The same page gives **no minimum buffer count** for VIDIOC_REQBUFS (verified: it only says the index runs to the count allocated). Minimum/recommended counts therefore **unverified** here; the general practice of 2-4 MMAP buffers with `poll()` before `DQBUF` is common but not sourced from this page.
- Latency: the lowest-latency pattern is "dequeue, process, requeue" with the timestamp taken from the kernel buffer (CLOCK_MONOTONIC), so the viewer measures capture time rather than dequeue time. The kernel doc above is the primary source for the flag and clock.

### What this codebase does
- `src/capture/v4l2.rs:35-70` (`V4l2::open`): requests YUYV or MJPG at the RES/FMT, checks the card gave the size, fourcc, stride (line 56-58) and fps within 0.5 % (line 65), then `into_stream()`.
- `src/capture/v4l2.rs:121-135`: `poll(POLLIN)` before each dequeue, `POLLERR|POLLHUP|POLLNVAL` treated as card gone. Good; matches the "poll before dequeue" pattern.
- `src/capture/v4l2.rs:137-147`: `dequeue` closure hands `Raw` with `captured = buf.timestamp()` (kernel SOE time), `mark = Mark::Sequence(buf.sequence())`, `flagged = buf.is_error()`. This uses the kernel clock and sequence numbers as the kernel doc describes; it is the right design.
- `src/capture/v4l2.rs:138-141`: warns once if the buffer is not `TIMESTAMP_MONOTONIC`. Correct behaviour.
- Vendored linuxvideo: `vendor/linuxvideo/src/stream.rs` line 40-41 `DEFAULT_BUFFER_COUNT: u32 = 2`; `vendor/linuxvideo/src/lib.rs` (`into_stream`, ~line 407-414) calls `ReadStream::new(..., DEFAULT_BUFFER_COUNT)`. `ReadStream::new` is `pub(crate)` (stream.rs ~138), so **the count cannot be set from the caller**. Only a fork can change it.
- `vendor/linuxvideo/src/stream.rs:215-248` (`dequeue`): the closure runs **before** the buffer is re-enqueued (`self.enqueue(buf.index)` after `cb(view)`). So with 2 buffers, while the policy decodes MJPEG and copies the frame (`src/capture/policy.rs:532-589`, decode at line 555-557, `src/run.rs:183-209` copies into the window slot at line 192), only one buffer is queued with the driver.
- The patch (`vendor/linuxvideo-timestamp-sequence.patch`, referenced from `Cargo.toml` `[patch.crates-io]`) adds `timestamp()` and `sequence()` to `ReadBufferView`. The `Cargo.toml` comment says it is "to offer upstream". Upstream status **unverified** (see table).

### Verdict: **consider**
- Keep linuxvideo 0.3.5 + the patch. No maintained alternative exposes the needed per-buffer fields: v4l is stale (2023), and nokhwa's raw access was not verified. Keep.
- **Concrete change (consider):** raise the buffer count from 2 to 3 or 4 in the vendored copy (`DEFAULT_BUFFER_COUNT` in `vendor/linuxvideo/src/stream.rs:41`, or expose a `into_stream_with_buffers(n)` in the patch). Trade-off: the viewer always dequeues the newest buffer, so an extra buffer adds no display latency while the consumer keeps up; it only absorbs the frame held during decode and the copy, so fewer sequence gaps under load. Whether it reduces the gaps recorded on this card is **unverified**; measure with the dashboard's gap counter (`src/capture/policy.rs` `Event::Gap`) before and after, on a real card run (needs a hardware run, do not do it unasked).
- **Consider:** upstream the patch, or check the upstream `main` before the next bump. Unverified whether it is already there.

---

## 2. rust Media Foundation capture windows crate

### Current best practice (sources)
- The source reader's asynchronous mode: `ReadSample` returns at once, `OnReadSample` is called on a worker thread, the callback must be thread-safe, and "to get the next sample, call ReadSample again". https://learn.microsoft.com/en-us/windows/win32/medfound/using-the-source-reader-in-asynchronous-mode (fetched this session).
- `IMFMediaBuffer::Lock` remarks: for 2-D data, use `IMF2DBuffer::Lock2D`; the docs say the general guidance is "to avoid using IMFMediaBuffer and IMF2DBuffer whenever possible". https://learn.microsoft.com/en-us/windows/win32/api/mfobjects/nf-mfobjects-imfmediabuffer-lock (fetched this session).
- The `windows` crate (microsoft/windows-rs) is the maintained binding; `windows-core` `#[implement]` for the callback. https://crates.io/crates/windows (0.62.2, 2025-10-06).

### What this codebase does
- `src/capture/mf.rs:52-66` (`Mf::init`): COM multithreaded plus `MFStartup`, balanced in `Drop`. Right order.
- `src/capture/mf.rs:337-372` (`reader`): picks the card's own YUY2 or MJPG type that matches RES and FPS within 0.5 %, and reads `MF_MT_DEFAULT_STRIDE`. `source_reader` (388-411) sets `MF_READWRITE_DISABLE_CONVERTERS` (the card's own frames, no converter). Whether that attribute is the lowest-latency option for a raw device source is **unverified**; the docs fetched did not address it.
- `src/capture/mf.rs:456-487` (`Callback`, `OnReadSample`): the callback sends a `Delivered` over an mpsc channel and reuses byte buffers through a free list. Its one outstanding `ReadSample` is re-issued by the capture thread **before** the frame is taken (`src/capture/mf.rs:570-600`, `self.request()?` before `take`), which matches the MS pattern.
- `src/capture/mf.rs:492-503` (`copy`): `ConvertToContiguousBuffer` + `Lock` + `memcpy` per sample. This is the `IMFMediaBuffer` path that the MS doc discourages for 2-D data. A YUY2 sample at 1080p60 is about 4 MB, so about 250 MB/s of copies. The per-sample copy is the cost, not correctness.
- `src/capture/mf.rs:561`: `Running::request` on the capture thread.

### Verdict: **keep the direct `windows` binding; consider reducing the copy**
- Keep: the `windows` crate is the right, maintained binding, and the async pattern follows the MS sample.
- **Concrete change (consider):** for YUY2 samples, try `IMF2DBuffer2::Lock2DSize` with a read-only lock flag, or copy straight from the locked buffer into the window slot (`src/run.rs:192`) instead of the intermediate `Vec` (the callback thread owns the `Vec`). Trade-off: one fewer copy per frame; more `unsafe` lifetime work because the buffer must stay locked while the frame is used. Measure first: no timing for `copy` exists in the repo.
- Note: the windows-rs 0.62.2 crate on crates.io is from 2025-10; a newer `windows` may exist (unverified).

---

## 3. rust MJPEG decode zune-jpeg performance

### Current best practice (sources)
- zune-jpeg is pure Rust, with SIMD upsampling paths (`src/upsampler/avx2.rs`, `src/upsampler/scalar.rs` in the 0.5.15 source). It supports an explicit output colour space: `DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA)` (documented in `zune-jpeg-0.5.15/src/lib.rs:109`). https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg (repo); https://docs.rs/zune-jpeg/0.5.15 (not fetched).
- Repo-side measurement already recorded: `mjpeg::tests::decode_cost` (`src/mjpeg.rs:195-235`), about 2.3 ms at 720p and 5.1 ms at 1080p per frame on a Ryzen 9 7950X3D (`capture-cards/ms2109.md`, Quirks). These are numbers the repo measured; not independently re-run here.
- Known cards: MS2130 MJPEG 1080p60 decodes with 2 undecodable frames in 2400 at stream start (`capture-cards/ms2130.md`).

### What this codebase does
- `src/mjpeg.rs:46-50`: `jpeg_set_out_colorspace(ColorSpace::YCbCr)` with `set_strict_mode(false)`, so the decoder keeps the JPEG's YCbCr values (no RGB round trip). Correct for YUYV output.
- `src/mjpeg.rs:99-107` (`pack_yuyv`): the decoder yields 4:4:4 (Y, Cb, Cr per pixel), and the code averages the chroma of each pixel pair into YUYV. The comment (lines 96-98) explains that a 4:2:2 JPEG's chroma sits between the pair.
- `src/mjpeg.rs:36-45`: frames without DHT get the AVI1 marker so the standard Huffman tables are used (ffmpeg behaviour, Annex K.3).
- `src/capture/policy.rs:555-557`: the decode runs inside the capture loop, inside the V4L2 dequeue closure, so the MJPEG decode holds the one queued buffer (see section 1).

### Verdict: **keep zune-jpeg 0.5.15**
- Keep: pure Rust, builds on Windows without a C toolchain (the stated reason, `src/mjpeg.rs:1-4`), SIMD upsampling, measured at 2-5 ms.
- The 4:4:4 round trip plus chroma averaging is extra work per frame. zune-jpeg has **no public option found** for raw subsampled output (grep of `zune-jpeg-0.5.15/src/lib.rs` found only the colour-space option; upsampling is internal). Unverified beyond that grep.
- Alternatives: libjpeg-turbo (C dependency, real `TurboJPEG` YUV output via `tjDecompressToYUV2`) would avoid the 4:4:4 expansion but adds a C build on Windows. Trade-off not worth it at 2-5 ms per frame, per the measurements. Not verified against a turbojpeg crate's release status.
- Do not move to 0.5.16-rc3 (pre-release).

---

## 4. cpal 0.18 pipewire backend loopback

### Current best practice (sources)
- cpal 0.18.2 PipeWire host: `BufferSize::Fixed(n)` sets the stream property `node.latency = "n/rate"` (`~/.cargo/registry/src/*/cpal-0.18.2/src/host/pipewire/device.rs:178-183`), and the input build rejects a size outside the server's quantum range (`device.rs:351-361`).
- cpal groups input and output nodes with `node.group = cpal-<pid>` so PipeWire schedules them in one quantum (`device.rs:174-176`). cpal also promotes its callback thread to real time (`rt_promote.rs`) and wraps input callbacks to keep `capture` monotonic (`device.rs:~370`).
- PipeWire's own loopback (`pw-loopback`) uses the same node properties, which is why view.sh and this code match it. https://docs.pipewire.org (not fetched this session; **unverified** for the specific property text).

### What this codebase does
- `src/audio.rs:33-37`: `HOST = HostId::PipeWire` on Linux, `Wasapi` on Windows.
- `src/audio.rs:566-600` (`Audio::start`): `BufferSize::Fixed(buffer_frames)` where `buffer_frames = LAT_MS * 48000 / 1000` (line 584). Fixed size makes cpal set `node.latency` as above, which is the intended use.
- `src/audio.rs:205-219` (`verify_linked`): before the output plays, `crate::pw::snapshot()` is called **every 5 ms** for up to 2 s (`LINK_POLL` line 200). `snapshot()` (`src/pw.rs:78-144`) creates a new PipeWire mainloop, context and core on each call, and syncs twice. The comment at `src/audio.rs:7-9` says a look costs about 2 ms. Correct but wasteful; it is a polling loop that could use one registry listener.
- `src/audio.rs:112` (`Ring`), `:596`: the loopback is a `Mutex<Ring>` (`VecDeque<[f32;2]>`) locked in both the input callback (line 608) and the output callback (`play_out`, line 473 onward; called via `ring.lock().unwrap()` at line 626). Both callbacks run on PipeWire's real-time threads.
- `src/audio.rs:605-614`: the input callback extends the `VecDeque` and drains to `ring_frames` under the lock. The deque is created with `with_capacity(ring_frames(buffer_frames))` (line 596), so an `extend` that goes past capacity can reallocate on the real-time thread. Size of the effect **unverified**; not measured.
- `src/main.rs:68-82`: `PIPEWIRE_PROPS` gets `node.dont-fallback = true` so a missing target is not silently the default microphone. Done before threads start (the `set_var` is in `main`). Correct.
- `src/pw.rs:1-10`: the module doc says the `dont-fallback` proof is in `sources_linked_to_us` (line 176-193). Correct.

### Verdict: **keep cpal; consider two fixes**
- Keep cpal 0.18.2 for both the PipeWire and WASAPI paths. It is the most-downloaded Rust audio crate (29M downloads) and its PipeWire host does the `node.latency` work this code relies on.
- **Concrete change (consider, low risk):** replace the 5 ms `pw::snapshot()` polling in `verify_linked` with one registry listener that watches for a link into this process's capture stream and stops on the first match or at the 2 s deadline. Trade-off: about 40 lines in `pw.rs`; saves a PipeWire connection per 5 ms during start and sync keys. Keep the `dont-fallback` check as it is.
- **Concrete change (consider):** swap the `Mutex<VecDeque>` ring for a single-producer, single-consumer lock-free ring (e.g. the `ringbuf` or `rtrb` crate, both used for audio in Rust; versions **not checked**), and remove the `.lock().unwrap()` panic in the real-time callbacks. Trade-off: a new dependency and `unsafe`-free but more code; the current lock is uncontended except during the drain. Measure the underruns and xruns counters before choosing. Do not change without a measured reason.
- Unverified: whether cpal 0.18.2 changed the input/output callback thread priority behaviour; `rt_promote.rs` was read but not timed.

---

## 5. rust WASAPI audio windows crate

### Current best practice (sources)
- WASAPI shared mode with an event callback: cpal 0.18.2 uses `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` by default (`cpal-0.18.2/src/host/wasapi/device.rs:55`).
- Device activation through `ActivateAudioInterfaceAsync` for default endpoints, and `IMMDevice::Activate` for specific ones: cpal's `device.rs:346-390`. The repo's own comment (`src/audio.rs:533-541`) records that the async default path failed with `RPC_E_CHANGED_MODE` on a laptop when the output was a default handle (#40), and that the specific-device path worked.
- The `wasapi` crate (wasapi-rs, HEnquist) is the other option: 0.27.0 on 2026-11-06, actively maintained (https://crates.io/crates/wasapi). It exposes WASAPI directly rather than through a cross-platform API.

### What this codebase does
- `src/audio.rs:224-229`: `HOST = HostId::Wasapi` on Windows; names are the friendly name and the endpoint id (`src/audio.rs:49-54`).
- `src/audio.rs:529-560` (`default_output`, windows): resolves the default endpoint to its specific device by id (cpal's `host.output_devices()` scan) before opening it, to avoid the async default path. The `warning:` for an output without an id is printed, not swallowed (line 556).
- `src/capture/usb_hub.rs` reads the card's link speed through `IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX(_V2)` (lines 25-31, `link` at 35-56); unrelated to audio, noted for completeness.
- `src/audio.rs:334-395` / `src/audio.rs:398-460` (WASAPI session volume through `ISimpleAudioVolume`): per-process session level, consistent with this codebase's level key design.

### Verdict: **keep cpal**
- Do not switch to the `wasapi` crate. It is a second WASAPI wrapper with its own threading; cpal already sits on the `windows` crate and the repo already uses `windows` directly for MF and USB. Switching would duplicate device handling and add a second COM model.
- **Consider:** report the `RPC_E_CHANGED_MODE` default-device failure upstream to cpal (the repo's own workaround at `src/audio.rs:533-541` is the reproduction), and re-test with cpal 0.18.2 on a Windows run before removing the workaround. Not verified whether 0.18.2 already fixed it.

---

## Ranked top-5 recommendations

1. **Measure before anything else: V4L2 buffer count (consider).** Raise the vendored linuxvideo buffer count from 2 to 3-4 (`vendor/linuxvideo/src/stream.rs:41`), then compare the dashboard's gap counter on a card run. Low cost, reversible, and it addresses the decode-holds-the-buffer effect in `src/capture/policy.rs:555`. Needs a hardware run, so ask first.
2. **Replace the 5 ms PipeWire snapshot loop in `verify_linked` with one registry listener (consider).** `src/audio.rs:205-219`, `src/pw.rs:78-144`. Cuts a PipeWire connection per 5 ms during start and sync; keep `dont-fallback` check.
3. **Upstream or re-check the linuxvideo timestamp/sequence patch (consider).** Confirm whether upstream `main` merged it (not readable here), so the vendored copy can be dropped or kept with a reason. (The buffer-count setter was tried and dropped: the #54 A/B showed no difference.)
4. **Reduce the Media Foundation copy (consider, measure first).** `src/capture/mf.rs:492-503`: YUY2 via `IMF2DBuffer2::Lock2DSize` read lock, or copy directly into the window slot. Only if a timing shows the copy matters.
5. **Look at the audio ring's locks and allocation (consider).** `src/audio.rs:596, 608, 626`: a lock-free SPSC ring would remove the mutex and `unwrap` from real-time callbacks; first confirm by measuring underruns/xruns. Also re-test the Windows default-output workaround (`src/audio.rs:533-560`) on cpal 0.18.2 and report to cpal if it still reproduces.

Keep as is: linuxvideo 0.3.5 (no better maintained V4L2 crate: v4l stale since 2023), zune-jpeg 0.5.15 (no public raw-YUV output found; decode measured at 2-5 ms), cpal 0.18.2 (PipeWire and WASAPI host, pipewire 0.10 shared with the direct dependency), the direct `windows` 0.62.2 binding (MF, USB).

## Unverified or not done
- linuxvideo upstream `main` state (patch merged? buffer count?): not readable.
- Minimum/recommended V4L2 MMAP buffer count: kernel doc gives none.
- nokhwa's internals against the per-buffer fields: not checked.
- zune-jpeg raw subsampled output: only a grep of `lib.rs`; no docs.rs read.
- Lock-free ring crate versions (ringbuf, rtrb): not checked.
- MS Learn "low-latency" attributes for raw device sources: not found in the pages fetched.
- Whether the current card gap counts depend on buffer count: needs a hardware run (not done, per instructions).
- The `windows` crate: 0.62.2 is the latest on crates.io as of the API read; a newer release was not confirmed.
