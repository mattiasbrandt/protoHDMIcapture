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
# shellcheck source=lib/debug.sh
. "$HERE/lib/debug.sh"

# --- mode --------------------------------------------------------------------
# --setup hands over before the config is even read, so a config that would
# make this script die can still be fixed by the wizard.
if [ "${1:-}" = "--setup" ]; then
    shift
    exec "$HERE/lib/setup.sh" "$@"
fi
MODE=play
for _arg in "$@"; do
    case "$_arg" in
        --tune) MODE=tune ;;
        -h|--help)
            echo "usage: ./view.sh [--setup | --tune]   (knobs: env vars or the config file)"
            exit 0 ;;
        *) echo "view.sh: unknown option $_arg (try --help)" >&2; exit 2 ;;
    esac
done

# --- first run -------------------------------------------------------------
# No config file yet, and someone at a terminal: offer the wizard. Declining
# leaves a config file that only says so, so this asks once. The menu path
# (no terminal) is handled by launch.sh, which opens a terminal for it.
if [ "$MODE" = play ] && [ ! -e "$CONFIG_FILE" ] && [ -z "${PROTOHDMI_SETUP_CHILD:-}" ] \
   && [ -z "${NO_SETUP:-}" ] && [ -t 0 ] && [ -t 1 ]; then
    printf '\033[1mNo settings yet.\033[0m Run the setup wizard now? [Y/n] '
    read -r _reply || _reply=n
    if [[ "$_reply" =~ ^[Nn] ]]; then
        if config_write; then
            printf '# Setup was skipped; run ./view.sh --setup to choose settings.\n' >>"$CONFIG_FILE"
        fi
    else
        "$HERE/lib/setup.sh" --then-play
        [ "$(cat "${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture/setup.done" 2>/dev/null)" = saved ] || exit 0
    fi
fi

# --- configuration ---------------------------------------------------------
# Precedence: environment, then the config file (see config.example), then the
# defaults below.
debug_snapshot_env "${CONFIG_KEYS[@]}"
load_config
debug_snapshot_file "${CONFIG_KEYS[@]}"

# Player knobs you set yourself (environment or config file), as opposed to
# defaults. Only these earn a warning when the backend cannot honour them, so
# BACKEND=gst does not nag about the default SCALING.
#
# FULLSCREEN=yes|no predates WINDOW and still works; WINDOW wins if both are
# set. Translated here, before the scan, so a migrated value counts as yours.
DEPRECATED=""; DEPRECATED_BAD=""
if [ -n "${FULLSCREEN:-}" ]; then
    if [ -n "${WINDOW:-}" ]; then
        DEPRECATED="FULLSCREEN=$FULLSCREEN is ignored because WINDOW=$WINDOW is set; remove FULLSCREEN"
    else
        case "$FULLSCREEN" in
            yes) WINDOW=fullscreen ;;
            no)  WINDOW=windowed ;;
            *)   DEPRECATED_BAD="FULLSCREEN=$FULLSCREEN is not yes or no (FULLSCREEN is deprecated; use WINDOW=fullscreen | borderless | windowed)" ;;
        esac
        [ -n "$DEPRECATED_BAD" ] || DEPRECATED="FULLSCREEN is deprecated; use WINDOW=$WINDOW"
    fi
fi
REQUESTED=" "
for _knob in FMT WINDOW WINDOW_SIZE SCALING LEVELS IPC GST_IO_MODE; do
    [ -n "${!_knob:-}" ] && REQUESTED+="$_knob "
done
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
# Display mode, as games present it. fullscreen covers the monitor it opens on;
# borderless is a maximized window without decorations (the desktop's bar
# stays visible); windowed is a normal window of WINDOW_SIZE. Only fullscreen
# gets direct scanout and tearing, so it is the lowest-latency mode.
WINDOW="${WINDOW:-fullscreen}"  # fullscreen | borderless | windowed
WINDOW_SIZE="${WINDOW_SIZE:-1280x720}"  # WxH, for WINDOW=windowed
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
# off | log = every decision, the negotiated format, verbose player output and
# a perf line every 5 s, on screen and in $XDG_RUNTIME_DIR/protohdmicapture/debug.log
DEBUG="${DEBUG:-off}"           # off | log

# Exit status 3 means "failed, and the reason was already reported": printed
# on a terminal, and also sent as a desktop notification when there is none
# (started from a menu via launch.sh), so nobody has to dig it out of a log.
die() {
    printf '\033[31m!! %s\033[0m\n' "$*" >&2
    [ -t 2 ] || desktop_notify critical "protoHDMIcapture failed" "$*"
    exit 3
}
info() { printf '\033[36m:: %s\033[0m\n' "$*"; }
warn() { printf '\033[33m** %s\033[0m\n' "$*" >&2; }

