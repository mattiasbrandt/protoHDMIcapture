# shellcheck shell=bash
# shellcheck disable=SC2034  # CARD_NAME, CARD_ERROR, SRC are outputs read by the caller
# Card module: settings, and which capture card to use. Sourced by view.sh and
# probe.sh.
#
#   load_config          read the config file into unset variables (env wins)
#   config_set K V       write one KEY=value into the config file
#   resolve_card         set DEV (the card's video node) and CARD_NAME
#   resolve_audio_source set SRC (the card's PipeWire source)
#   card_serial DEV      print the USB serial of a video node
#
# resolve_* return 1 with the reason in CARD_ERROR instead of exiting, so each
# caller decides how to fail.

CONFIG_FILE="${CONFIG_FILE:-${XDG_CONFIG_HOME:-$HOME/.config}/protohdmicapture/config}"

# Keys the config file may set. Anything else is reported, not silently used.
CONFIG_KEYS=(CARD_SERIAL IGNORE_CARDS_RE DEV SRC SINK RES FPS FMT DELAY LAT_MS
             BACKEND AUDIO FULLSCREEN SCALING LEVELS IPC REFRESH GST_IO_MODE)

# Parsed, never sourced: the file holds settings, not code. Values may be
# wrapped in single or double quotes. A variable already set in the
# environment is left alone, so `DELAY=0.1 ./view.sh` beats the file.
load_config() {
    [ -r "$CONFIG_FILE" ] || return 0
    local line key value
    while IFS= read -r line || [ -n "$line" ]; do
        line="${line#"${line%%[![:space:]]*}"}"
        case "$line" in ''|'#'*) continue ;; esac
        key="${line%%=*}"
        value="${line#*=}"
        if [ "$key" = "$line" ] || ! [[ " ${CONFIG_KEYS[*]} " == *" $key "* ]]; then
            printf '\033[33m** %s: ignoring unknown line: %s\033[0m\n' "$CONFIG_FILE" "$line" >&2
            continue
        fi
        case "$value" in
            \"*\") value="${value#\"}"; value="${value%\"}" ;;
            \'*\') value="${value#\'}"; value="${value%\'}" ;;
        esac
        [ -n "${!key+x}" ] || printf -v "$key" '%s' "$value"
    done <"$CONFIG_FILE"
}

config_set() {
    local key="$1" value="$2"
    mkdir -p "$(dirname "$CONFIG_FILE")" || return 1
    if [ -f "$CONFIG_FILE" ] && grep -q "^${key}=" "$CONFIG_FILE"; then
        sed -i "s|^${key}=.*|${key}=${value}|" "$CONFIG_FILE"
    else
        printf '%s=%s\n' "$key" "$value" >>"$CONFIG_FILE"
    fi
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

resolve_card() {
    CARD_ERROR=""
    if [ -z "${DEV:-}" ]; then
        local n
        for n in /dev/video*; do
            [ -e "$n" ] || continue
            _is_capture_node "$n" || continue
            if [ -n "${CARD_SERIAL:-}" ]; then
                [ "$(card_serial "$n")" = "$CARD_SERIAL" ] || continue
            elif [ -n "${IGNORE_CARDS_RE:-}" ]; then
                # Guarded: an empty pattern would match, and skip, every card.
                _card_type "$n" | grep -Eq "$IGNORE_CARDS_RE" && continue
            fi
            DEV="$n"; break
        done
    fi
    if [ -z "${DEV:-}" ]; then
        if [ -n "${CARD_SERIAL:-}" ]; then
            CARD_ERROR="no capture card with serial $CARD_SERIAL found; is it plugged in? (CARD_SERIAL in $CONFIG_FILE)"
        else
            CARD_ERROR="no capture device found; set DEV=/dev/videoN (run ./probe.sh)"
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
