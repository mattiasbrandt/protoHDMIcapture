# shellcheck shell=bash
# shellcheck disable=SC2034  # CARD_NAME, CARD_ERROR, CONFIG_ERROR, SRC are outputs read by the caller
# Card module: settings, and which capture card to use. Sourced by view.sh and
# lib/setup.sh.
#
#   load_config          read the config file into unset variables (env wins)
#   config_read ARRAY    the file's own values into an associative array
#   config_write K=V...  set keys in the config file (config_set K V for one)
#   config_unset K...    remove keys from the config file
#   card_list            "node|serial|card type" for every capture node
#   resolve_card         set DEV (the card's video node) and CARD_NAME
#   resolve_audio_source set SRC (the card's PipeWire source)
#   card_serial DEV      print the USB serial of a video node
#
# resolve_* return 1 with the reason in CARD_ERROR instead of exiting, so each
# caller decides how to fail.

CONFIG_FILE="${CONFIG_FILE:-${XDG_CONFIG_HOME:-$HOME/.config}/protohdmicapture/config}"

# Keys the config file may set. Anything else is reported, not silently used.
CONFIG_KEYS=(CARD_SERIAL IGNORE_CARDS_RE DEV SRC SINK RES FPS FMT DELAY LAT_MS
             BACKEND AUDIO WINDOW WINDOW_SIZE SCALING LEVELS IPC REFRESH GST_IO_MODE
             FULLSCREEN)   # FULLSCREEN: deprecated alias, still read

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

# A variable already set in the environment is left alone, so
# `DELAY=0.1 ./view.sh` beats the file.
load_config() {
    [ -r "$CONFIG_FILE" ] || return 0
    local line rc
    while IFS= read -r line || [ -n "$line" ]; do
        _config_line "$line"; rc=$?
        if [ "$rc" = 2 ]; then
            printf '\033[33m** %s: ignoring unknown line: %s\033[0m\n' "$CONFIG_FILE" "$line" >&2
        elif [ "$rc" = 0 ]; then
            [ -n "${!_CK+x}" ] || printf -v "$_CK" '%s' "$_CV"
        fi
    done <"$CONFIG_FILE"
}

# config_read ARRAY: the file's own values into an associative array, ignoring
# the environment, so a caller can tell "set in the file" from "a default".
config_read() {
    local -n _cfg="$1"
    _cfg=()
    [ -r "$CONFIG_FILE" ] || return 0
    local line
    while IFS= read -r line || [ -n "$line" ]; do
        _config_line "$line" && _cfg[$_CK]="$_CV"
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
        printf '%s|%s|%s\n' "$n" "$(card_serial "$n")" "$(_card_type "$n")"
    done
}

resolve_card() {
    CARD_ERROR=""
    if [ -z "${DEV:-}" ]; then
        local n serial type
        while IFS='|' read -r n serial type; do
            if [ -n "${CARD_SERIAL:-}" ]; then
                [ "$serial" = "$CARD_SERIAL" ] || continue
            elif [ -n "${IGNORE_CARDS_RE:-}" ]; then
                # Guarded: an empty pattern would match, and skip, every card.
                grep -Eq "$IGNORE_CARDS_RE" <<<"$type" && continue
            fi
            DEV="$n"; break
        done < <(card_list)
    fi
    if [ -z "${DEV:-}" ]; then
        if [ -n "${CARD_SERIAL:-}" ]; then
            CARD_ERROR="no capture card with serial $CARD_SERIAL found; is it plugged in? (CARD_SERIAL in $CONFIG_FILE)"
        else
            CARD_ERROR="no capture device found; set DEV=/dev/videoN (or run ./view.sh --setup)"
        fi
        return 1
    fi
    [ -e "$DEV" ] || { CARD_ERROR="$DEV does not exist"; return 1; }
    CARD_NAME="$(_card_type "$DEV")"
}

# Match the card's own USB serial, which PipeWire embeds in the node name.
# "First USB source" grabs a headset mic instead; observed on the original box.
resolve_audio_source() {
    CARD_ERROR=""
    [ -n "${SRC:-}" ] && return 0
    local serial
    serial="$(card_serial "$DEV")"
    if [ -n "$serial" ]; then
        SRC=$(pactl list short sources 2>/dev/null \
              | awk '$2 !~ /\.monitor$/ {print $2}' \
              | grep -F "$serial" | head -1)
    fi
    [ -n "$SRC" ] && return 0
    CARD_ERROR="no audio source matches the card's serial '${serial:-<none>}'; set SRC=<name from 'pactl list short sources'>"
    return 1
}
