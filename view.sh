#!/usr/bin/env bash
# protoHDMIcapture: low-latency HDMI capture card viewer for the desktop.
#
# Architecture: video and audio are DELIBERATELY separate pipelines.
#   video -> mpv/gst/ffplay, untimed, no audio track, no A/V sync machinery
#   audio -> PipeWire loopback straight from the card's capture source to your
#            output sink, with an explicit delay used to line it up with video
#
# That split is the whole point. A single pipeline (OBS, or mpv with an audio
# track) has to reconcile two clocks that drift, and it buys that sync with
# buffer. Here nothing reconciles anything: video is shown the instant it
# arrives, audio is delayed by one fixed number you tune once.
#
# Usage:
#   ./view.sh                 # auto-detect, mpv backend, loopback audio
#   BACKEND=gst ./view.sh     # GStreamer backend
#   DELAY=0.08 ./view.sh      # audio delayed 80 ms to match video
#   ./view.sh --tune          # interactive A/V sync tuner
#
# Every knob is an env var; see the block below.

set -uo pipefail

HERE=$(dirname "$(readlink -f "$0")")
# shellcheck source=lib/card.sh
. "$HERE/lib/card.sh"
# shellcheck source=lib/desktop.sh
. "$HERE/lib/desktop.sh"

# --- configuration ---------------------------------------------------------
# Precedence: environment, then the config file (see config.example), then the
# defaults below.
load_config
DEV="${DEV:-}"                  # /dev/videoN  (empty = find the card)
SRC="${SRC:-}"                  # PipeWire source of the card's audio (auto)
CARD_SERIAL="${CARD_SERIAL:-}"  # USB serial of the card; pins it among other cameras
IGNORE_CARDS_RE="${IGNORE_CARDS_RE:-}"  # without CARD_SERIAL: card types to skip, e.g. 'OBSBOT|Integrated Camera'
SINK="${SINK:-}"                # PipeWire sink to hear it on (empty = default)
RES="${RES:-1920x1080}"         # capture resolution
FPS="${FPS:-60}"                # capture framerate
# yuyv422 is confirmed correct for this card: uncompressed, 1080p60, no decode
# step, and its MJPEG path is broken ("No JPEG data found in image").
# Empty means "leave the device on whatever it was last set to" — which is luck,
# not a setting, so do not default to empty.
FMT="${FMT-yuyv422}"            # yuyv422 | mjpeg | rawvideo | nv12 | "" to leave as-is
DELAY="${DELAY:-0.05}"          # seconds of audio delay to match video latency
LAT_MS="${LAT_MS:-10}"          # PipeWire loopback buffer, ms
BACKEND="${BACKEND:-mpv}"       # mpv | gst | ffplay
AUDIO="${AUDIO:-loopback}"      # loopback | mpv | none
FULLSCREEN="${FULLSCREEN:-yes}" # yes | no  (fullscreens on whatever monitor it opens on)
SCALING="${SCALING:-balanced}"  # balanced | fast | config  (see the mpv block below)
LEVELS="${LEVELS:-auto}"        # auto | limited | full  (fix for crushed/washed blacks)
IPC="${IPC-/tmp/mpv-protohdmicapture.sock}"  # mpv IPC socket; empty to disable
# keep  = leave the display alone (default)
# match = for the session only, switch the monitor the window opens on to its
#         mode closest to FPS, then restore on exit (Hyprland only). 60 fps into a 100 Hz panel
#         holds frames for 1 or 2 refreshes alternately, which judders no matter
#         how clean the capture path is. No output is hardcoded — whichever
#         monitor is focused when you launch is the one that changes.
REFRESH="${REFRESH:-keep}"      # keep | match
GST_IO_MODE="${GST_IO_MODE:-mmap}"   # mmap | dmabuf  (dmabuf is zero-copy but not all cards do it)

die() { printf '\033[31m!! %s\033[0m\n' "$*" >&2; exit 1; }
info() { printf '\033[36m:: %s\033[0m\n' "$*"; }
warn() { printf '\033[33m** %s\033[0m\n' "$*" >&2; }

# --- resolve the card ------------------------------------------------------
resolve_card || die "$CARD_ERROR"
info "video device: $DEV ($CARD_NAME)"
if [ "$AUDIO" != "none" ]; then
    resolve_audio_source || die "$CARD_ERROR"
    info "audio source: $SRC"
fi

