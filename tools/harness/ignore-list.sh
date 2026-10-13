#!/usr/bin/env bash
# The wizard's ignored devices (ignore_others, 5c4c9e3): a camera is added
# as a regex literal, a name that also matches the chosen card is refused,
# a known capture chip defaults to kept, a device already on the list is
# not asked about, and a chosen card the list matched is taken off it. The
# literal survives the config writer, quote included.
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"
# shellcheck source=lib/card.sh
. lib/card.sh
# shellcheck disable=SC1090
. <(fn_from lib/setup.sh _regex_literal _ignore_without ignore_others)

center() { :; }; note() { :; }; say() { :; }; ui_warn() { :; }
# Answers, taken in order; "" takes the prompt's default. Not read through
# $( ): a subshell would never advance the queue.
ANS=() A=""
_answer() { A="${ANS[0]-}"; ANS=("${ANS[@]:1}"); }
confirm()     { _answer; [ "$A" = y ]; }
confirm_yes() { _answer; [ "$A" != n ]; }
_usb_id() { case "$1" in /dev/videoC) echo 534d:2109 ;; *) echo 0000:0000 ;; esac; }
# As lib/capture-cards.sh: 0 or 1 a known chip, 2 unknown.
capture_card_lookup() { [ "$1" = 534d:2109 ] && return 0; return 2; }
declare -A NEW

# run LIST CARD A B C ANSWERS... -> the new list
# shellcheck disable=SC2034  # CARD_TYPE, nodes, types: read by ignore_others
run() {
    NEW[IGNORE_CARDS_RE]="$1"; CARD_TYPE="$2"
    nodes=(/dev/video1 /dev/videoA /dev/videoB /dev/videoC)
    types=("$2" "$3" "$4" "$5")
    ANS=("${@:6}")
    ignore_others 0 nodes types
    printf '%s' "${NEW[IGNORE_CARDS_RE]}"
}
CARD='UVC Camera (1e4e:7016)'
got=$(run "" "$CARD" "OBSBOT Meet 2" "$CARD" "USB Video" "" "")
[ "$got" = "OBSBOT Meet 2" ] || no "camera added, twin refused, chip kept by default: got '$got'"
got=$(run "OBSBOT" "$CARD" "OBSBOT Meet 2" "Integrated Camera" "USB Video" "y" "n")
[ "$got" = "OBSBOT|Integrated Camera" ] || no "already-ignored device asked about, or answers misread: got '$got'"
got=$(run 'OBSBOT|UVC Camera \(1e4e:7016\)|Meet 2' "$CARD" "OBSBOT x" "Integrated Camera" "USB Video" "n" "n")
[ "$got" = "OBSBOT|Meet 2" ] || no "chosen card not taken off the list: got '$got'"

lit=$(_regex_literal "$CARD")
grep -Eq -- "$lit" <<<"$CARD" || no "regex literal does not match its own name"
grep -Eq -- "$lit" <<<"UVC Camera" && no "regex literal matches a shorter name"

# Through the config writer and back.
q="Tom's cam [v2.0]"
v="OBSBOT|$lit|$(_regex_literal "$q")"
CONFIG_FILE="$HARNESS_TMP/config"
: >"$CONFIG_FILE"
config_write "IGNORE_CARDS_RE=$v" || no "config_write refused the list: $CONFIG_ERROR"
declare -A c
config_read c
[ "${c[IGNORE_CARDS_RE]-}" = "$v" ] || no "list changed through the config: '${c[IGNORE_CARDS_RE]-}'"
IGNORE_CARDS_RE="${c[IGNORE_CARDS_RE]-}"
card_ignored "$q" || no "a quoted name is not matched after the round trip"
finish
