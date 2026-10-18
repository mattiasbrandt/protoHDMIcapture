# protoHDMIcapture

A low-latency viewer for playing an HDMI device through a cheap USB capture
card on Linux. It shows the picture and plays the sound; it does not record
or stream.

## Language

**Capture card**:
The USB device that turns the source's HDMI signal into video and audio the
PC can read. "Card" alone is fine in code; prose says capture card.
_Avoid_: dongle, capture device, camera

**Ignored device**:
A video device on the PC that is not a capture card (a webcam, a laptop
camera) and that the viewer never opens as one. Marked in the wizard, kept in
the saved settings.
_Avoid_: blocklist, blacklist

**Chip**:
The capture silicon inside a capture card (MS2109, MS2130). Many no-name
products share one chip, often under borrowed USB ids.
_Avoid_: model, brand

**Capture card profile**:
What the project knows about one chip: the USB ids it appears under, the
link it needs, its usable modes, and quirks tied to the id they were seen on.
_Avoid_: card database, chip table

**Support status**:
How much the project has seen of a USB id. **Tested**: a real source played
end to end (picture, audio, sync). **Probed**: plugged in and its modes
measured, no source played. **Reported**: known only from vendor docs or
third parties.
_Avoid_: verified, supported, compatible

**Source**:
The HDMI device plugged into the capture card: a console, a camera, another PC.
_Avoid_: console (too narrow), device, input

**Card audio input**:
The audio input the capture card exposes to the desktop's sound system; it carries
the source's sound.
_Avoid_: source (that is the HDMI device), mic

**Source profile**:
What the project knows about one kind of source: setup tips for its own
menus, and suggested viewer settings. Chosen in the wizard, browsable in the
overlay.
_Avoid_: preset, console profile

**Setup tips**:
The source-side settings a person changes on the source itself (HDCP, audio
format, HDR, resolution) so the capture card gets a clean signal.
_Avoid_: recommendations, guide

**Feed**:
The live picture from the capture card, shown in the viewer's window.
_Avoid_: stream, preview, playback

**No signal**:
The capture card delivers no picture, either since the feed opened or
because the source stopped (asleep, switched off). A card that keeps sending
black frames is still delivering a picture. Frames that arrive but cannot be
shown (MJPEG that does not decode, short or flagged buffers) are no picture
either; the viewer says which. Shown after 5 s with no picture since the feed
opened, or 2 s after the last one.
_Avoid_: stall, frozen, signal loss

**Overlay**:
The in-window control panel, opened on a key, that lists the keys, the live
settings with their current values, and the setup tips.
_Avoid_: HUD, OSD, menu

**Audio sync**:
The fixed delay added to the source's sound so it lines up with the feed.
One value, tuned once and kept; re-tune it after changing the capture card
or the display.
_Avoid_: latency, offset, A/V delay

**Setting**:
A named value that shapes a run, such as the levels, the audio sync or the
capture card pin; taken from the environment, else the saved settings, else
its default (started from the menu, the Rust viewer's menu default sits
before the default: `REFRESH=match`).
_Avoid_: knob, key, option

**Saved settings**:
The settings a run starts with, kept in the user's config file. Changed by
the wizard, by hand, or by an explicit save from the overlay.
_Avoid_: preferences, profile (that is a source profile)

**Live change**:
A setting changed while the feed runs; it lasts until the viewer closes
unless it is saved.
_Avoid_: temporary setting, override

**Dashboard**:
The diagnostics view in its own terminal: frame rates, drops, repeated
refreshes, latency, CPU, the audio's delay, underruns and xruns, the
monitors, and recent events. Never part of the overlay.
_Avoid_: debug window, stats

**Repeated refresh**:
A monitor refresh that showed the previous card frame again because no new
one was ready: a dropped or late frame, seen as judder.
_Avoid_: stutter, frame skip