# --- audio path: PipeWire loopback ----------------------------------------
LOOPBACK_PID=""
start_audio() {
    [ "$AUDIO" = "loopback" ] || return 0
    stop_audio
    # -C capture source, -P playback sink (omitted = follow default sink),
    # --latency = loopback buffer, --delay = deliberate offset to match video.
    pw-loopback -n protohdmicapture-audio \
        -C "$SRC" ${SINK:+-P "$SINK"} \
        --latency "$LAT_MS" \
        --delay "$DELAY" >/dev/null 2>&1 &
    LOOPBACK_PID=$!
    info "audio loopback up (pid $LOOPBACK_PID, delay ${DELAY}s, buffer ${LAT_MS}ms)"

    # pw-loopback silently falls back to the DEFAULT source if -C names a node
    # that is not currently available, and then you are listening to your
    # microphone instead of the HDMI source with no error anywhere. Verify the link
    # actually landed where we asked.
    # `pw-link -li` prints each input port followed by its connections:
    #     input.protohdmicapture-audio:input_FL
    #       |<- <source-node>:capture_FL
    # so take the line after ours and strip the arrow and the port suffix.
    sleep 0.5
    local linked
    linked=$(pw-link -li 2>/dev/null \
             | grep -A1 '^input\.protohdmicapture-audio:input_FL$' | sed -n '2p' \
             | sed 's/^[[:space:]]*|<-[[:space:]]*//; s/:[^:]*$//')
    if [ -n "$linked" ] && [ "$linked" != "$SRC" ]; then
        printf '\033[33m** audio is linked to "%s", not the requested "%s"\033[0m\n' "$linked" "$SRC" >&2
        printf '\033[33m** the requested source was probably unavailable; check it with: pactl list short sources\033[0m\n' >&2
    fi
}
stop_audio() {
    if [ -n "$LOOPBACK_PID" ] && kill -0 "$LOOPBACK_PID" 2>/dev/null; then
        kill "$LOOPBACK_PID" 2>/dev/null
        wait "$LOOPBACK_PID" 2>/dev/null
    fi
    LOOPBACK_PID=""
}
trap 'stop_audio; desktop_refresh_restore' EXIT INT TERM

# --- display refresh matching ---------------------------------------------
start_refresh() {
    [ "$REFRESH" = "match" ] || return 0
    if desktop_refresh_match "$FPS"; then info "$DESKTOP_MSG"; else warn "$DESKTOP_MSG"; fi
}

# --- interactive A/V sync tuner -------------------------------------------
# Restarts the loopback with a new delay on each keypress. Video keeps running
# in another terminal / window; you only nudge audio until it lines up.
if [ "${1:-}" = "--tune" ]; then
    [ "$AUDIO" = "loopback" ] || die "--tune needs AUDIO=loopback"
    start_audio
    cat <<'EOF'

  A/V sync tuner. Start the video in another terminal:
      AUDIO=none ./view.sh

  Then, watching something with sharp transients (console menu clicks are ideal):
      j / k   audio 10 ms earlier / later
      h / l   audio 50 ms earlier / later
      0       reset to 0
      q       quit and print the DELAY value to keep

EOF
    while true; do
        printf '\r  DELAY = %-8s ' "$DELAY"
        IFS= read -rsn1 key || break
        case "$key" in
            j) DELAY=$(awk -v d="$DELAY" 'BEGIN{v=d-0.010; if(v<0)v=0; printf "%.3f", v}') ;;
            k) DELAY=$(awk -v d="$DELAY" 'BEGIN{printf "%.3f", d+0.010}') ;;
            h) DELAY=$(awk -v d="$DELAY" 'BEGIN{v=d-0.050; if(v<0)v=0; printf "%.3f", v}') ;;
            l) DELAY=$(awk -v d="$DELAY" 'BEGIN{printf "%.3f", d+0.050}') ;;
            0) DELAY=0.000 ;;
            q) echo; info "keep this: DELAY=$DELAY"; exit 0 ;;
            *) continue ;;
        esac
        start_audio
    done
    exit 0
fi

# --- video path ------------------------------------------------------------
start_audio
start_refresh

W="${RES%x*}"; H="${RES#*x}"

