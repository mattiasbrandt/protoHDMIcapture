# shellcheck shell=bash
# shellcheck disable=SC2034  # DESKTOP_MSG is an output read by the caller
# Desktop module: the few things that depend on the compositor. Sourced by
# view.sh and launch.sh.
#
#   desktop_notify URGENCY SUMMARY [BODY]  desktop notification; no-op without notify-send
#   desktop_focus_pid PID                  raise that process's window; 1 if this desktop can't
#   desktop_refresh_match FPS              switch the focused monitor to its mode nearest FPS
#   desktop_refresh_restore                undo desktop_refresh_match; safe to call twice
#   desktop_place_window PID WINDOW SIZE   shape the player's window where its own flags
#                                          cannot (size, border); 1 if nothing was done
#   desktop_can FEATURE                    0 if this adapter implements refresh_match or
#                                          place_window
#   desktop_outputs                        "name WxH@Hz [focused]" per monitor, if known
#   desktop_active_window / desktop_focus_window ID
#                                          remember and restore which window has focus
#
# desktop_place_window and desktop_refresh_match leave a one-line report in
# DESKTOP_MSG. desktop_refresh_match returns 0 when
# there is nothing to worry about, 1 when the display was left alone for a
# reason worth telling the user. It never exits.
#
# DESKTOP selects the adapter: "hyprland" when a Hyprland session is running,
# "generic" otherwise. DESKTOP=generic forces the plain behaviour anywhere.

_DESKTOP_LIB=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")

if [ -z "${DESKTOP:-}" ]; then
    if [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] && command -v hyprctl >/dev/null 2>&1; then
        DESKTOP=hyprland
    else
        DESKTOP=generic
    fi
fi

desktop_notify() {
    command -v notify-send >/dev/null 2>&1 || return 0
    notify-send -a protoHDMIcapture -u "$1" "$2" "${3:-}"
}

_DESKTOP_RESTORE=""

case "$DESKTOP" in
hyprland)
    # Every dispatch below MUST go through Lua: Omarchy configures Hyprland in
    # Lua, which rejects `hyprctl keyword` ("can't work with non-legacy
    # parsers") and the bare `dispatch focuswindow` form, silently changing
    # nothing.
    desktop_focus_pid() {
        hyprctl dispatch "hl.dsp.focus({ window = \"pid:$1\" })" >/dev/null 2>&1
    }

    desktop_can() { case "$1" in refresh_match|place_window) return 0 ;; esac; return 1; }

    desktop_active_window() { hyprctl activewindow -j 2>/dev/null | jq -r '.address // empty'; }
    desktop_focus_window() {
        [ -n "$1" ] && hyprctl dispatch "hl.dsp.focus({ window = \"address:$1\" })" >/dev/null 2>&1
    }

    desktop_outputs() {
        hyprctl monitors -j 2>/dev/null | jq -r '.[] |
            "\(.name) \(.width)x\(.height)@\(.refreshRate * 100 | round / 100)Hz\(if .focused then " focused" else "" end)"'
    }

    # Hyprland floats or tiles windows itself and ignores a client's size and
    # maximize requests, and it draws the border, not the client. So the
    # player's own flags are applied first and this finishes the job: float
    # + resize + centre for windowed, maximize + no border for borderless.
    # fullscreen needs nothing.
    desktop_place_window() {
        local pid="$1" mode="$2" size="$3" sel i
        sel="pid:$1"
        [ "$mode" = fullscreen ] && { DESKTOP_MSG="window: fullscreen, nothing to place"; return 1; }
        # The window maps some time after the process starts; wait for it.
        for i in $(seq 30); do
            hyprctl clients -j 2>/dev/null | jq -e --argjson p "$pid" 'any(.[]; .pid == $p)' >/dev/null && break
            [ "$i" = 30 ] && { DESKTOP_MSG="window: pid $pid never mapped a window"; return 1; }
            sleep 0.1
        done
        _hl() { hyprctl dispatch "$1" >/dev/null 2>&1; }
        case "$mode" in
        windowed)
            _hl "hl.dsp.window.float({ action = \"enable\", window = \"$sel\" })"
            _hl "hl.dsp.window.resize({ x = ${size%x*}, y = ${size#*x}, window = \"$sel\" })"
            _hl "hl.dsp.window.center({ window = \"$sel\" })"
            DESKTOP_MSG="window: floating ${size}, centred" ;;
        borderless)
            _hl "hl.dsp.window.set_prop({ prop = \"border_size\", value = \"0\", window = \"$sel\" })"
            _hl "hl.dsp.window.set_prop({ prop = \"rounding\", value = \"0\", window = \"$sel\" })"
            _hl "hl.dsp.window.fullscreen({ mode = \"maximized\", window = \"$sel\" })"
            DESKTOP_MSG="window: maximized, no border" ;;
        esac
    }

    desktop_refresh_match() {
        # Mode selection lives in pick-refresh-mode.py: Python braces inside
        # $( ) are brittle and unlintable.
        local plan name target restore cur chosen
        plan=$("$_DESKTOP_LIB/pick-refresh-mode.py" "$1" \
               < <(hyprctl monitors -j 2>/dev/null) 2>/dev/null) || true
        if [ -z "$plan" ]; then
            DESKTOP_MSG="could not work out a refresh plan; leaving the display alone"
            return 1
        fi
        IFS='|' read -r name target restore cur chosen <<<"$plan"
        if [ "$cur" = "$chosen" ]; then
            DESKTOP_MSG="display: $name already at ${cur}Hz, nothing to change"
            return 0
        fi
        if ! hyprctl eval "$target" >/dev/null 2>&1; then
            DESKTOP_MSG="refresh switch failed; leaving the display alone"
            return 1
        fi
        _DESKTOP_RESTORE="$restore"
        DESKTOP_MSG="display: $name ${cur}Hz -> ${chosen}Hz (restored on exit)"
    }

    desktop_refresh_restore() {
        [ -n "$_DESKTOP_RESTORE" ] || return 0
        hyprctl eval "$_DESKTOP_RESTORE" >/dev/null 2>&1
        _DESKTOP_RESTORE=""
    }
    ;;
generic)
    desktop_focus_pid() { return 1; }
    desktop_can() { return 1; }
    desktop_active_window() { :; }
    desktop_focus_window() { return 1; }
    desktop_outputs() { return 1; }
    desktop_place_window() {
        DESKTOP_MSG="window: left to the player's own flags"
        return 1
    }
    desktop_refresh_match() {
        DESKTOP_MSG="REFRESH=match is only implemented for Hyprland; leaving the display alone"
        return 1
    }
    desktop_refresh_restore() { :; }
    ;;
*)
    printf '!! unknown DESKTOP=%s (hyprland | generic)\n' "$DESKTOP" >&2
    exit 1
    ;;
esac
