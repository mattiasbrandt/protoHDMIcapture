#!/usr/bin/env bash
# lib/preflight.sh (a4fd771, fbc9776): a missing required tool is named,
# with its Arch package only where pacman is; an extra is only reported.
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"

# A PATH holding just the named tools (plus what the check itself runs).
path_with() {
    local d="$HARNESS_TMP/path-$RANDOM" t
    mkdir -p "$d"
    for t in "$@"; do ln -s "$(command -v "$t")" "$d/$t"; done
    printf '%s' "$d"
}
REQUIRED=(v4l2-ctl flock mpv pactl pw-loopback pw-link)

# run PATH-DIR DESKTOP_NOTE -> status, then the missing list, then the off list
run() {
    PATH="$1" DESKTOP_NOTE="$2" DESKTOP=generic /usr/bin/bash -c '
        . lib/preflight.sh
        preflight_check mpv loopback keep off; echo "rc=$?"
        printf "missing:%s\n" "${PREFLIGHT_MISSING[@]}"; printf "off:%s\n" "${PREFLIGHT_OFF[@]}"'
}
out=$(run "$(path_with "${REQUIRED[@]}")" "")
grep -qx 'rc=0' <<<"$out" || no "all tools present, yet: $out"

out=$(run "$(path_with flock mpv pactl pw-loopback pw-link pacman)" "")
grep -qx 'rc=1' <<<"$out" || no "no v4l2-ctl, yet passed"
grep -qx 'missing:v4l2-ctl (Arch package: v4l-utils)' <<<"$out" || no "no v4l2-ctl, with pacman: $out"

out=$(run "$(path_with flock mpv pactl pw-loopback pw-link)" "")
grep -qx 'missing:v4l2-ctl' <<<"$out" || no "no v4l2-ctl, no pacman, package still named: $out"

out=$(run "$(path_with "${REQUIRED[@]}")" "jq is gone")
grep -qx 'rc=0' <<<"$out" || no "an extra stopped the run: $out"
grep -qx 'off:jq is gone' <<<"$out" || no "the desktop note is not reported: $out"
finish