# --- one window, one loopback ----------------------------------------------
# Two locks rather than one session lock, because the tuning pair
# (AUDIO=none ./view.sh beside ./view.sh --tune) is one video holder plus one
# audio holder and must be allowed. What is refused is a second window, or a
# second loopback (double audio). flock locks drop when their holder exits,
# however it dies, so there is nothing stale to clean up.
RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture"
mkdir -p "$RUN_DIR" || die "cannot create $RUN_DIR"

take_video_lock() {
    exec {VIDEO_LOCK}>"$RUN_DIR/video.lock"
    flock -n "$VIDEO_LOCK" && return 0
    local pid
    pid=$(cat "$RUN_DIR/video.pid" 2>/dev/null)
    if [ -n "$pid" ] && desktop_focus_pid "$pid"; then
        info "already running (pid $pid); focused it"
    else
        info "already running (pid ${pid:-unknown})"
        [ -t 1 ] || desktop_notify normal "protoHDMIcapture is already running"
    fi
    exit 0
}

AUDIO_LOCK=""
take_audio_lock() {
    [ -z "$AUDIO_LOCK" ] || return 0
    exec {AUDIO_LOCK}>"$RUN_DIR/audio.lock"
    flock -n "$AUDIO_LOCK" \
        || die "another session is already playing the card's audio; stop it, or start this one with AUDIO=none"
    dbg "audio lock: held"
}

# --- backend capabilities --------------------------------------------------
# Which player knobs each backend honours. The env-var interface is shared,
# but gst and ffplay have no equivalent for some of mpv's settings; say so
# instead of silently ignoring them.
declare -A BACKEND_KNOBS=(
    [mpv]="FMT WINDOW WINDOW_SIZE SCALING LEVELS IPC"
    [gst]="FMT WINDOW GST_IO_MODE"
    [ffplay]="FMT WINDOW WINDOW_SIZE"
)
# The WINDOW modes each backend can produce on its own. waylandsink has no
# decorations to remove and no way to ask for a size.
declare -A BACKEND_WINDOWS=(
    [mpv]="fullscreen borderless windowed"
    [gst]="fullscreen"
    [ffplay]="fullscreen borderless windowed"
)
[ -z "$DEPRECATED_BAD" ] || die "$DEPRECATED_BAD"
[ -z "$DEPRECATED" ] || warn "$DEPRECATED"
case "$WINDOW" in
    fullscreen|borderless|windowed) ;;
    *) die "unknown WINDOW=$WINDOW (fullscreen | borderless | windowed)" ;;
esac
[[ "$WINDOW_SIZE" =~ ^[0-9]+x[0-9]+$ ]] || die "WINDOW_SIZE=$WINDOW_SIZE is not WIDTHxHEIGHT"
[ -n "${BACKEND_KNOBS[$BACKEND]+x}" ] || die "unknown BACKEND=$BACKEND (mpv | gst | ffplay)"
for _knob in $REQUESTED; do
    [[ " ${BACKEND_KNOBS[$BACKEND]} " == *" $_knob "* ]] \
        || warn "BACKEND=$BACKEND ignores $_knob=${!_knob} (it honours: ${BACKEND_KNOBS[$BACKEND]})"
done
if ! [[ " ${BACKEND_WINDOWS[$BACKEND]} " == *" $WINDOW "* ]]; then
    if desktop_can place_window; then
        info "BACKEND=$BACKEND cannot make a $WINDOW window by itself; the $DESKTOP adapter shapes it"
    else
        warn "BACKEND=$BACKEND cannot make a $WINDOW window by itself; you get an undecorated window the desktop sizes"
    fi
fi
# Not a knob but a mode: the other backends would play no audio at all.
[ "$AUDIO" != "mpv" ] || [ "$BACKEND" = "mpv" ] \
    || die "AUDIO=mpv needs BACKEND=mpv; use AUDIO=loopback with $BACKEND"

case "$DEBUG" in off|log) ;; *) die "unknown DEBUG=$DEBUG (off | log)" ;; esac
if [ "$DEBUG" != off ] && [ "$MODE" = play ]; then
    debug_tee "$RUN_DIR/debug.log"
    dbg "protoHDMIcapture $(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo '?') · desktop $DESKTOP · log $RUN_DIR/debug.log"
    debug_knob_sources CARD_SERIAL DEV SRC SINK RES FPS FMT DELAY LAT_MS BACKEND AUDIO \
        WINDOW WINDOW_SIZE SCALING LEVELS IPC REFRESH GST_IO_MODE
