# shellcheck shell=bash
# shellcheck disable=SC2034  # CARD_NAME, CARD_ERROR, CARD_WARNING, CONFIG_ERROR, SRC are outputs read by the caller
# Card module: the config file, and which capture card to use. Sourced by
# view.sh and lib/setup.sh; it sources lib/settings.sh, the rows.
#
#   config_read ARRAY    the file's own values into an associative array
#   config_write K=V...  set keys in the config file (config_set K V for one)
#   config_unset K...    remove keys from the config file
#   card_list            "node|serial|port|card type" for every capture node
#   resolve_card         set DEV (the card's video node) and CARD_NAME; CARD_WARNING
#                        when a DEV that is no capture card was set aside
#   resolve_audio_source set SRC (the card's PipeWire source)
#   card_serial DEV      print the USB serial of a video node
#   card_port DEV        print the USB port path (udev ID_PATH) of a video node
#   card_usb_dir DEV     print the sysfs directory of the USB device behind it
#
# resolve_* return 1 with the reason in CARD_ERROR instead of exiting, so each
# caller decides how to fail.

CONFIG_FILE="${CONFIG_FILE:-${XDG_CONFIG_HOME:-$HOME/.config}/protohdmicapture/config}"

# shellcheck source=lib/settings.sh
. "$(dirname "${BASH_SOURCE[0]}")/settings.sh"

# Keys the config file may set: the settings (lib/settings.sh). Anything else
# is reported, not silently used.
mapfile -t CONFIG_KEYS < <(settings_with)

# Parsed, never sourced: the file holds settings, not code. Values may be
# wrapped in single or double quotes.
#
# _config_line LINE: sets _CK/_CV and returns 0 for a setting, 1 for a blank
# line or comment, 2 for anything else. The one parser every reader uses.
_config_line() {
    local line="${1#"${1%%[![:space:]]*}"}"
    case "$line" in ''|'#'*) return 1 ;; esac
    _CK="${line%%=*}"
    _CV="${line#*=}"
    if [ "$_CK" = "$line" ] || ! [[ " ${CONFIG_KEYS[*]} " == *" $_CK "* ]]; then
        return 2
    fi
    case "$_CV" in
        \"*\") _CV="${_CV#\"}"; _CV="${_CV%\"}" ;;
        \'*\') _CV="${_CV#\'}"; _CV="${_CV%\'}" ;;
    esac
    return 0
}

# config_read ARRAY: the file's own values into an associative array, ignoring
# the environment, so a caller can tell "set in the file" from "a default".
config_read() {
    local -n __config_read_out="$1"
    __config_read_out=()
    [ -r "$CONFIG_FILE" ] || return 0
    local line
    while IFS= read -r line || [ -n "$line" ]; do
        _config_line "$line" && __config_read_out[$_CK]="$_CV"
    done <"$CONFIG_FILE"
}

# config_write KEY=VALUE... / config_unset KEY...
# Rewrites the file in one pass: comments, blank and unknown lines stay where
# they are, a key already present is replaced in place, a new key is appended.
# Written to a temp file and renamed, so a crash never leaves half a config.
# Values outside a plain safe set are single-quoted; a value containing a
# single quote is refused (CONFIG_ERROR), since the parser has no escapes.
config_write() { _config_rewrite set "$@"; }
config_unset() { _config_rewrite unset "$@"; }
config_set()   { config_write "$1=$2"; }

_config_quote() {
    if [[ "$1" =~ ^[A-Za-z0-9._/:@+-]*$ ]]; then
        printf '%s' "$1"
    else
        printf "'%s'" "$1"
    fi
}

