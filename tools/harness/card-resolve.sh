#!/usr/bin/env bash
# Which node resolve_card opens (2986f2d, 4ae6129, b8d0c8e), on a made-up
# set of devices: a DEV that is gone, not a capture node, or ignored is set
# aside for the pin, and with no pin the run stops; an ignored device is
# never opened, not even at the pin.
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"
# shellcheck source=lib/card.sh
. lib/card.sh

# node|serial|port|type|capture? (the card, a webcam and its metadata node)
D="$HARNESS_TMP/dev"
mkdir -p "$D"
DEVICES=(
    "$D/video1|111|port-card|UVC Camera (1e4e:7016)|y"
    "$D/video2||port-cam|OBSBOT Meet 2|y"
    "$D/video3||port-cam|OBSBOT Meet 2|n"
)
for row in "${DEVICES[@]}"; do : >"${row%%|*}"; done
_row() { local r; for r in "${DEVICES[@]}"; do [ "${r%%|*}" = "$1" ] && echo "$r"; done; return 0; }
_card_type() { _row "$1" | cut -d'|' -f4; }
_is_capture_node() { [ "$(_row "$1" | cut -d'|' -f5)" = y ]; }
card_list() { local r; for r in "${DEVICES[@]}"; do [ "${r##*|}" = y ] && echo "${r%|*}"; done; }

# expect DEV SERIAL PORT IGNORE -> "rc DEV" plus whether an error/warning was set
check() {
    local want="$1"; shift
    local got
    got=$(
        DEV="$1" CARD_SERIAL="$2" CARD_PORT="$3" IGNORE_CARDS_RE="$4" CONFIG_FILE=/x
        resolve_card; echo "$? ${DEV:-none}${CARD_WARNING:+ W}${CARD_ERROR:+ E}"
    )
    [ "$got" = "$want" ] || no "DEV='$1' serial='$2' port='$3' ignore='$4': got '$got', expected '$want'"
}
IGN='OBSBOT'
check "0 $D/video1"     "$D/video1" ""    ""          "$IGN"   # the card: kept, silent
check "0 $D/video1 W"   "$D/video2" "111" ""          "$IGN"   # ignored DEV, serial pin
check "0 $D/video1 W"   "$D/video0" "111" ""          "$IGN"   # vanished DEV, serial pin
check "1 none E"        "$D/video0" ""    ""          "$IGN"   # vanished DEV, no pin: stop
check "1 none E"        "$D/video3" ""    ""          "$IGN"   # metadata node, no pin: stop
check "1 none E"        ""          ""    "port-cam"  "$IGN"   # ignored camera at the pinned port
check "0 $D/video2"     ""          ""    "port-cam"  ""       # not ignored: only the list stops it
check "1 none E"        ""          "111" ""          "Camera" # list matching the pinned card
check "0 $D/video1"     ""          ""    ""          "$IGN"   # scan skips the ignored camera
finish
