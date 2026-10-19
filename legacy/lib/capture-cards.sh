# shellcheck shell=bash
# shellcheck disable=SC2034  # CC_* are outputs read by the caller
# Capture card profiles: what the project knows about each chip, one file per
# chip in capture-cards/<chip>.md. Sourced by lib/setup.sh and
# tools/capture-cards-table; the file shape is described in
# docs/capture-cards.md.
#
#   capture_card_lookup ID   set CC_* for a USB id (vid:pid); returns 0 for a
#                            profiled id, 1 for a known vendor, 2 for unknown
#   cc_field FILE KEY        the value of the first "KEY: value" line
#   cc_statuses FILE         "id|level|date|note" for each Status line
#
# CC_FILE CC_CHIP CC_HINT CC_LINK CC_BEST, CC_STATUS ("level date note" for
# the id, empty when the id is profiled but has no Status line) and
# CC_QUIRKS (the chip's quirks plus those tagged with this id, tags removed).

CC_DIR="${CC_DIR:-$(dirname "$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")")/capture-cards}"

# tr: a profile saved with CRLF line ends must still match its last id.
cc_field() { sed -n "s/^$2: //p" "$1" | head -1 | tr -d '\r'; }

cc_statuses() {
    awk '/^Status: / {
        id = $2; level = $3; date = $4
        note = $0; sub(/^Status: +[^ ]+ +[^ ]+ +[^ ]+ */, "", note)
        print id "|" level "|" date "|" note
    }' "$1"
}

capture_card_lookup() {
    local id="$1" f
    CC_FILE=""; CC_CHIP=""; CC_HINT=""; CC_LINK=""; CC_BEST=""; CC_STATUS=""; CC_QUIRKS=()
    for f in "$CC_DIR"/*.md; do
        [ -e "$f" ] || continue
        [[ " $(cc_field "$f" Ids) " == *" $id "* ]] && { CC_FILE="$f"; break; }
    done
    if [ -n "$CC_FILE" ]; then
        CC_CHIP=$(sed -n '1s/^# //p' "$CC_FILE")
        CC_HINT=$(cc_field "$CC_FILE" Hint)
        CC_LINK=$(cc_field "$CC_FILE" Link)
        CC_BEST=$(cc_field "$CC_FILE" Best)
        local sid level date note
        while IFS='|' read -r sid level date note; do
            [ "$sid" = "$id" ] && { CC_STATUS="$level $date${note:+ $note}"; break; }
        done < <(cc_statuses "$CC_FILE")
        mapfile -t CC_QUIRKS < <(awk -v id="$id" '
            /^## / { in_q = ($0 == "## Quirks"); next }
            in_q && /^- / {
                q = substr($0, 3)
                if (q ~ /^\[[^]]*\] /) {
                    tag = substr(q, 2, index(q, "]") - 2)
                    if (tag != id) next
                    q = substr(q, index(q, "]") + 2)
                }
                print q
            }' "$CC_FILE")
        return 0
    fi
    for f in "$CC_DIR"/*.md; do
        [ -e "$f" ] || continue
        if [[ " $(cc_field "$f" Vendors) " == *" ${id%%:*} "* ]]; then
            CC_CHIP="$(sed -n '1s/^# \([^ ]*\).*/\1/p' "$f") family, exact chip unknown"
            CC_HINT="go by the format list"
            return 1
        fi
    done
    CC_CHIP="unknown chip"
    CC_HINT="go by the format list"
    return 2
}