case "$BACKEND" in
mpv)
    # --profile=low-latency sets: audio-buffer=0, vd-lavc-threads=1,
    #   demuxer-lavf-o-add=fflags=+nobuffer, probe-info=nostreams,
    #   analyzeduration=0.1, video-sync=audio, interpolation=no,
    #   video-latency-hacks=yes, stream-buffer-size=4k
    # --untimed shows each frame the moment it arrives. It is only safe
    #   because there is no audio track in this pipeline.
    # --swapchain-depth=1 stops the GPU queueing a second frame ahead.
    # timestamps=abs puts v4l2 on the wall clock so it shares a time base with
    # the pulse input — needed ONLY for AUDIO=mpv. Everywhere else it just makes
    # the timeline read as seconds-since-epoch (496860:54:40), so leave it off.
    LAVF_O="video_size=${RES},framerate=${FPS}"
    [ -n "$FMT" ] && LAVF_O="${LAVF_O},input_format=${FMT}"
    [ "$AUDIO" = "mpv" ] && LAVF_O="timestamps=abs,${LAVF_O}"

    # ~/.config/mpv/mpv.conf is tuned for films and actively fights this
    # workload, so the settings that matter are overridden explicitly on the
    # command line (CLI beats config). Each override below names what it undoes.
    args=(
        "av://v4l2:$DEV"
        --demuxer-lavf-o="$LAVF_O"
        --profile=low-latency
        --vo=gpu-next
        --swapchain-depth=1
        --keepaspect=yes
        --cursor-autohide=200
        --osc=no
        --title="protoHDMIcapture"

        # undoes hwdec=auto-safe — YUYV is uncompressed, there is nothing to decode
        --hwdec=no
        # undoes target-colorspace-hint=auto — HDR negotiation is pointless for an
        # SDR capture and can shift colours
        --target-colorspace-hint=no
        # undoes keep-open=yes — if the source signal drops, exit instead of sitting
        # paused on a stale frame
        --keep-open=no
        # undoes save-position-on-quit — a resume position for a live device is
        # meaningless and just litters watch_later
        --save-position-on-quit=no

        # window appears immediately instead of waiting for the first frame
        --force-window=immediate
        # never start paused, whatever the config or a stale state says
        --pause=no
        # do not steal stdin from the terminal you launched this in
        --input-terminal=no
        # a live feed generates no input events, so idle-blanking would otherwise
        # kick in mid-game
        --stop-screensaver=always
    )
    [ -n "$IPC" ] && args+=(--input-ipc-server="$IPC")
    [ "$LEVELS" != "auto" ] && args+=(--video-output-levels="$LEVELS")
    [ "$FULLSCREEN" = "yes" ] && args+=(--fs)

    # Scaling. mpv.conf sets profile=high-quality (ewa_lanczossharp + debanding
    # + antiringing); --profile=low-latency does NOT undo that, so it survives
    # into this pipeline and costs real GPU frame time on every frame.
    case "$SCALING" in
    balanced) # good upscale, a fraction of high-quality's cost
        args+=(--scale=spline36 --cscale=spline36 --dscale=mitchell --deband=no --dither=no) ;;
    fast)     # bilinear everything; lowest GPU time, visibly softer
        args+=(--profile=fast) ;;
    config)   # leave mpv.conf's high-quality settings alone
        ;;
    *) die "unknown SCALING=$SCALING (balanced | fast | config)" ;;
    esac

    if [ "$AUDIO" = "mpv" ]; then
        # Single-pipeline fallback: mpv owns both streams and does the sync.
        # Higher latency than the loopback path, but nothing to tune.
        # --untimed is unsafe with an audio track, so it is omitted here.
        args+=(--audio-file="av://pulse:$SRC" --audio-delay=0)
    else
        args+=(--no-audio --untimed)
    fi

    info "mpv ${args[*]}"
    # MANGOHUD=1 is set globally in ~/.config/environment.d/99-proton-amd.conf and
    # MangoHud is an *implicit* Vulkan layer, so it injects into gpu-next and adds
    # an overlay draw to every frame. Not wanted in a latency path.
    # The VIDIOC_QBUF line is libavdevice's noisy v4l2 teardown on exit; it is
    # harmless (verified: no re-enumeration) and only that exact line is dropped.
    MANGOHUD=0 mpv "${args[@]}" \
        2> >(grep -vE 'ioctl\(VIDIOC_QBUF\): Inappropriate ioctl for device' >&2)
    ;;

gst)
    # sync=false on the sink is the key knob: render on arrival, do not wait
    # for a presentation timestamp. queue leaky=downstream drops rather than
    # accumulates if the sink ever falls behind, so latency cannot creep.
    if [ "$FMT" = "mjpeg" ]; then
        CAPS="image/jpeg,width=${W},height=${H},framerate=${FPS}/1"
        DECODE="jpegdec ! "
    else
        CAPS="video/x-raw,width=${W},height=${H},framerate=${FPS}/1"
        DECODE=""
    fi
    PIPE="v4l2src device=${DEV} io-mode=${GST_IO_MODE} do-timestamp=true ! ${CAPS} ! ${DECODE}queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! videoconvert ! waylandsink sync=false async=false qos=false enable-last-sample=false"
    info "gst-launch-1.0 $PIPE"
    gst-launch-1.0 -e $PIPE
    ;;

ffplay)
    args=(
        -hide_banner -loglevel warning
        -f v4l2
        -video_size "$RES" -framerate "$FPS"
        -fflags nobuffer -flags low_delay
        -probesize 32 -analyzeduration 0
        -framedrop -vf setpts=0
        -an
        -window_title "protoHDMIcapture"
    )
    [ -n "$FMT" ] && args=(-input_format "$FMT" "${args[@]}")
    [ "$FULLSCREEN" = "yes" ] && args+=(-fs)
    info "ffplay ${args[*]} $DEV"
    ffplay "${args[@]}" "$DEV"
    ;;

*)
    die "unknown BACKEND=$BACKEND (mpv | gst | ffplay)"
    ;;
esac
