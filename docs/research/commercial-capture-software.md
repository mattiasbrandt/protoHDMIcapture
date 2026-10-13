# Commercial capture-card software vs protoHDMIcapture

Research note, 2026-10-13. Scope: what the vendor capture apps offer, what
protoHDMIcapture does today, and what is worth building on Linux.

Verification key: **[fetched]** = read from the page this session;
**[snippet]** = seen only in search-result text (page not opened, or
returned 403/404); **[unverified]** = could not confirm from a primary source.
Vendor help-center pages were mostly 403 to WebFetch, so Elgato claims are
largely [snippet]. Treat them as leads to check in a browser.

**Decided after this note (2026-10-13):** the project stays a viewer.
Recording, the replay buffer, streaming and Stream Deck integration are out
of scope; screenshots stay mpv's own `s`. The shortlist below is kept as the
record of what was weighed. One finding it lacks: mpv's `stream-record`
property records in-process with no second reader (tested: MS2109 MJPEG
720p60, 241 frames in 4.0 s, feed held 60 fps), which is the route if
recording is ever reconsidered.

**Status since (2026-10-13, same day, later):** this note is a snapshot and is
not kept current; "protoHDMIcapture today" below describes the repo as it was
then. Since shipped from the shortlist: 4 in part (mute and volume on the
loopback, keys **m**, **9**, **0**; no level meter), 5 (colour keys **1**–**8**,
**r** resets), 7 in another shape (the **Tab** overlay is a control panel and
setup tips; fps, drops and audio state stay in the terminal dashboard), and 8
(source profiles in `sources/` for PS4, PS5, Xbox Series, Switch, Switch 2 and
PC). Also new: a "No signal" banner. Still open: 1 (screenshot confirm and
folder) and 6 (rebindable keys). For what the viewer does now, read the README.

## protoHDMIcapture today (from repo)

- Video: `mpv --untimed --profile=low-latency` straight from `/dev/videoN`
  (README, Quick start / TL;DR). No audio in the video path.
