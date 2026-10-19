#!/usr/bin/env bash
# protoHDMIcapture live debug dashboard. Opened by view.sh in its own terminal
# when DEBUG=window (or ./view.sh --debug); runs as `./view.sh --dashboard`.
#
# Read-only: it takes no lock and only reads what view.sh leaves in the run
# directory (video.pid, audio.pid, audio.env, ipc.path, backend, debug.log),
# so closing it, or it failing, never touches the feed. It closes itself a few
# seconds after the feed ends.

set -uo pipefail

HERE=$(dirname "$(dirname "$(readlink -f "$0")")")
# shellcheck source=legacy/lib/desktop.sh
. "$HERE/lib/desktop.sh"
# shellcheck source=legacy/lib/debug.sh
. "$HERE/lib/debug.sh"
# shellcheck source=legacy/lib/ui.sh
. "$HERE/lib/ui.sh"

RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture"
LOG="$RUN_DIR/debug.log"
INTERVAL=1
HZ=$(getconf CLK_TCK)

trap 'ui_restore; exit 0' INT TERM HUP
trap ui_restore EXIT
[[ -t 1 ]] && tput civis 2>/dev/null

# Wait briefly for the feed to have written its pid: the dashboard can start
# before the player does.
for _ in $(seq 50); do [ -s "$RUN_DIR/video.pid" ] && break; sleep 0.1; done
PID=$(cat "$RUN_DIR/video.pid" 2>/dev/null)
[ -n "$PID" ] && kill -0 "$PID" 2>/dev/null || { echo "No feed is running."; sleep 3; exit 1; }
BACKEND=$(cat "$RUN_DIR/backend" 2>/dev/null || echo "?")
IPC=$(cat "$RUN_DIR/ipc.path" 2>/dev/null)
START=$(date +%s)

row() { printf '  %s%-14s%s %s\n' "$DIM" "$1" "$RESET" "$2"; }
head_() { printf '\n  %s%s%s\n' "$BOLD$BLUE" "$1" "$RESET"; }
good_bad() { # value, ok-test: green when the test passes, yellow when not
    if eval "$2"; then printf '%s%s%s' "$GREEN" "$1" "$RESET"; else printf '%s%s%s' "$YELLOW" "$1" "$RESET"; fi
}

# xruns (ERR) per node from the second sample of pw-top: the first is
# collected before any cycle has run and reads zeros.
pw_errs() {
    timeout 3 pw-top -b -n 2 2>/dev/null | awk -v want="$1" '
        /^S +ID/ { n++ } n == 2 && $NF ~ want { print $9; exit }'
}

