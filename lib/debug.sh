# shellcheck shell=bash
# Debug module: troubleshooting output for view.sh. Sourced by view.sh.
#
#   DEBUG=off | log | window   (window adds the live dashboard, see --dashboard)
#
#   dbg MSG                  timestamped "[dbg]" line when DEBUG is not off
#   debug_tee LOGFILE        from here on, copy stdout and stderr into LOGFILE
#   debug_knob_sources K...  one line: each knob's value and where it came from
#                            (env | file | default); call debug_snapshot_env
#                            before load_config and debug_snapshot_file after
#   debug_after_start PID    background: the negotiated capture format, then a
#                            perf line every 5 s until PID exits
#   mpv_get SOCKET PROP      one property over mpv's IPC socket (needs socat)
#   proc_cpu_ticks PID / proc_rss_mb PID   /proc readings for CPU and memory
#
# Nothing here is per-frame: the aim is every decision and a steady pulse of
# numbers, not a firehose.

DEBUG_PERF_INTERVAL="${DEBUG_PERF_INTERVAL:-5}"

dbg() {
    [ "${DEBUG:-off}" = off ] && return 0
    printf '\033[2m%s [dbg] %s\033[0m\n' "$(date +%T.%3N)" "$*"
}

debug_tee() {
    : >"$1"
    exec > >(tee -a "$1") 2> >(tee -a "$1" >&2)
}

declare -A _DEBUG_SRC=()
debug_snapshot_env() {
    local k
    for k in "$@"; do [ -n "${!k+x}" ] && _DEBUG_SRC[$k]="env"; done
}
debug_snapshot_file() {
    local k
    for k in "$@"; do [ -n "${!k+x}" ] && [ -z "${_DEBUG_SRC[$k]:-}" ] && _DEBUG_SRC[$k]="file"; done
}
debug_knob_sources() {
    [ "${DEBUG:-off}" = off ] && return 0
    local k out=""
    for k in "$@"; do out+=" $k=${!k:-\"\"}(${_DEBUG_SRC[$k]:-default})"; done
    dbg "knobs:$out"
}

mpv_get() {
    command -v socat >/dev/null 2>&1 && [ -S "$1" ] || return 1
    printf '{"command":["get_property","%s"]}\n' "$2" \
        | socat -t 0.5 - "$1" 2>/dev/null \
        | sed -n 's/.*"data":\([^,}]*\).*"error":"success".*/\1/p' | tr -d '"' | head -1
}

# utime + stime of a process, in clock ticks (fields 14 and 15 of stat; the
# command name in field 2 may contain spaces, so count from its closing paren).
proc_cpu_ticks() {
    local stat
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    stat="${stat##*) }"
    # shellcheck disable=SC2086  # deliberate field splitting
    set -- $stat
    echo $(( ${12} + ${13} ))
}
proc_rss_mb() {
    awk '/^VmRSS:/ { printf "%d", $2 / 1024 }' "/proc/$1/status" 2>/dev/null
}

_num1() { if [[ "${1:-}" =~ ^[0-9.]+$ ]]; then printf '%.1f' "$1"; else printf '?'; fi; }

# CPU % of PID over the last interval, from two tick readings.
_cpu_pct() { awk -v a="$1" -v b="$2" -v t="$3" -v hz="$4" 'BEGIN { if (a == "" || b == "") print "?"; else printf "%.1f", (b - a) / hz / t * 100 }'; }

debug_after_start() {
    [ "${DEBUG:-off}" = off ] && return 0
    local pid="$1" hz
    hz=$(getconf CLK_TCK)
    sleep 2
    kill -0 "$pid" 2>/dev/null || return 0
    # What the device really runs at, as opposed to what was asked for.
    local fmt parm
    fmt=$(v4l2-ctl -d "$DEV" --get-fmt-video 2>/dev/null \
          | sed -n "s/^[[:space:]]*Width\/Height[[:space:]]*:[[:space:]]*//p; s/^[[:space:]]*Pixel Format[[:space:]]*:[[:space:]]*//p" | paste -sd' ')
    parm=$(v4l2-ctl -d "$DEV" --get-parm 2>/dev/null | sed -n 's/.*Frames per second:[[:space:]]*//p')
    dbg "negotiated: ${fmt:-?} @ ${parm:-?}"

    local p0 l0 p1 l1 lp line fps drops dec delayed dfps
    p0=$(proc_cpu_ticks "$pid")
    lp=$(cat "$RUN_DIR/audio.pid" 2>/dev/null)
    l0=$( [ -n "$lp" ] && proc_cpu_ticks "$lp")
    while sleep "$DEBUG_PERF_INTERVAL"; do
        kill -0 "$pid" 2>/dev/null || break
        p1=$(proc_cpu_ticks "$pid")
        lp=$(cat "$RUN_DIR/audio.pid" 2>/dev/null)
        l1=$( [ -n "$lp" ] && proc_cpu_ticks "$lp")
        line="player cpu $(_cpu_pct "$p0" "$p1" "$DEBUG_PERF_INTERVAL" "$hz")% rss $(proc_rss_mb "$pid")MB"
        [ -n "$lp" ] && line+=" | loopback cpu $(_cpu_pct "$l0" "$l1" "$DEBUG_PERF_INTERVAL" "$hz")%"
        if [ "$BACKEND" = mpv ] && [ -n "${IPC:-}" ]; then
            fps=$(mpv_get "$IPC" estimated-vf-fps)
            dfps=$(mpv_get "$IPC" display-fps)
            drops=$(mpv_get "$IPC" frame-drop-count)
            dec=$(mpv_get "$IPC" decoder-frame-drop-count)
            delayed=$(mpv_get "$IPC" vo-delayed-frame-count)
            line+=" | fps $(_num1 "$fps") display $(_num1 "$dfps")Hz drops ${drops:-?} decoder-drops ${dec:-?} delayed ${delayed:-?}"
        fi
        dbg "perf: $line"
        p0=$p1; l0=$l1
    done
}

# ffplay's -stats line is rewritten with \r every frame; keep one per second.
debug_ffplay_filter() {
    awk 'BEGIN { RS = "[\r\n]" }
         /fd=/ { t = systime(); if (t != last) { print; last = t; fflush() } next }
         NF    { print; fflush() }'
}
