#!/usr/bin/env bash
# Probe a freshly connected USB capture card: identify it, dump every capture
# mode it offers, find its audio side, and print ready-to-paste view commands.
#
# Run with the card plugged in AND the HDMI source (console, PC, ...) powered on and outputting a signal.
# Some cards only expose their real mode list while a source is locked.
#
#   ./probe.sh                 # auto-detect (ignores known non-capture cams)
#   ./probe.sh /dev/video4     # force a specific node

set -uo pipefail

# Video nodes belonging to devices we know are NOT the capture card.
# Card type strings for the OBSBOT webcam, which is NOT the capture card.
# Do not add "UVC Camera (1e4e:7016)" here: despite the Cubeternet vendor id,
# that IS the capture card (serial 20000130041415, MacroSilicon inside).
IGNORE_CARDS_RE='OBSBOT|Meet 2|Integrated Camera'

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
hr()   { printf '%s\n' "----------------------------------------------------------------"; }

bold "== USB tree (look for the capture card, note its Bus/Device) =="
lsusb
hr

bold "== USB link speed of each connected device =="
# 5000/10000 = USB3 (needed for uncompressed 1080p60); 480 = USB2 (MJPEG only).
for d in /sys/bus/usb/devices/[0-9]*-[0-9]*; do
    [ -r "$d/speed" ] || continue
    [ -r "$d/product" ] || continue
    printf '  %-40s %6s Mbit/s\n' "$(cat "$d/product")" "$(cat "$d/speed")"
done
hr

bold "== Capture chip identification =="
# The blue 'USB3.0 Video Capture HDMI 4K' box is a MacroSilicon reference
# design. Which chip decides whether uncompressed YUYV at 1080p60 is available.
FOUND_CHIP=0
for d in /sys/bus/usb/devices/[0-9]*-[0-9]*; do
    [ -r "$d/idVendor" ] || continue
    vid=$(cat "$d/idVendor"); pid=$(cat "$d/idProduct" 2>/dev/null)
    case "$vid:$pid" in
    534d:2109)
        FOUND_CHIP=1
        echo "  $vid:$pid -> MacroSilicon MS2109 (USB 2.0)"
        echo "     * MJPEG is effectively mandatory: YUYV manages ~5 fps at 1080p."
        echo "     * Audio firmware bug (96kHz-mono / swapped channels) is fixed"
        echo "       in-kernel; nothing to do on $(uname -r)."
        ;;
    345f:2130|534d:2130|534d:0021)
        FOUND_CHIP=1
        echo "  $vid:$pid -> MacroSilicon MS2130 (USB 3.0)"
        echo "     * Prefer UNCOMPRESSED yuyv422 at 1080p60 — no JPEG decode step."
        echo "     * Verify the link speed above reads 5000, not 480."
        ;;
    1e4e:7016)
        FOUND_CHIP=1
        echo "  $vid:$pid -> THIS BOX'S CAPTURE CARD (serial 20000130041415)"
        echo "     The 1e4e 'Cubeternet' vendor id is borrowed; the silicon is"
        echo "     MacroSilicon. It reports no manufacturer/product strings."
        echo "     * On a USB 2 port it enumerates at 480 and reset-loops badly."
        echo "       Confirm the link speed below reads 5000 (SuperSpeed)."
        echo "     * 'sticky mixer values ... disabling' in dmesg is expected."
        ;;
    534d:*|345f:*)
        FOUND_CHIP=1
        echo "  $vid:$pid -> MacroSilicon family, exact chip unknown"
        echo "     * Go by the format list below, not by the model number."
        ;;
    esac
    [ "$FOUND_CHIP" = 1 ] && printf '     link speed: %s Mbit/s  (5000 = USB3, 480 = USB2)\n' "$(cat "$d/speed" 2>/dev/null)"
done
[ "$FOUND_CHIP" = 0 ] && echo "  (no MacroSilicon device seen — note the ID from the lsusb list above)"
hr

bold "== V4L2 devices =="
v4l2-ctl --list-devices
hr

