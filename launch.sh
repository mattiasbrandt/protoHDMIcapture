#!/usr/bin/env bash
# GUI entry point for view.sh, used by protohdmicapture.desktop.
#
# view.sh expects a terminal: it prints its decisions, and `die` writes the
# reason to stderr. Launched from an app launcher there is no terminal, so this
# wrapper does the three jobs the terminal was doing:
#
#   1. keeps a second pipeline from starting on top of the first
#   2. turns a failed start into a desktop notification instead of silence
#   3. keeps the full run log somewhere readable afterwards
#
# Env vars are passed straight through, so the desktop entry's defaults can
# still be overridden from a terminal:
#
#     REFRESH=keep ./launch.sh

set -uo pipefail

HERE=$(dirname "$(readlink -f "$0")")
LOG="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture.log"

notify() { command -v notify-send >/dev/null 2>&1 && notify-send -a protoHDMIcapture "$@"; }

# --- already running? focus it instead of starting a second pipeline -------
# Match class AND title exactly. A browser tab called "protoHDMIcapture ..." must not count,
# which is why this does not use `omarchy launch or focus` — that matches a
# word-boundary regex against class *or* title and would focus the browser.
if command -v hyprctl >/dev/null 2>&1; then
    addr=$(hyprctl clients -j 2>/dev/null \
           | jq -r '.[] | select(.class == "mpv" and .title == "protoHDMIcapture") | .address' \
           | head -1)
    if [ -n "${addr:-}" ] && [ "$addr" != "null" ]; then
        hyprctl dispatch focuswindow "address:$addr" >/dev/null 2>&1
        exit 0
    fi
fi

# --- defaults for the GUI path --------------------------------------------
# DP-1 runs at 100Hz and cannot do VRR (native G-Sync module, no vrr_capable in
# sysfs), so 60fps judders unless the panel is dropped to its 59.97Hz mode.
# view.sh restores the original mode on exit. See README "Display cadence".
export REFRESH="${REFRESH:-match}"

# --- run -------------------------------------------------------------------
# No "starting…" toast: mpv runs with --force-window=immediate, so the window is
# up before a notification would be, and a failure gets its own toast below.
"$HERE/view.sh" >"$LOG" 2>&1
status=$?

if [ "$status" -ne 0 ]; then
    # `die` prints "!! reason" in red; fall back to the last non-empty line.
    reason=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -m1 '^!! ' | sed 's/^!! //')
    [ -n "$reason" ] || reason=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -v '^$' | tail -1)
    [ -n "$reason" ] || reason="exited with status $status"
    notify -u critical "protoHDMIcapture failed" "$reason
Full log: $LOG"
fi

exit "$status"