_config_rewrite() {
    local mode="$1"; shift
    CONFIG_ERROR=""
    local -A want=()
    local -a order=()
    local arg key
    for arg in "$@"; do
        if [ "$mode" = set ]; then
            key="${arg%%=*}"
            [ "$key" != "$arg" ] || { CONFIG_ERROR="not KEY=VALUE: $arg"; return 1; }
            [[ "${arg#*=}" != *"'"* ]] || { CONFIG_ERROR="$key: values cannot contain a single quote"; return 1; }
            want[$key]="${arg#*=}"
        else
            key="$arg"; want[$key]=""
        fi
        order+=("$key")
    done

    mkdir -p "$(dirname "$CONFIG_FILE")" || { CONFIG_ERROR="cannot create $(dirname "$CONFIG_FILE")"; return 1; }
    local tmp
    tmp=$(mktemp "$CONFIG_FILE.XXXXXX") || { CONFIG_ERROR="cannot write next to $CONFIG_FILE"; return 1; }
    local -A done_=()
    {
        if [ -r "$CONFIG_FILE" ]; then
            local line
            while IFS= read -r line || [ -n "$line" ]; do
                if _config_line "$line" && [ -n "${want[$_CK]+x}" ]; then
                    [ -z "${done_[$_CK]+x}" ] || continue      # drop duplicates
                    done_[$_CK]=1
                    [ "$mode" = set ] && printf '%s=%s\n' "$_CK" "$(_config_quote "${want[$_CK]}")"
                    continue
                fi
                printf '%s\n' "$line"
            done <"$CONFIG_FILE"
        else
            printf '# protoHDMIcapture settings, written by ./view.sh --setup.\n'
            printf '# One KEY=value per line; see config.example in the repo.\n'
        fi
        if [ "$mode" = set ]; then
            for key in "${order[@]}"; do
                [ -n "${done_[$key]+x}" ] && continue
                done_[$key]=1
                printf '%s=%s\n' "$key" "$(_config_quote "${want[$key]}")"
            done
        fi
    } >"$tmp" || { rm -f "$tmp"; CONFIG_ERROR="cannot write $tmp"; return 1; }
    mv -f "$tmp" "$CONFIG_FILE" || { rm -f "$tmp"; CONFIG_ERROR="cannot replace $CONFIG_FILE"; return 1; }
}

_card_info() { v4l2-ctl -d "$1" --info 2>/dev/null; }

card_serial() {
    _card_info "$1" | sed -n 's/^[[:space:]]*Serial[[:space:]]*:[[:space:]]*//p' | head -1
}

# The physical USB port, stable across replugs into the same port. It pins a
# card that reports no serial, and tells two identical such cards apart.
card_port() {
    udevadm info -q property -n "$1" 2>/dev/null | sed -n 's/^ID_PATH=//p'
}

# /sys/class/video4linux/videoN/device is the USB interface; its parent is
# the USB device, which carries the ids, the link speed and the card's sound
# interface. Nothing for a node that is not on USB.
card_usb_dir() {
    local intf
    intf=$(readlink -f "/sys/class/video4linux/$(basename "$1")/device" 2>/dev/null) || return 0
    [ -n "$intf" ] && [ -r "$(dirname "$intf")/idVendor" ] && dirname "$intf"
    return 0
}

_card_type() {
    _card_info "$1" | sed -n 's/^[[:space:]]*Card type[[:space:]]*:[[:space:]]*//p'
}

# A UVC device exposes a video node and a metadata node, and `--all` lists the
# whole device's capabilities on both. Only the node's own "Device Caps" block
# says which one actually delivers frames.
_is_capture_node() {
    _card_info "$1" | sed -n '/Device Caps/,/^[^[:space:]]/p' | grep -q 'Video Capture'
}

# "|", not a tab: read collapses runs of whitespace separators, so an empty
# serial between two tabs would shift the card type into the serial field.
card_list() {
    local n
    for n in /dev/video*; do
        [ -e "$n" ] || continue
        _is_capture_node "$n" || continue
        printf '%s|%s|%s|%s\n' "$n" "$(card_serial "$n")" "$(card_port "$n")" "$(_card_type "$n")"
    done
}

# An ignored device: its card type matches IGNORE_CARDS_RE. Guarded: an
# empty pattern would match, and so ignore, every device.
card_ignored() {
    [ -n "${IGNORE_CARDS_RE:-}" ] && grep -Eq "$IGNORE_CARDS_RE" <<<"$1"
}