fi

[ "$MODE" = tune ] || take_video_lock
if [ -n "${VIDEO_LOCK:-}" ]; then dbg "video lock: held"; else dbg "video lock: not taken (tune mode)"; fi

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
    take_audio_lock
    stop_audio
    # -C capture source, -P playback sink (omitted = follow default sink),
    # --latency = loopback buffer, --delay = deliberate offset to match video.
    pw-loopback -n protohdmicapture-audio \
        -C "$SRC" ${SINK:+-P "$SINK"} \
        --latency "$LAT_MS" \
        --delay "$DELAY" >/dev/null 2>&1 &
    LOOPBACK_PID=$!
    echo "$LOOPBACK_PID" >"$RUN_DIR/audio.pid"
    printf 'DELAY=%s\nLAT_MS=%s\nSRC=%s\n' "$DELAY" "$LAT_MS" "$SRC" >"$RUN_DIR/audio.env"
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
    dbg "audio link: ${linked:-none yet} -> input.protohdmicapture-audio -> ${SINK:-default sink}"
    if [ -n "$linked" ] && [ "$linked" != "$SRC" ]; then
        warn "audio is linked to \"$linked\", not the requested \"$SRC\""
        warn "the requested source was probably unavailable; check it with: pactl list short sources"
    fi
}
stop_audio() {
    if [ -n "$LOOPBACK_PID" ] && kill -0 "$LOOPBACK_PID" 2>/dev/null; then
        kill "$LOOPBACK_PID" 2>/dev/null
        wait "$LOOPBACK_PID" 2>/dev/null
    fi
    LOOPBACK_PID=""
    rm -f "$RUN_DIR/audio.pid"
}

# --- player process ----------------------------------------------------------
# The player runs in the background so its PID can be recorded for a second
# launch to focus, and so a signal to this script reaches it: a background
# job in a non-interactive shell ignores SIGINT, so Ctrl-C has to be passed on
# explicitly rather than relied on.
PLAYER_PID=""
run_player() {
    PLAYER_PID=$1
    echo "$PLAYER_PID" >"$RUN_DIR/video.pid"
    # Shape the window once it maps, without holding up the wait below. The
    # helper closes the lock fds it inherited: it can outlive a short-lived
    # player by a moment, and must not keep a relaunch locked out.
    (
        exec {VIDEO_LOCK}>&-
        [ -z "$AUDIO_LOCK" ] || exec {AUDIO_LOCK}>&-
        if desktop_place_window "$PLAYER_PID" "$WINDOW" "$WINDOW_SIZE"; then info "$DESKTOP_MSG"
        else dbg "$DESKTOP_MSG"; fi
    ) &
    if [ "$DEBUG" != off ]; then
        (
            exec {VIDEO_LOCK}>&-
            [ -z "$AUDIO_LOCK" ] || exec {AUDIO_LOCK}>&-
            debug_after_start "$PLAYER_PID"
        ) &
        DEBUG_JOB=$!
    fi
    dbg "player: $BACKEND pid $PLAYER_PID, window $WINDOW${WINDOW_SIZE:+ ($WINDOW_SIZE)}"
    wait "$PLAYER_PID"
    local rc=$?
    dbg "player exited with status $rc"
    return "$rc"
}
stop_player() {
    if [ -n "$PLAYER_PID" ] && kill -0 "$PLAYER_PID" 2>/dev/null; then
        kill "$PLAYER_PID" 2>/dev/null
        wait "$PLAYER_PID" 2>/dev/null
    fi
    PLAYER_PID=""
}

DEBUG_JOB=""
trap 'stop_player; stop_audio; desktop_refresh_restore; [ -z "$DEBUG_JOB" ] || kill "$DEBUG_JOB" 2>/dev/null' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# --- display refresh matching ---------------------------------------------
start_refresh() {
    [ "$REFRESH" = "match" ] || return 0
    if desktop_refresh_match "$FPS"; then info "$DESKTOP_MSG"; else warn "$DESKTOP_MSG"; fi
    dbg "refresh: $DESKTOP_MSG"
}

