#!/usr/bin/env bash
# The wizard's children do not hold setup.lock (c080309): a child that
# outlives the wizard (a dashboard terminal) would keep launch.sh waiting
# for "the wizard ended" for good.
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"
# shellcheck disable=SC1090
. <(fn_from lib/setup.sh run_child)

apply_to() { :; }
# shellcheck disable=SC2034  # read by run_child
MANAGED=(DEV) PREVIEW="$HARNESS_TMP/preview" CONFIG_FILE="$HARNESS_TMP/none"
exec {SETUP_LOCK}>"$HARNESS_TMP/setup.lock"
flock -n "$SETUP_LOCK" || { no "could not take the test lock"; finish; }

# The child counts its own fds that point at the lock file.
count='ls -l /proc/$$/fd | grep -c "setup\.lock"'
n=$(run_child bash -c "$count")
[ "$n" = 0 ] || no "run_child: the child holds setup.lock ($n)"
n=$( (run_child --exec bash -c "$count") )
[ "$n" = 0 ] || no "run_child --exec: the child holds setup.lock ($n)"
finish