# CPU % is ticks over the real time between samples: a cycle takes longer
# than INTERVAL because pw-top needs two samples of its own.
p0=$(proc_cpu_ticks "$PID"); l0=""; t0=$(date +%s.%N)
while true; do
    if ! kill -0 "$PID" 2>/dev/null; then
        printf '\n  %sThe feed has ended; closing.%s\n' "$YELLOW" "$RESET"
        sleep 2
        exit 0
    fi
    LP=$(cat "$RUN_DIR/audio.pid" 2>/dev/null)
    p1=$(proc_cpu_ticks "$PID")
    l1=""; [ -n "$LP" ] && l1=$(proc_cpu_ticks "$LP")
    [ -z "$l0" ] && l0="$l1"
    t1=$(date +%s.%N)
    dt=$(awk -v a="$t0" -v b="$t1" 'BEGIN { d = b - a; if (d <= 0) d = 1; print d }')

    out=$(
        printf '\n  %s📊 protoHDMIcapture · live%s   %s%s · up %ss%s\n' "$BOLD" "$RESET" "$DIM" "$(date +%T)" $(( $(date +%s) - START )) "$RESET"

        head_ "Video"
        row "player" "$BACKEND · pid $PID · cpu $(_cpu_pct "$p0" "$p1" "$dt" "$HZ")% · rss $(proc_rss_mb "$PID") MB"
        if [ "$BACKEND" = mpv ] && [ -n "$IPC" ]; then
            fps=$(mpv_get "$IPC" estimated-vf-fps); dfps=$(mpv_get "$IPC" display-fps)
            want=$(mpv_get "$IPC" container-fps)
            drops=$(mpv_get "$IPC" frame-drop-count); dec=$(mpv_get "$IPC" decoder-frame-drop-count)
            delayed=$(mpv_get "$IPC" vo-delayed-frame-count)
            w=$(mpv_get "$IPC" width); h=$(mpv_get "$IPC" height)
            row "fps" "$(good_bad "$(_num1 "$fps")" "awk -v a='${fps:-0}' -v b='${want:-0}' 'BEGIN{exit !(b==0 || a>=b*0.95)}'") captured of $(_num1 "$want") · display $(_num1 "$dfps") Hz"
            row "frames" "dropped $(good_bad "${drops:-?}" "[ '${drops:-0}' = 0 ]") · decoder $(good_bad "${dec:-?}" "[ '${dec:-0}' = 0 ]") · delayed $(good_bad "${delayed:-?}" "[ '${delayed:-0}' = 0 ]")"
            row "picture" "${w:-?}x${h:-?}"
            # From the mpv script: ok, waiting (no frame yet), never (none
            # since the feed opened) or stopped.
            sig=$(mpv_get "$IPC" user-data/protohdmicapture/signal)
            case "$sig" in
                ok|waiting) row "signal" "$sig" ;;
                never)      row "signal" "${RED}none since the feed opened${RESET}" ;;
                stopped)    row "signal" "${RED}stopped (source asleep or off?)${RESET}" ;;
                *)          row "signal" "?" ;;
            esac
        elif [ "$BACKEND" = ffplay ]; then
            last=$(grep 'fd=' "$LOG" 2>/dev/null | tail -1 | sed 's/\x1b\[[0-9;]*m//g; s/^ *//')
            row "ffplay stats" "${last:-(none yet)}"
        elif [ "$BACKEND" = gst ]; then
            qos=$(grep -ci 'qos' "$LOG" 2>/dev/null)
            row "qos events" "$(good_bad "${qos:-0}" "[ '${qos:-0}' = 0 ]") (late or dropped buffers reported by the sink)"
        fi

        head_ "Audio"
        if [ -n "$LP" ] && kill -0 "$LP" 2>/dev/null; then
            delay=$(sed -n 's/^DELAY=//p' "$RUN_DIR/audio.env" 2>/dev/null)
            lat=$(sed -n 's/^LAT_MS=//p' "$RUN_DIR/audio.env" 2>/dev/null)
            err=$(pw_errs 'input\\.protohdmicapture-audio')
            row "loopback" "pid $LP · cpu $(_cpu_pct "$l0" "$l1" "$dt" "$HZ")% · delay ${delay:-?} s · buffer ${lat:-?} ms"
            row "xruns" "$(good_bad "${err:-?}" "[ '${err:-0}' = 0 ]") (PipeWire ERR count for the loopback)"
        elif [ -r "$RUN_DIR/audio.failed" ]; then
            row "loopback" "${RED}failed: $(head -1 "$RUN_DIR/audio.failed")${RESET} (see audio.log)"
        else
            row "loopback" "not running (AUDIO=none or mpv)"
        fi

        head_ "Display"
        while read -r line; do row "monitor" "$line"; done < <(desktop_outputs 2>/dev/null)

        head_ "Recent warnings"
        grep -aiE '\*\* |!! |warn|error|fail|dropp|corrupt|underrun|xrun' "$LOG" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' \
            | tail -8 | cut -c1-$(( COLS - 4 )) | sed 's/^/    /'
        printf '\n  %sCtrl-C closes this window; the feed keeps running.%s\n' "$DIM" "$RESET"
    )
    _clear
    printf '%s\n' "$out"
    p0=$p1; l0=$l1; t0=$t1
    sleep "$INTERVAL"
done
