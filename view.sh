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
#   ./view.sh --tune          # the feed with the overlay open: ctrl -/+ syncs audio
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
# shellcheck source=lib/audio.sh
. "$HERE/lib/audio.sh"

# --- mode --------------------------------------------------------------------
# --setup hands over before the config is even read, so a config that would
# make this script die can still be fixed by the wizard.
if [ "${1:-}" = "--setup" ]; then
    shift
    exec "$HERE/lib/setup.sh" "$@"
fi
# --dashboard is the live debug window; like --setup it needs no config.
[ "${1:-}" = "--dashboard" ] && exec "$HERE/lib/dashboard.sh"

# Helpers for the keys in the mpv window (lib/mpv/protohdmicapture.lua), acting
# on the running session through its run directory; no config, no locks.
_RUN="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture"
case "${1:-}" in
--place-window)
    _pid=$(cat "$_RUN/video.pid" 2>/dev/null) && kill -0 "$_pid" 2>/dev/null || exit 1
    case "${2:-}" in fullscreen|borderless|windowed) ;; *) echo "--place-window fullscreen|borderless|windowed" >&2; exit 2 ;; esac
    echo "$2" >"$_RUN/window.mode"
    desktop_place_window "$_pid" "$2" "$(cat "$_RUN/window.size" 2>/dev/null || echo 1280x720)"
    exit 0 ;;
--open-dashboard)
    desktop_open_terminal protohdmicapture.debug "protoHDMIcapture debug" "$HERE/view.sh" --dashboard
    exit ;;
--audio)
    # The overlay's m, 9 and 0: mute and volume of this session's loopback.
    RUN_DIR="$_RUN"
    shift
    audio_control "$@"
    exit ;;
--save)
    # The overlay's "save as defaults": the same writer the wizard uses, and
    # only the settings flagged save in lib/settings.sh.
    shift
    [ "$#" -gt 0 ] || { echo "--save KEY=VALUE..." >&2; exit 2; }
    # The wizard's test feed reads a preview config that the wizard deletes;
    # a save there would vanish.
    [ -z "${PROTOHDMI_SETUP_CHILD:-}" ] \
        || { echo "this is the wizard's test feed; change settings in the wizard" >&2; exit 1; }
    for _kv in "$@"; do
        [[ " $(settings_with save | tr '\n' ' ') " == *" ${_kv%%=*} "* ]] \
            || { echo "--save: ${_kv%%=*} is not a setting the overlay saves" >&2; exit 2; }
    done
    config_write "$@" || { echo "$CONFIG_ERROR" >&2; exit 1; }
    echo "saved to $CONFIG_FILE: $*"
    exit 0 ;;
esac
MODE=play
DEBUG_FLAG=""
for _arg in "$@"; do
    case "$_arg" in
        --tune) MODE=tune ;;
        --debug) DEBUG_FLAG=1 ;;
        --settings) MODE=settings ;;
        -h|--help)
            echo "usage: ./view.sh [--setup | --tune | --debug | --settings]   (settings: env vars or the config file)"
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

# --- configuration ---------------------------------------------------------
# Every setting comes from the environment, else the saved settings (see
# config.example), else its default; lib/settings.sh holds one row per
# setting: its default, the values it allows and the players that honour it.
settings_resolve
[ -z "$DEBUG_FLAG" ] || { DEBUG=window; SETTING_FROM[DEBUG]="env"; }

# --settings: each setting, its value and where it came from, then whether
# they are all valid. Changes nothing; needs no card.
if [ "$MODE" = settings ]; then
    for _n in "${SETTINGS[@]}"; do
        printf '%-16s %-34s %s\n' "$_n" "${!_n:-\"\"}" "${SETTING_FROM[$_n]}"
    done
    for _n in "${SETTINGS_NOTES[@]}"; do echo "note: $_n"; done
    if settings_validate; then echo "all valid"; exit 0; fi
    for _n in "${SETTINGS_ERRORS[@]}"; do echo "invalid: $_n"; done
    exit 1
fi

for _n in "${SETTINGS_NOTES[@]}"; do warn "$_n"; done
if ! settings_validate; then
    for _n in "${SETTINGS_ERRORS[@]:1}"; do warn "$_n"; done
    die "${SETTINGS_ERRORS[0]}"