# --- interactive A/V sync tuner -------------------------------------------
# Restarts the loopback with a new delay on each keypress. Video keeps running
# in another terminal / window; you only nudge audio until it lines up.
# TUNE_OUT: on q, also write the kept DELAY to this file (the setup wizard
# reads it). TUNE_EMBEDDED: the caller already runs the video, skip saying so.
if [ "$MODE" = tune ]; then
    [ "$AUDIO" = "loopback" ] || die "--tune needs AUDIO=loopback"
    start_audio
    if [ -z "${TUNE_EMBEDDED:-}" ]; then
        printf '\n  A/V sync tuner. Start the video in another terminal:\n      AUDIO=none ./view.sh\n'
    fi
    cat <<'EOF'

  Watch something with sharp transients (console menu clicks are ideal):
      j / k   audio 10 ms earlier / later
      h / l   audio 50 ms earlier / later
      0       reset to 0
      q       keep this DELAY and quit
      x       quit without keeping it

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
            q) echo; info "keep this: DELAY=$DELAY"
               [ -z "${TUNE_OUT:-}" ] || printf '%s\n' "$DELAY" >"$TUNE_OUT"
               exit 0 ;;
            x) echo; info "discarded"; exit 1 ;;
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
        # the per-frame "V: hh:mm:ss" status line means nothing for a live feed
        --term-status-msg=
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
    case "$WINDOW" in
        fullscreen) args+=(--fs) ;;
        borderless) args+=(--border=no --title-bar=no --window-maximized=yes) ;;
        windowed)   args+=(--geometry="$WINDOW_SIZE") ;;
    esac

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

    # Verbose everywhere, except libplacebo's shader chatter and the subtitle
    # renderer, which say nothing about this pipeline.
    [ "$DEBUG" != off ] && args+=("--msg-level=all=v,vo/gpu-next/libplacebo=info,osd/libass=warn,clipboard=warn")
    info "mpv ${args[*]}"
    # MANGOHUD=1 is set globally in ~/.config/environment.d/99-proton-amd.conf and
    # MangoHud is an *implicit* Vulkan layer, so it injects into gpu-next and adds
    # an overlay draw to every frame. Not wanted in a latency path.
    # The VIDIOC_QBUF line is libavdevice's noisy v4l2 teardown on exit; it is
    # harmless (verified: no re-enumeration) and only that exact line is dropped.
    MANGOHUD=0 mpv "${args[@]}" \
        2> >(grep -vE 'ioctl\(VIDIOC_QBUF\): Inappropriate ioctl for device' >&2) &
    run_player $!
    ;;

gst)
    # sync=false on the sink is the key knob: render on arrival, do not wait
    # for a presentation timestamp. queue leaky=downstream drops rather than
    # accumulates if the sink ever falls behind, so latency cannot creep.
    # waylandsink is Wayland-only, and it cannot set a window title: on
    # Hyprland the window shows up as class gst-launch-1.0 with no title.
    [ -n "${WAYLAND_DISPLAY:-}" ] || die "BACKEND=gst renders with waylandsink and needs a Wayland session"
    DECODE=""
    case "$FMT" in
        mjpeg)       CAPS="image/jpeg"; DECODE="jpegdec ! " ;;
        yuyv422)     CAPS="video/x-raw,format=YUY2" ;;
        nv12)        CAPS="video/x-raw,format=NV12" ;;
        rawvideo|"") CAPS="video/x-raw" ;;
        *)           die "BACKEND=gst has no caps for FMT=$FMT (yuyv422 | mjpeg | nv12 | rawvideo | \"\")" ;;
    esac
    CAPS="${CAPS},width=${W},height=${H},framerate=${FPS}/1"
    SINK_OPTS="sync=false async=false qos=false enable-last-sample=false"
    [ "$WINDOW" = fullscreen ] && SINK_OPTS="$SINK_OPTS fullscreen=true"
    PIPE="v4l2src device=${DEV} io-mode=${GST_IO_MODE} do-timestamp=true ! ${CAPS} ! ${DECODE}queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! videoconvert ! waylandsink ${SINK_OPTS}"
    info "gst-launch-1.0 $PIPE"
    if [ "$DEBUG" != off ]; then
        # Element warnings, source and sink at info, QoS (late/dropped
        # buffers) at debug; -v prints the negotiated caps once.
        GST_DEBUG="2,v4l2src:4,waylandsink:4,qos:5" GST_DEBUG_NO_COLOR=1 gst-launch-1.0 -v -e $PIPE &
    else
        gst-launch-1.0 -e $PIPE &
    fi
    run_player $!
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
    case "$WINDOW" in
        fullscreen) args+=(-fs) ;;
        borderless) args+=(-noborder) ;;
        windowed)   args+=(-x "${WINDOW_SIZE%x*}" -y "${WINDOW_SIZE#*x}") ;;
    esac
    info "ffplay ${args[*]} $DEV"
    if [ "$DEBUG" != off ]; then
        args=("${args[@]/warning/verbose}" -stats)
        ffplay "${args[@]}" "$DEV" 2> >(debug_ffplay_filter >&2) &
    else
        ffplay "${args[@]}" "$DEV" &
    fi
    run_player $!
    ;;

esac
