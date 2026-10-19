# shellcheck shell=bash
# Shared by protoHDMIcapture legacy's harnesses in legacy/tests/:
# hardware-free checks of the functions behind past defects. Best effort
# (ADR 0002): no check runs them; run one by hand, e.g.
# legacy/tests/card-resolve.sh, and it exits non-zero on a failure. Each
# harness sources this, defines stand-ins for what it does not test, and
# calls ok/no. Paths are from legacy/.
#
#   fn_from FILE NAME...   print the named functions' definitions from FILE
#                          (view.sh and lib/setup.sh run when sourced, so
#                          their functions are lifted out instead)
#   ok MSG / no MSG        record a pass or a failure; finish exits 1 on any
#   finish                 the harness's exit status

cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." || exit 1
HARNESS_TMP=$(mktemp -d)
trap 'rm -rf "$HARNESS_TMP"' EXIT
_harness_fail=0

fn_from() {
    local file="$1" name
    shift
    for name in "$@"; do
        grep -q "^$name() {" "$file" || { echo "fn_from: no $name() in $file" >&2; return 1; }
        sed -n "/^$name() {/,/^}/p" "$file"
    done
}

ok() { :; }
no() { printf '   %s: %s\n' "$(basename "$0" .sh)" "$*" >&2; _harness_fail=1; }
finish() { exit "$_harness_fail"; }
