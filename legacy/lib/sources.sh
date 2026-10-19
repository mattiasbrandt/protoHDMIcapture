# shellcheck shell=bash
# Source profiles: what the project knows about one kind of HDMI source, one
# file per source in sources/<id>.md. Sourced by lib/setup.sh; the overlay
# reads the same files. sources/generic.md is the checklist for any source,
# used when SOURCE is unset.
#
# File shape:
#   # <display name>                    first line
#   Suggest: KEY=value ...              viewer settings it suggests (may be empty)
#   Status: <tested|reported> <date|->  tested = played through a card here
#   Wake: <what wakes it>               optional; the overlay's no-signal banner
#   - <setting>: <value> (<menu path>). <why>     the setup tips, one per line
#   ## References                       links; ends the tips
#
#   source_ids               every profile id, generic last
#   source_name ID           its display name
#   source_suggest ID        its suggested KEY=value words, one per line
#   source_status ID         "level date"
#   source_tips ID           its tips, one per line

SOURCES_DIR="${SOURCES_DIR:-$(dirname "$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")")/sources}"

_source_file() { printf '%s/%s.md' "$SOURCES_DIR" "${1:-generic}"; }

source_ids() {
    local f id
    for f in "$SOURCES_DIR"/*.md; do
        [ -e "$f" ] || continue
        id=$(basename "$f" .md)
        [ "$id" = generic ] || echo "$id"
    done
    [ -e "$SOURCES_DIR/generic.md" ] && echo generic
}

source_name()    { sed -n '1s/^# //p' "$(_source_file "$1")"; }
source_status()  { sed -n 's/^Status: //p' "$(_source_file "$1")" | head -1; }
source_suggest() { sed -n 's/^Suggest:[[:space:]]*//p' "$(_source_file "$1")" | head -1 | tr -s ' ' '\n' | sed '/^$/d'; }
source_tips()    { awk '/^## / { exit } /^- / { print substr($0, 3) }' "$(_source_file "$1")"; }