fi


# --- one window, one loopback ----------------------------------------------
# Two locks rather than one session lock, because the tuning pair
# (AUDIO=none ./view.sh beside ./view.sh --tune) is one video holder plus one
# audio holder and must be allowed. What is refused is a second window, or a
# second loopback (double audio). flock locks drop when their holder exits,
# however it dies, so there is nothing stale to clean up.
RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture"
mkdir -p "$RUN_DIR" || die "cannot create $RUN_DIR"

# --tune is the feed with the overlay open: audio sync is ctrl -/+ there.
# Beside a running feed with no loopback of its own (AUDIO=none ./view.sh,
# the old tuning pair) there is no window to tune in, so it is the terminal
# tuner instead, as the wizard's sync stage uses (TUNE_EMBEDDED).
_lock_free() { ( flock -n 9 ) 9>"$RUN_DIR/$1.lock"; }
OPEN_OVERLAY=""
if [ "$MODE" = tune ] && [ -z "${TUNE_EMBEDDED:-}" ]; then
    [ "$AUDIO" = loopback ] || die "--tune needs AUDIO=loopback"
    if ! _lock_free video && _lock_free audio; then
        info "a feed without its own audio is running: tuning here in the terminal"
    else
        MODE=play; OPEN_OVERLAY=panel
    fi
fi

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
    [ -z "$OPEN_OVERLAY" ] || info "audio sync is in that window: ctrl -/+ (Tab shows it, w saves it)"
    exit 0
}

AUDIO_LOCK=""
take_audio_lock() {
    [ -z "$AUDIO_LOCK" ] || return 0
    exec {AUDIO_LOCK}>"$RUN_DIR/audio.lock"
    flock -n "$AUDIO_LOCK" \
        || die "another session is already playing the card's audio; stop it, or start this one with AUDIO=none"
    dbg "audio lock: held"
    # A new session starts from what WirePlumber restores for the stream,
    # not from the previous session's file; only the lock holder may clear it.
    rm -f "$RUN_DIR/audio.state"
}

# --- backend capabilities --------------------------------------------------
# Which players honour which setting is in lib/settings.sh: gst and ffplay
# have no equivalent for some of mpv's settings, and a value you set for a
# player that ignores it earns a warning, never silence (a default does not).
# The WINDOW modes each backend can produce on its own. waylandsink has no
# decorations to remove and no way to ask for a size.
declare -A BACKEND_WINDOWS=(
    [mpv]="fullscreen borderless windowed"
    [gst]="fullscreen"
    [ffplay]="fullscreen borderless windowed"
)
for _n in $(settings_with "ignored-by=$BACKEND"); do
    [ "${SETTING_FROM[$_n]}" = default ] || warn "BACKEND=$BACKEND ignores $_n=${!_n} (only $(settings_fact "$_n" players) honours it)"
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

# The dashboard reads mpv over its IPC socket; with IPC disabled, use a
# private one for this session rather than showing no numbers.
if [ "$DEBUG" = window ] && [ "$BACKEND" = mpv ] && [ -z "$IPC" ]; then
    IPC="$RUN_DIR/mpv.sock"
    warn "IPC is disabled; the debug window uses a private socket for this session ($IPC)"