- Audio: separate `pw-loopback` with fixed `--delay` (README TL;DR).
- Keys (`lib/mpv/protohdmicapture.lua`): **f** display mode, **d** dashboard.
  README: **i**/**I** mpv stats overlay, **q** quit.
- Setup wizard, USB-serial card pinning, USB link check, signal check, A/V
  sync tuner, Markdown diagnostics report (README, "The setup wizard").
- Display modes fullscreen/borderless/windowed; Hyprland refresh matching.
- Config: `LEVELS` (auto/limited/full) for range (README config table).
- mpv IPC socket `/tmp/mpv-protohdmicapture.sock` (README, mpv section).
- Source-side guidance for PS5: HDCP off, Linear PCM, HDR off, 1080p,
  120 Hz/VRR off (README, Preconditions).
- Not present: recording, clips, replay/Flashback, audio meter or mute,
  brightness/contrast controls, EDID control, firmware tools, mic monitoring
  UI, overlays beyond mpv stats.

## 1. Per-product feature inventory

### Elgato 4K Capture Utility (4KCU), legacy

| Feature | Status | Source |
|---|---|---|
| Two tabs: Capture (preview, record/Flashback controls) and Library | [snippet] | https://help.elgato.com/hc/en-us/articles/360027963752 |
| Flashback: caches footage while not recording, "several hours" claimed | [snippet] hours figure unverified | https://techpowerup.com/forums/threads/293621 (quotes HD60 X press) |
| Default hotkeys (Ctrl+Shift+Alt+F Flashback, Ctrl+Shift+F save last 30 s, PgUp/PgDn slider) | [snippet] legacy page, may not match current build | https://help.elgato.com/hc/en-us/articles/11395449701773-Elgato-4K-Capture-Utility-Hotkeys-and-Key-Binds |
| Instant Gameview: low-latency monitoring, described as hardware-side, syncs capture with gameplay | [snippet] | https://zoftwarehub.com/products/elgato-game-capture/features |
| Supported devices: HD60 S/S+/X, 4K60 S+, 4K X, Neo, HD60 Pro, 4K60 Pro, 4K60 Pro MK.2, 4K Pro, Cam Link family (Cam Link 4K revision 20GAM9902 excluded) | [snippet] | https://help.elgato.com/hc/en-us/articles/360060805031-Elgato-4K-Capture-Utility-Supported-Capture-Devices |
| Latest release notes found: 1.7.12 (2024-04-18), capture category lists up to 1.7.16 | [snippet] | https://help.elgato.com/hc/en-us/articles/26006411476365-Elgato-4K-Capture-Utility-1-7-12-Release-Notes |
| Legacy/EOL notice on supported-devices page (zh-tw version) | [snippet] | https://help.elgato.com/hc/zh-tw/articles/360060805031-Elgato-4K-Capture-Utility-%E6%94%AF%E6%8F%B4%E7%9A%84%E6%93%B7%E5%8F%96%E8%A3%9D%E7%BD%AE |
| Firmware updates and device info via Camera Hub (not 4KCU) | [snippet] | same supported-devices page |
| Recording and streaming: older article says 4KCU records but cannot stream, some cards limited to 1080p60 | [snippet] old, likely outdated | https://help.elgato.com/hc/en-us/articles/360028241191-Elgato-4K-Capture-Utility-and-Elgato-Game-Capture-Devices |
| Mac build reportedly less capable than Windows | [unverified] third-party blog only | https://www.narcosis-the-game.com/?p=6680 |

Audio mixing, HDR, colour/EDID, overlays: not confirmed for 4KCU in any
fetched source [unverified].

### Elgato Studio (current app, Windows and macOS)

| Feature | Status | Source |
|---|---|---|
| Preview, device settings and recording in one window; HDR preview auto-tone-mapped to SDR on SDR display | [fetched] | https://www.elgato.com/ch/en/explorer/products/capture/elgato-studio-software-overview/ |
| Record (red button); settings for max resolution, max fps, quality, format H.264 / HEVC / AV1 | [fetched] | same |
| 10-bit HDR recording needs HEVC or AV1, may lower max resolution | [fetched] | same |
| Screenshot button, thumbnail, click to open folder | [fetched] | same |
| Volume slider; audio input HDMI or Analog (3.5 mm) | [fetched] | same |
| HDR tone mapping for third-party apps (OBS), applied after Studio closes | [fetched] | same |
| Device settings: HDMI colour range, Input EDID mode, Custom EDID (with EDID preset), device info with serial and firmware version | [fetched] | same |
| Stream Deck control via separate Elgato Studio plugin | [fetched] | same |
| No Flashback Recording | [snippet] | https://help.elgato.com/hc/en-us/articles/37231557626129-Elgato-Studio-Software-Interface |
| Instant Gameview, latency: not mentioned in Studio's pages | [fetched] absence only | same |

### Elgato Camera Hub

- A webcam app. Per the 4KCU supported-devices page, it is the tool for
  firmware updates and device info for capture hardware [snippet]:
  https://help.elgato.com/hc/en-us/articles/360060805031-Elgato-4K-Capture-Utility-Supported-Capture-Devices
- Supported-device list: https://help.elgato.com/hc/en-us/articles/4417243600909-Elgato-Camera-Hub-Supported-Devices [unverified, not opened]

### Elgato Stream Deck capture integration

- Only what Elgato Studio's page states: the Stream Deck plugin for Studio
  [fetched, see Studio table]. Actions and button list not verified.

### Elgato Game Capture HD / HD60 (legacy)

| Feature | Status | Source |
|---|---|---|
| Discontinued legacy product; final builds 3.7 (Windows), 2.11.14 (macOS) | [snippet] | https://help.elgato.com/hc/en-us/articles/360027964072-Elgato-Game-Capture-HD-Software-Release-Notes-Windows |
| Stream Command scenes, separate audio tracks (from 2.0) | [snippet] | https://help.elgato.com/hc/en-us/articles/360027951012-Newer-Elgato-Game-Capture-HD-software-works-with-original-Elgato-Game-Capture-HD-hardware |
| Windows changelog includes Facecam as a webcam source in Stream Command | [snippet] | release notes link above |

### Corsair

- No capture product found. Corsair iCUE is peripheral and system control
  (RGB, macros, fans, PSU monitoring) [snippet]:
  https://www.guru3d.com/news-story/corsair-launches-icue-unified-software.html
- "Corsair Game Capture 4K" not found in any search. Elgato, which makes
  the capture products above, is Corsair-owned [unverified: general
  knowledge, not checked this session]. Any Corsair capture features are
  Elgato's, covered above.

### Razer Ripsaw HD / Synapse

| Feature | Status | Source |
|---|---|---|
| 1080p60 capture, 4K60 passthrough, built-in audio mixing | [snippet] | https://www.guru3d.com/news-story/razer-ripsaw-hd-video-capture-box.html |
| No standalone capture app; Synapse 3 manages drivers and firmware | [snippet] | https://www.windowscentral.com/how-set-and-use-razer-ripsaw-hd |
| Synapse handles audio monitoring; notifies on display setting changes | [snippet] | https://www.tomsguide.com/us/razer-ripsaw-hd%2Creview-6388.html |
| Front LED green/red when Synapse recognises the card | [snippet] | https://www.windowscentral.com/razer-ripsaw-review |
| Synapse 4 volume-control problem reported by users | [snippet] | https://insider.razer.com/razer-synapse-4-55/synapse-4-can-t-adjust-ripsaw-hd-s-volume-69061 |
| Instant GameView or any "GameView" feature | not found [unverified] | Razer manual not located |
| Recording, preview, HDR, EDID, overlays in Razer software | not found [unverified] | Razer Insider page fetched: user support post only |

Razer's own support pages were not reachable this session.

### Razer Cortex capture

- Not checked; Cortex is a game booster. No capture-card features found [unverified].

### AVerMedia (Streaming Center, Assist Central, RECentral, Gaming Utility)

| Feature | Status | Source |
|---|---|---|
| Streaming Center: Record, Record HDR, full-screen preview, HDR tone mapping and brightness adjustment | [fetched] | https://www.avermedia.com/product-detail/StreamingCenter |
| Overlays: window, image and text sources; picture-in-picture | [fetched] | same |
| Screenshot; scenes; "Director Mode" and "Easy Mode" switch | [fetched] | same |
| Audio: volume, EQ, compressor, listed as "Total Audio Control" | [fetched] | same |
| 4K60 HDR passthrough (system requirements) | [fetched] | same |
| Streaming Center Plug (OBS plugin); Assist Central Pro bundled with Live Gamer ULTRA S (GC553Pro) | [snippet] | https://www.businesswire.com/news/home/20250212917276/en/AVerMedia-Announces-the-Release-of-Its-Latest-Capture-Card-Live-Gamer-ULTRA-S-GC553Pro |
| Super Resolution (needs RTX 30/40/50 GPU); AV1 encode needs GPU AV1 support | [snippet] | https://www.avermedia.com/product-detail/StreamingCenter |
| RECentral 3 and 4 exist; RECentral 5 not found | [snippet] | https://www.avermedia.com/product-detail/GC570 |
| Latency claims | none found | — |
| Gaming Utility, Assist Central (non-Pro): not checked | [unverified] | — |

### Open-source references

- **OBS Studio, Video Capture Device (V4L2)**: Linux-only source. Its
  Buffering option, when unchecked, lowers delay; default on [snippet]:
  https://obsproject.com/kb/video-capture-sources . OBS is one pipeline that
  holds audio and video together, which the README already argues against.
- **llccpv** (GitHub, MIT): Qt low-latency capture preview, GPU shader
  path, YUV 4:2:2/4:2:0, FSR 1 upscaling; created 2026-04, 0 stars
  [snippet, repo not opened]: https://awesome.ecosyste.ms/projects/github.com%2Ftornadus%2Fllccpv
- **qvidcap**: basic V4L2 viewer, in Alpine repos [snippet]:
  https://pkgs.alpinelinux.org/package/edge/community/x86/qvidcap
- Flathub listing for a capture-card viewer: not found [unverified].
- guvcview (UVC viewer with recording): not checked this session [unverified].

## 2. Gap table

Feasibility notes assume the existing player (mpv, fed from `/dev/videoN`).
Latency cost is the added delay on the preview path; "none" means it is
off the path by design, not measured.

| Feature | Who has it | protoHDMIcapture today | Linux feasibility (open parts, concrete mechanism) | Value for console-play viewer | Why |
|---|---|---|---|---|---|
| Screenshot hotkey | All, incl. Elgato Studio, AVerMedia | Built in: mpv default binding `s` [fetched, mpv manual]; `screenshot-template` already set (repo README, mpv 0.41 note) | Already works. Add OSD confirmation and config for screenshot-directory in Lua | Med | Cheap; players grab frames for captions |
| Record to file (toggle) | Elgato Studio, 4KCU, AVerMedia, Razer (via OBS) | No | Requires one reader that owns the device: GStreamer `v4l2src ! tee`, preview branch to mpv or sink, record branch `queue leaky=downstream ! encoder ! mux ! filesink`. Or ffmpeg with split filter. See section 3 | High | Clips are what people buy these cards for |
| Replay / Flashback buffer | Elgato 4KCU (legacy), AVerMedia [unverified] | No | Rolling segments on the record branch (`splitmuxsink` max-size-time with delete of old files, or ffmpeg segment muxer `-f segment`, unverified) | Med | Keeps last N min for clips without deciding in advance |
| Instant preview (passthrough, no recorder in path) | Elgato Instant Gameview [snippet]; the design of this repo already | Yes: mpv direct from device | Already in place. Keep recorder off this path | High | Core of the product |
| Audio level meter, mute, volume hotkey | Razer Synapse, AVerMedia, Elgato Studio slider | Loopback runs; no meter or mute | PipeWire: `wpctl set-volume` / `wpctl set-mute` on the loopback sink; meter from `pw-record` or `pw-cli` peak; no video-path cost | Med | Console volume matters, and mic/gain balance on the card |
| Audio monitoring / mic commentary | Razer (Synapse), AVerMedia, Elgato Studio analog input | Card mic input is mixed into USB audio (README, mic jack) | Mic path is in card firmware; Linux side is just which source feeds `pw-loopback`. Adding a second source to the loopback is PipeWire routing | Low-med | Card already mixes mic in hardware |
| Colour: brightness, contrast, saturation, gamma | AVerMedia brightness adjustment, Elgato Studio colour range | `LEVELS` only | mpv video properties `brightness`, `contrast`, `saturation`, `gamma` (each -100..100, confirmed with `mpv --list-options` on 0.41.0); set via IPC or keys. GPU shader, no added frame latency | Med | Lets the user fix a washed-out SDR feed |
| HDR tone mapping | Elgato Studio (preview auto-mapped), AVerMedia | Off by PS5 setting (README: HDR off) | libplacebo in mpv `gpu-next`, `--tone-mapping` (confirmed on 0.41.0: auto, bt.2390, hable, …). Card delivers YUYV SDR-limited; HDR input not expected | Low | README recommends HDR off on source; no HDR path in the card's YUYV output |
| Colour range / EDID control | Elgato Studio: HDMI range, Input EDID mode, Custom EDID | `LEVELS` in software only | EDID is chip-side. MS2130 flash tools exist (see below), but no EDID tool found for the card. Risky | Low | The source is PS5 and the card's own EDID is what matters. Chip-specific, usually impossible from Linux |
| Overlays (text, image, PIP) | AVerMedia, Elgato Studio (via OBS only) | mpv OSD stats only (`i`/`I`) | mpv OSD or `osd-overlay` via IPC; or GStreamer `compositor`. Overlays on the preview path add a GPU draw per frame (README comments on overlay cost) | Low | Adds frame cost to the path that must stay lean |
| Hotkeys, rebindable | Elgato hotkeys page [snippet], Razer | f, d, i, I, q in Lua; IPC socket | `input.conf` / forced bindings in Lua with config keys | Med | Easy; reduces need for a terminal |
| Firmware update | Elgato Camera Hub [snippet], Razer Synapse | None | MS2130 only: ms-tools `read`/`write` on FLASH, experimental, no EDID or UVC XU features mentioned [fetched]. Flash tool risk; no vendor-signed firmware available | Low | Only a fix if firmware is broken; brick risk |
| Passthrough control | Razer, AVerMedia (4K60 HDR passthrough) | Passthrough is hardware, not controlled from Linux | Not controllable in software on MS2130 UVC [unverified] | Low | Passthrough path is already the TV's job |
| Multi-device | Elgato Studio (serial + firmware per device), AVerMedia | Card pinned by USB serial; one video window and one loopback (README) | Already does this. Add a device list in the wizard | Low | Single console use case |
| Per-console setup guides | Elgato help center; AVerMedia | PS5 settings in README; wizard | Already present for PS5 | Med | Matches the project's current scope |
| Stream Deck integration | Elgato Studio plugin [fetched] | None | Stream Deck is a generic HID device; would need a bridge that talks to mpv IPC (the mpv socket already exists). Not checked whether a Stream Deck plugin API fits | Low-med | Niche, but cheap where a deck is present: buttons can send JSON to the existing socket |
| Stream / broadcast | Elgato Studio, AVerMedia (via OBS) | None | Out of scope for a play viewer | Low | Not the product |

### MS2130 tooling check (asked for specifically)

- **ms-tools** (BertoldVdb): "Experimental support" for MS2130; commands
  `read`, `write`, `write-file`, `dump-rom`, `i2c-scan`, `gpio-*`; FLASH
  read/write documented. No EDID or UVC extension-unit access mentioned.
  Warns patch code may fail depending on firmware running. No flash-write
  risk warning in the README. https://github.com/BertoldVdb/ms-tools (fetched)
- **ms2130_patcher** (steve-m): firmware patch for MS2130 with 4K support,
  disables sharpening and scaling, register changes from hsdaoh. No frame
  rate or EDID changes in the README. Back up flash before flashing. Not
  verified against the actual patch source. https://github.com/steve-m/ms2130_patcher (fetched)
- **MISRC tools** (Stefan-Olt): raw capture from MS2130 via hsdaoh, bypassing
  on-chip processing [snippet, not opened]:
  https://raw.githubusercontent.com/Stefan-Olt/MISRC/main/misrc_tools/README.md
- **LinkPi / MS2131 patch post** on community.getchannels.com: the post
  itself labels its content as an unverified AI amalgamation. Do not use
  its firmware steps. https://community.getchannels.com/t/linkpi-encoder-family/38860?page=23

Conclusion: a UVC-controlled EDID or brightness path through the vendor
extension unit was not found in any source. The only written tools touch
flash and the chip's registers, so EDID and firmware features are chip-level
and high risk. Treat them as out of scope for this project.

## 3. Ranked shortlist

Ranked by value for console play, cost, and risk to the preview path.

1. **Screenshot confirm and folder** (mpv `s`, already bound). Sketch: set
   `screenshot-directory` in config and show OSD text in
   `lib/mpv/protohdmicapture.lua` on the screenshot event. Latency cost: none.
2. **Record toggle, off the preview path.** Needs one owner of the device.
   Sketch: a supervisor process opens `/dev/videoN` once; GStreamer
   `v4l2src ! tee name=t`, branch A feeds mpv (or a v4l2loopback device, see
   below), branch B `queue leaky=downstream max-size-buffers=N ! encoder ! mux ! filesink`.
   Latency cost: none on branch A if the record queue is leaky. Encoding can
   still starve the CPU; measure with the existing `DEBUG=log` perf line.
3. **Replay buffer (Flashback equivalent).** Same tee; rolling segments
   via `splitmuxsink` with max-size-time and deletion of old files. Note
   that segments must cut on keyframes. Latency cost: none on preview.
4. **Audio level meter, mute and volume hotkeys** on the loopback sink with
   `wpctl`/PipeWire. Latency cost: none.
5. **Colour adjust keys** (brightness, contrast, saturation, gamma) via mpv
   properties and IPC. Shader cost is small; measure fps with `i`.
6. **Rebindable hotkeys** from a config file, so the Lua script needs no
   edits. Latency cost: none.
7. **Overlay toggle** (text OSD: mode, fps, drops, audio state) via mpv OSD,
   not a second video layer. Latency cost: small GPU draw; keep off by default.
8. **Wizard device list and per-console tips** extended beyond PS5 (Xbox,
   Switch, PC). Latency cost: none. Documentation work only.

Not shortlisted: EDID, firmware, HDR tone mapping (source-side HDR is off by
design), Stream Deck (niche), stream output (out of scope).

### Latency measurement for the recorder

- **V4L2 multi-open (verified from kernel source):** UVC allows multiple
  `open()` calls. All handles share one `stream->queue`. Source:
  `drivers/media/usb/uvc/uvc_v4l2.c`, `uvc_v4l2_open` allocates a per-file
  handle and attaches it to the shared stream
  (https://raw.githubusercontent.com/torvalds/linux/master/drivers/media/usb/uvc/uvc_v4l2.c).
  `vb2_core_reqbufs` returns -EBUSY while the queue is streaming, and
  `vb2_thread_start` returns -EBUSY when `vb2_is_busy` (
  https://raw.githubusercontent.com/torvalds/linux/master/drivers/media/common/videobuf2/videobuf2-core.c).
  So a second process cannot stream the same device while mpv is reading it.
  The recorder has to be inside the same process as the preview, or read a
  fan-out device.
- **Fan-out via v4l2loopback** (single reader, writes a loopback device;
  mpv reads the loopback): one extra copy per frame. Latency cost not
  measured [unverified]; test with the film-the-screen method in the README.
- **No second reader of the physical device** is possible, so option 2 is
  the only route without loopback.

## 4. Sources

- https://help.elgato.com/hc/en-us/articles/360027963752 (4KCU help) [snippet]
- https://help.elgato.com/hc/en-us/articles/11395449701773-Elgato-4K-Capture-Utility-Hotkeys-and-Key-Binds [snippet]
- https://help.elgato.com/hc/en-us/articles/360060805031-Elgato-4K-Capture-Utility-Supported-Capture-Devices [snippet]
- https://help.elgato.com/hc/zh-tw/articles/360060805031-Elgato-4K-Capture-Utility-%E6%94%AF%E6%8F%B4%E7%9A%84%E6%93%B7%E5%8F%96%E8%A3%9D%E7%BD%AE [snippet]
- https://help.elgato.com/hc/en-us/articles/26006411476365-Elgato-4K-Capture-Utility-1-7-12-Release-Notes [snippet]
- https://help.elgato.com/hc/en-us/articles/360028241191-Elgato-4K-Capture-Utility-and-Elgato-Game-Capture-Devices [snippet]
- https://help.elgato.com/hc/en-us/articles/37231557626129-Elgato-Studio-Software-Interface [snippet]
- https://www.elgato.com/ch/en/explorer/products/capture/elgato-studio-software-overview/ [fetched]
- https://help.elgato.com/hc/en-us/articles/360027964072-Elgato-Game-Capture-HD-Software-Release-Notes-Windows [snippet]
- https://help.elgato.com/hc/en-us/articles/360027951012-Newer-Elgato-Game-Capture-HD-software-works-with-original-Elgato-Game-Capture-HD-hardware [snippet]
- https://help.elgato.com/hc/en-us/articles/4417243600909-Elgato-Camera-Hub-Supported-Devices [unverified, not opened]
- https://techpowerup.com/forums/threads/293621 [snippet]
- https://zoftwarehub.com/products/elgato-game-capture/features [snippet]
- https://www.guru3d.com/news-story/razer-ripsaw-hd-video-capture-box.html [snippet]
- https://www.windowscentral.com/how-set-and-use-razer-ripsaw-hd [snippet]
- https://www.tomsguide.com/us/razer-ripsaw-hd%2Creview-6388.html [snippet]
- https://www.windowscentral.com/razer-ripsaw-review [snippet]
- https://insider.razer.com/razer-synapse-4-55/synapse-4-can-t-adjust-ripsaw-hd-s-volume-69061 [snippet]
- https://insider.razer.com/razer-support-45/ripsaw-drivers-27766 [fetched, support post only]
- https://www.avermedia.com/product-detail/StreamingCenter [fetched]
- https://www.avermedia.com/product-detail/GC570 [snippet]
- https://www.businesswire.com/news/home/20250212917276/en/AVerMedia-Announces-the-Release-of-Its-Latest-Capture-Card-Live-Gamer-ULTRA-S-GC553Pro [snippet]
- https://www.guru3d.com/news-story/corsair-launches-icue-unified-software.html [snippet]
- https://obsproject.com/kb/video-capture-sources [snippet]
- https://awesome.ecosyste.ms/projects/github.com%2Ftornadus%2Fllccpv [snippet]
- https://pkgs.alpinelinux.org/package/edge/community/x86/qvidcap [snippet]
- https://www.narcosis-the-game.com/?p=6680 [unverified, third-party]
- https://mpv.io/manual/stable/ [fetched in part: `screenshot` default key `s`, `--dump-stats`, `--profile=low-latency`]
- https://github.com/BertoldVdb/ms-tools [fetched]
- https://github.com/steve-m/ms2130_patcher [fetched]
- https://raw.githubusercontent.com/Stefan-Olt/MISRC/main/misrc_tools/README.md [snippet]
- https://community.getchannels.com/t/linkpi-encoder-family/38860?page=23 [snippet, flagged unverified by its author]
- https://docs.kernel.org/userspace-api/media/v4l/func-open.html [fetched, no multi-open rule stated]
- https://docs.kernel.org/userspace-api/media/v4l/io.html [fetched, no exclusivity rule stated]
- https://raw.githubusercontent.com/torvalds/linux/master/drivers/media/usb/uvc/uvc_v4l2.c [fetched]
- https://raw.githubusercontent.com/torvalds/linux/master/drivers/media/common/videobuf2/videobuf2-core.c [fetched]

## Not verified (to check in a browser)

- Elgato 4KCU Flashback duration, hotkeys and Instant Gameview specifics
  (help pages returned 403).
- Current 4KCU and Studio feature lists after 1.7.16.
- Razer and AVerMedia manuals (not reachable; Razer GameView not found).
- AVerMedia RECentral 5, Gaming Utility, Assist Central (non-Pro).
- Exact mpv option names for tone mapping and for `--stream-record`
  (mpv manual page was too long to parse in full).
- ffmpeg segment muxer option names; splitmuxsink property names.
- v4l2loopback latency cost; all latency numbers in this note are
  unmeasured.
