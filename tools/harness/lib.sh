# shellcheck shell=bash
# Shared by the harnesses in tools/harness/: hardware-free checks of the
# functions behind past defects, run by tools/check. Each harness sources
# this, defines stand-ins for what it does not test, and calls ok/no.
# (protoHDMIcapture legacy's harnesses have their own, legacy/tests/lib.sh.)
#
#   ok MSG / no MSG        record a pass or a failure; finish exits 1 on any
#   finish                 the harness's exit status

cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../.." || exit 1
HARNESS_TMP=$(mktemp -d)
trap 'rm -rf "$HARNESS_TMP"' EXIT
_harness_fail=0

ok() { :; }
no() { printf '   %s: %s\n' "$(basename "$0" .sh)" "$*" >&2; _harness_fail=1; }
finish() { exit "$_harness_fail"; }