fi
if [ "$DEBUG" != off ] && [ "$MODE" = play ]; then
    debug_tee "$RUN_DIR/debug.log"
    dbg "protoHDMIcapture $(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo '?') · desktop $DESKTOP · log $RUN_DIR/debug.log"
    debug_settings
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
    audio_state_apply

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
rm -f "$RUN_DIR/player.quit"
run_player() {
    PLAYER_PID=$1
    echo "$PLAYER_PID" >"$RUN_DIR/video.pid"
    echo "$BACKEND" >"$RUN_DIR/backend"
    echo "$WINDOW" >"$RUN_DIR/window.mode"
    echo "$WINDOW_SIZE" >"$RUN_DIR/window.size"
    if [ "$BACKEND" = mpv ]; then echo "$IPC" >"$RUN_DIR/ipc.path"; else rm -f "$RUN_DIR/ipc.path"; fi
    # The dashboard is fire-and-forget: in its own terminal, holding none of
    # this session's lock fds, and nothing here waits on it or checks it.
    if [ "$DEBUG" = window ] && [ "$MODE" = play ]; then
        (
            exec {VIDEO_LOCK}>&-
            [ -z "$AUDIO_LOCK" ] || exec {AUDIO_LOCK}>&-
            desktop_open_terminal protohdmicapture.debug "protoHDMIcapture debug" "$HERE/view.sh" --dashboard \
                || warn "DEBUG=window: no terminal found for the dashboard; the log still runs"
        )
    fi
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
    dbg "player: $BACKEND pid $PLAYER_PID, window $WINDOW$([ "$WINDOW" = windowed ] && echo " ($WINDOW_SIZE)")"
    # USR1 (an audio sync change from the overlay) interrupts the wait: serve
    # it and wait again. Decided by the flag and the player still being ours
    # to wait for, never by $?: a player killed by a signal also returns >128.
    # Drained before each wait: a request that lands during a restart (half a
    # second of link checking) is served right after it, not left waiting.
    local rc
    while :; do
        while [ -n "$SYNC_REQUESTED" ]; do SYNC_REQUESTED=""; apply_sync_request; done
        wait "$PLAYER_PID"; rc=$?
        [ -n "$SYNC_REQUESTED" ] && kill -0 "$PLAYER_PID" 2>/dev/null && continue
        break
    done
    # mpv sometimes segfaults in libavdevice's v4l2 teardown after a normal
    # quit (upstream; "Some buffers are still owned by the caller on close").
    # The mpv script marked the shutdown before it, so this was a clean quit:
    # do not report it as a failure. A crash mid-feed has no mark and still is.
    if [ "$rc" -gt 128 ] && [ -e "$RUN_DIR/player.quit" ]; then
        info "mpv crashed (signal $((rc - 128))) during its own shutdown after a normal quit; a known upstream teardown bug, ignored"
        rc=0
    fi
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

# An audio sync change from the overlay: the mpv script writes audio.request
# and sends USR1 here, and this process, which owns the loopback, its lock
# and its PID file, restarts it. The trap only sets a flag; run_player's
# wait loop does the work.
SYNC_REQUESTED=""
apply_sync_request() {
    local want
    # Claimed by a rename before reading, so a request written meanwhile is
    # the next one, not deleted unread. None left: an earlier signal already
    # served the newest request.
    mv -f "$RUN_DIR/audio.request" "$RUN_DIR/audio.request.claimed" 2>/dev/null \
        || { dbg "audio sync: no request waiting"; return; }
    want=$(sed -n 's/^DELAY=//p' "$RUN_DIR/audio.request.claimed")
    rm -f "$RUN_DIR/audio.request.claimed"
    if ! [[ "$want" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
        warn "audio sync request ignored: '$want' is not a delay in seconds"; return
    fi
    if [ "$AUDIO" != loopback ] || [ -z "$LOOPBACK_PID" ]; then
        warn "audio sync request ignored: this session has no audio loopback"; return
    fi
    DELAY="$want"
    start_audio
}
trap 'SYNC_REQUESTED=1' USR1

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

# --- terminal A/V sync tuner (the wizard's) ---------------------------------
# Only with TUNE_EMBEDDED: the setup wizard runs the video beside it and this
# tuner in its own terminal. A plain --tune is the feed with the overlay open
# (see the mode section). Restarts the loopback with a new delay on each
# keypress. TUNE_OUT: on q, also write the kept DELAY to this file.
if [ "$MODE" = tune ]; then
    [ "$AUDIO" = "loopback" ] || die "--tune needs AUDIO=loopback"
    start_audio
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
            k) DELAY=$(awk -v d="$DELAY" 'BEGIN{v=d+0.010; if(v>1)v=1; printf "%.3f", v}') ;;
            h) DELAY=$(awk -v d="$DELAY" 'BEGIN{v=d-0.050; if(v<0)v=0; printf "%.3f", v}') ;;
            l) DELAY=$(awk -v d="$DELAY" 'BEGIN{v=d+0.050; if(v>1)v=1; printf "%.3f", v}') ;;
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

    if [ -n "$SOURCE" ] && [ ! -r "$HERE/sources/$SOURCE.md" ]; then
        warn "SOURCE=$SOURCE has no profile in sources/; the overlay shows the tips for any HDMI source"
        SOURCE=""
    fi

    # What the overlay may cycle and save, from lib/settings.sh, as
    # script-opts ("+" between items: script-opts split on commas).
    _settings_opts() {
        local n o
        o="protohdmicapture-saveable=$(settings_with save | paste -sd+)"
        for n in WINDOW SCALING LEVELS; do
            o+=",protohdmicapture-${n,,}_values=$(settings_fact "$n" values | sed 's/^enum://; s/,/+/g')"
        done
        o+=",protohdmicapture-delay_max=$(settings_fact DELAY values | sed 's/.*\.\.//')"
        printf '%s' "$o"
    }

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
        # f cycles the display mode, d opens the debug dashboard
        --script="$HERE/lib/mpv/protohdmicapture.lua"
        --script-opts="protohdmicapture-helper=$HERE/view.sh,protohdmicapture-mode=$WINDOW,protohdmicapture-quitfile=$RUN_DIR/player.quit,protohdmicapture-scaling=$SCALING,protohdmicapture-levels=$LEVELS,protohdmicapture-sources=$HERE/sources,protohdmicapture-source=$SOURCE,protohdmicapture-audio=$AUDIO,protohdmicapture-owner=$$,protohdmicapture-rundir=$RUN_DIR,protohdmicapture-delay=$DELAY,protohdmicapture-open=$OPEN_OVERLAY,$(_settings_opts)"
        # a live feed generates no input events, so idle-blanking would otherwise
        # kick in mid-game
        --stop-screensaver=always
    )
    [ -n "$IPC" ] && args+=(--input-ipc-server="$IPC")
    # The range of the card's frames, not of the display: --video-output-levels
    # would send studio-range RGB to a PC monitor. The format filter only
    # relabels the frames, no copy.
    # Labelled, so the overlay's l key can replace it live.
    [ "$LEVELS" != "auto" ] && args+=(--vf-append="@phc-levels:format:colorlevels=$LEVELS")
    # Always passed, 0 included: a saved 0 must beat a value in mpv.conf.
    for _c in BRIGHTNESS CONTRAST SATURATION GAMMA; do
        args+=(--"${_c,,}=${!_c}")
    done
    case "$WINDOW" in
        fullscreen) args+=(--fs) ;;
        borderless) args+=(--border=no --title-bar=no --window-maximized=yes) ;;
        windowed)   args+=(--geometry="$WINDOW_SIZE") ;;
    esac

    # Scaling. mpv.conf sets profile=high-quality (ewa_lanczossharp + debanding
    # + antiringing); --profile=low-latency does NOT undo that, so it survives
    # into this pipeline and costs real GPU frame time on every frame. The mpv
    # script applies the choice (so the overlay can cycle it live):
    #   balanced  spline36/mitchell, no deband, no dither: good upscale, a
    #             fraction of high-quality's cost
    #   fast      the properties of mpv's fast profile: bilinear, lowest GPU
    #             time, visibly softer
    #   config    leave mpv.conf's settings alone

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
    warn "BACKEND=gst is a diagnostic player: no overlay and no live keys (Tab, audio sync, volume); mpv has them"
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
    # GStreamer caps take a fraction, and must equal one the card lists: the
    # NTSC rates are 1000/1001 of a whole rate (59.94 = 60000/1001).
    case "$FPS" in
        23.976) _rate=24000/1001 ;; 29.97) _rate=30000/1001 ;;
        59.94)  _rate=60000/1001 ;; 119.88) _rate=120000/1001 ;;
        *.*)    _rate="$(awk -v f="$FPS" 'BEGIN { printf "%d", f * 1000 + 0.5 }')/1000" ;;
        *)      _rate="$FPS/1" ;;
    esac
    CAPS="${CAPS},width=${W},height=${H},framerate=${_rate}"
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
    warn "BACKEND=ffplay is a diagnostic player: no overlay and no live keys (Tab, audio sync, volume); mpv has them"
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