# --- pick the capture node -------------------------------------------------
DEV="${1:-}"
if [ -z "$DEV" ]; then
    for n in /dev/video*; do
        [ -e "$n" ] || continue
        card=$(v4l2-ctl -d "$n" --info 2>/dev/null | sed -n 's/^\s*Card type\s*:\s*//p')
        [ -n "$card" ] || continue
        echo "$card" | grep -Eq "$IGNORE_CARDS_RE" && continue
        # Must actually be a video-capture node (metadata nodes are not).
        v4l2-ctl -d "$n" --all 2>/dev/null | grep -q 'Video Capture' || continue
        DEV="$n"
        break
    done
fi

if [ -z "$DEV" ]; then
    echo "!! No capture node found. Plug the card in, or pass one explicitly:"
    echo "     $0 /dev/videoN"
    exit 1
fi

CARD=$(v4l2-ctl -d "$DEV" --info 2>/dev/null | sed -n 's/^\s*Card type\s*:\s*//p')
bold "== Selected capture device: $DEV  ($CARD) =="
hr

bold "== Device info =="
v4l2-ctl -d "$DEV" --info
hr

bold "== Supported formats / resolutions / framerates =="
echo "(This is the authoritative list. Pick a mode from here for FMT/RES/FPS.)"
v4l2-ctl -d "$DEV" --list-formats-ext
hr

bold "== Currently negotiated format =="
v4l2-ctl -d "$DEV" --get-fmt-video
v4l2-ctl -d "$DEV" --get-parm 2>/dev/null
hr

bold "== Signal detection (is the source actually locked in?) =="
v4l2-ctl -d "$DEV" --get-dv-timings 2>&1 | head -20
v4l2-ctl -d "$DEV" --query-dv-timings 2>&1 | head -20
hr

bold "== Driver controls (some cards expose HDR/colour-range knobs here) =="
v4l2-ctl -d "$DEV" --list-ctrls 2>&1 | head -40
hr

bold "== Audio: PipeWire sources =="
echo "(The card's audio input is the one whose name matches the card.)"
pactl list short sources
hr

bold "== Audio: raw ALSA capture devices =="
arecord -l 2>&1
hr

bold "== Audio: PipeWire sinks (where you want to hear it) =="
pactl list short sinks
hr

# --- audio source match ----------------------------------------------------
# Match on the card's USB serial, which appears verbatim in the PipeWire node
# name (alsa_input.usb-1e4e_7016_20000130041415-02...). Picking "the first USB
# source" instead grabs whatever headset happens to enumerate first.
CARD_SERIAL=$(v4l2-ctl -d "$DEV" --info 2>/dev/null | sed -n 's/^[[:space:]]*Serial[[:space:]]*:[[:space:]]*//p' | head -1)
SRC=""
if [ -n "$CARD_SERIAL" ]; then
    SRC=$(pactl list short sources 2>/dev/null \
          | awk '$2 !~ /\.monitor$/ {print $2}' \
          | grep -F "$CARD_SERIAL" | head -1)
fi
if [ -n "$SRC" ]; then
    echo "  matched the card's audio by serial $CARD_SERIAL:"
    echo "    $SRC"
else
    echo "  !! could not match an audio source to serial '${CARD_SERIAL:-<none>}'"
    echo "     pick it by hand from the source list above."
fi
hr

bold "== Suggested next step =="
echo
echo "  Video-only smoke test (no audio, safest first run):"
echo
echo "    mpv av://v4l2:$DEV --profile=low-latency --untimed --no-audio"
echo
echo "  Then the full launcher. Try UNCOMPRESSED first (no JPEG decode step):"
echo
printf "    DEV=%s \\\\\n" "$DEV"
[ -n "$SRC" ] && printf "    SRC=%s \\\\\n" "$SRC"
echo "    RES=1920x1080 FPS=60 FMT=yuyv422 \\"
echo "    ./view.sh"
echo
echo "  If the format list above shows YUYV capped at a low framerate at 1080p"
echo "  (the USB 2.0 / MS2109 case), use FMT=mjpeg instead."
echo
echo "  Paste the 'Supported formats' block above back to Claude — the right"
echo "  FMT/RES/FPS combination is card-specific and that list decides it."