resolve_card() {
    CARD_ERROR="" CARD_WARNING=""
    # A node number can move on a replug (another device may take it), so a
    # DEV that is not a capture card now, or is an ignored device, is set
    # aside for the pin or the scan below rather than opened.
    if [ -n "${DEV:-}" ]; then
        local t=""
        if [ ! -e "$DEV" ]; then
            CARD_WARNING="DEV=$DEV does not exist (any more); looking for the capture card instead"
        else
            t=$(_card_type "$DEV")
            if ! _is_capture_node "$DEV"; then
                CARD_WARNING="DEV=$DEV is ${t:-a device} now, not a video capture node; looking for the capture card instead"
            elif card_ignored "$t"; then
                CARD_WARNING="DEV=$DEV is ${t:-a device} now, an ignored device; looking for the capture card instead"
            fi
        fi
        [ -z "$CARD_WARNING" ] || DEV=""
    fi
    if [ -z "${DEV:-}" ]; then
        local n serial port type at_pin=""
        while IFS='|' read -r n serial port type; do
            # Ignored devices first: never opened, not even at the pin (a
            # camera plugged into the card's pinned USB port).
            if card_ignored "$type"; then
                if { [ -n "${CARD_SERIAL:-}" ] && [ "$serial" = "$CARD_SERIAL" ]; } \
                   || { [ -z "${CARD_SERIAL:-}" ] && [ -n "${CARD_PORT:-}" ] && [ "$port" = "$CARD_PORT" ]; }; then
                    at_pin="$n ($type)"
                fi
                continue
            fi
            if [ -n "${CARD_SERIAL:-}" ]; then
                [ "$serial" = "$CARD_SERIAL" ] || continue
            elif [ -n "${CARD_PORT:-}" ]; then
                [ "$port" = "$CARD_PORT" ] || continue
            fi
            DEV="$n"; break
        done < <(card_list)
    fi
    if [ -z "${DEV:-}" ]; then
        if [ -n "${at_pin:-}" ]; then
            CARD_ERROR="the device at the card pin, $at_pin, is an ignored device (IGNORE_CARDS_RE in $CONFIG_FILE): if it is the capture card, take it off that list; if not, plug the card back in where it was"
        elif [ -n "${CARD_SERIAL:-}" ]; then
            CARD_ERROR="no capture card with serial $CARD_SERIAL found; is it plugged in? (CARD_SERIAL in $CONFIG_FILE)"
        elif [ -n "${CARD_PORT:-}" ]; then
            CARD_ERROR="no capture card on USB port $CARD_PORT; is it plugged in, and into the same port? Re-run ./view.sh --setup if it moved"
        else
            CARD_ERROR="no capture device found; set DEV=/dev/videoN (or run ./view.sh --setup)"
        fi
        return 1
    fi
    CARD_NAME="$(_card_type "$DEV")"
}

# The card's own audio input is the PipeWire source whose sysfs.path lies
# under the same USB device as the video node. Works with or without a USB
# serial; "first USB source" would grab a headset mic instead, as observed on
# the original box.
resolve_audio_source() {
    CARD_ERROR=""
    [ -n "${SRC:-}" ] && return 0
    local usb
    usb="$(card_usb_dir "$DEV")"
    if [ -z "$usb" ]; then
        CARD_ERROR="$DEV is not a USB device, so its audio input cannot be found; set SRC=<name from 'pactl list short sources'>"
        return 1
    fi
    SRC=$(pactl list sources 2>/dev/null | awk -v want="${usb#/sys}/" '
        /^[[:space:]]*Name: /        { name = $2 }
        /^[[:space:]]*sysfs\.path = / {
            p = $0; sub(/^[^"]*"/, "", p); sub(/".*$/, "", p)
            if (index(p, want) == 1 && name !~ /\.monitor$/) { print name; exit }
        }')
    [ -n "$SRC" ] && return 0
    CARD_ERROR="no audio source on the card's USB device ($(basename "$usb")); set SRC=<name from 'pactl list short sources'>"
    return 1
}
