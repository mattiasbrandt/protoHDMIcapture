#!/usr/bin/env bash
# GUI entry point for view.sh, used by the .desktop entry; works on any
# desktop, with Hyprland extras where available (see lib/desktop.sh).
#
# view.sh enforces its own rules however it is started: one window and one
# audio loopback at a time (a second launch focuses the running window), and
# a failure becomes a desktop notification when there is no terminal. This
# wrapper only adds what a menu launch needs:
#
#   1. menu defaults: REFRESH=match, since a menu has nowhere to type it
#   2. the run log, kept somewhere readable afterwards
#   3. a notification for a failure view.sh could not report itself
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

# A 60 fps feed on a faster fixed-refresh panel judders unless the panel is
# dropped to its nearest mode; view.sh restores the original mode on exit.
# Without Hyprland this only logs that the display was left alone.
export REFRESH="${REFRESH:-match}"

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
