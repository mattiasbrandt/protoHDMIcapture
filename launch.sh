#!/usr/bin/env bash
# GUI entry point for view.sh, used by the .desktop entry; works on any
# desktop, with Hyprland extras where available (see lib/desktop.sh).
#
# view.sh enforces its own rules however it is started: one window and one
# audio loopback at a time (a second launch focuses the running window), and
# a failure becomes a desktop notification when there is no terminal. This
# wrapper only adds what a menu launch needs:
#
#   1. first run: no config yet, so open a terminal with the setup wizard,
#      and start the feed only if it saved settings
#   2. menu defaults: REFRESH=match unless the config says otherwise, since a
#      menu has nowhere to type it
#   3. the run log, kept somewhere readable afterwards
#   4. a notification for a failure view.sh could not report itself
#
# Env vars are passed straight through, so the defaults can still be
# overridden from a terminal:
#
#     REFRESH=keep ./launch.sh

set -uo pipefail

HERE=$(dirname "$(readlink -f "$0")")
LOG="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture.log"
# shellcheck source=lib/desktop.sh
. "$HERE/lib/desktop.sh"
# shellcheck source=lib/card.sh
. "$HERE/lib/card.sh"
RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture"
mkdir -p "$RUN_DIR"

# --- first run: the wizard in a terminal ------------------------------------
# A menu launch has no terminal to ask in, so open one. Whether the terminal
# command blocks until its window closes differs per terminal, so the wait
# does not rely on it: the wizard holds setup.lock for as long as it runs,
# and blocking on that lock returns however it ends, closed window included.
run_setup_in_terminal() {
    rm -f "$RUN_DIR/setup.done" "$RUN_DIR/setup.pid"
    local -a cmd=("$HERE/view.sh" --setup --from-launcher)
    if command -v xdg-terminal-exec >/dev/null 2>&1; then
        setsid -f xdg-terminal-exec --app-id=protohdmicapture.setup \
            --title="protoHDMIcapture setup" -- "${cmd[@]}" >/dev/null 2>&1
    elif [ -n "${TERMINAL:-}" ] && command -v "$TERMINAL" >/dev/null 2>&1; then
        setsid -f "$TERMINAL" -e "${cmd[@]}" >/dev/null 2>&1
    elif command -v x-terminal-emulator >/dev/null 2>&1; then
        setsid -f x-terminal-emulator -e "${cmd[@]}" >/dev/null 2>&1
    else
        return 1
    fi
    local i
    for i in $(seq 150); do
        [ -s "$RUN_DIR/setup.pid" ] && break
        [ "$i" = 150 ] && return 1
        sleep 0.1
    done
    flock "$RUN_DIR/setup.lock" true
    return 0
}

if [ ! -e "$CONFIG_FILE" ] && [ -z "${NO_SETUP:-}" ]; then
    if ! run_setup_in_terminal; then
        desktop_notify normal "protoHDMIcapture: no settings yet" \
            "Could not open a terminal for setup; playing with defaults. Run ./view.sh --setup to choose settings."
    elif [ "$(cat "$RUN_DIR/setup.done" 2>/dev/null)" != saved ]; then
        exit 0   # setup was cancelled or closed: do not surprise anyone with a feed
    fi
fi

# A 60 fps feed on a faster fixed-refresh panel judders unless the panel is
# dropped to its nearest mode; view.sh restores the original mode on exit.
# Without Hyprland this only logs that the display was left alone. A REFRESH
# in the config wins: exporting one here would override the file.
declare -A launch_cfg
config_read launch_cfg
[ -n "${REFRESH:-}" ] || [ -n "${launch_cfg[REFRESH]:-}" ] || export REFRESH=match

# No "starting…" toast: mpv opens its window immediately, and failures get
# their own notification.
"$HERE/view.sh" >"$LOG" 2>&1
status=$?

# 3 = view.sh's die() already notified with the reason.
if [ "$status" -ne 0 ] && [ "$status" -ne 3 ]; then
    desktop_notify critical "protoHDMIcapture failed" "exited with status $status
Full log: $LOG"
fi

exit "$status"
