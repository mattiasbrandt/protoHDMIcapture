#!/usr/bin/env bash
# view.sh's quit bound and wait loop (arm_reaper, wait_player): a USR2 that
# lands before the wait still bounds a hung quit (3c91db3); a player that
# exits on its own leaves no timer behind; a loopback that stops mid-feed is
# reported and the wait goes on (6fcbcec).
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"
# shellcheck disable=SC1090
. <(fn_from view.sh arm_reaper wait_player)

warn() { echo "$*" >>"$HARNESS_TMP/warn"; }
apply_sync_request() { :; }
audio_failed() { echo "$*" >>"$HARNESS_TMP/failed"; }
_close_locks() { :; }
# shellcheck disable=SC2034  # read by the functions under test
RUN_DIR="$HARNESS_TMP" SYNC_REQUESTED="" REAPER_PID="" LOOPBACK_PID=""
trap arm_reaper USR2

# A hung quit: the player ignores TERM, and USR2 arrives before any wait.
bash -c 'trap "" TERM; sleep 30' & PLAYER_PID=$!
kill -USR2 $$
SECONDS=0; wait_player serve-session; rc=$?
[ "$rc" = 137 ] || no "hung quit: status $rc, expected 137 (SIGKILL)"
[ "$SECONDS" -le 4 ] || no "hung quit: took ${SECONDS}s, bound is 2 s"
grep -q 'did not finish quitting' "$HARNESS_TMP/warn" 2>/dev/null || no "hung quit: no warning"
[ -z "$PLAYER_PID" ] || no "PLAYER_PID not cleared after the reap"

# A quit that finishes on its own: no reaper outlives it.
bash -c 'sleep 0.3' & PLAYER_PID=$!
kill -USR2 $$
wait_player serve-session; rc=$?
[ "$rc" = 0 ] || no "clean quit: status $rc"
[ -z "$REAPER_PID" ] || no "clean quit: reaper $REAPER_PID still recorded"
sleep 2.2
[ -z "$(jobs -rp)" ] || no "clean quit: background jobs left: $(jobs -rp | tr '\n' ' ')"

# A loopback that stops mid-feed: reported, and the player is still waited for.
: >"$HARNESS_TMP/failed"
bash -c 'sleep 0.2' & LOOPBACK_PID=$!
bash -c 'sleep 0.8' & PLAYER_PID=$!
wait_player serve-session; rc=$?
[ "$rc" = 0 ] || no "loopback stop: player status $rc"
grep -q 'stopped mid-feed' "$HARNESS_TMP/failed" || no "loopback stop: not reported"
[ -z "$LOOPBACK_PID" ] || no "loopback stop: LOOPBACK_PID not cleared"
finish
