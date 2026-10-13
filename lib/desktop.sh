# shellcheck shell=bash
# shellcheck disable=SC2034  # DESKTOP_MSG is an output read by the caller
# Desktop module: the few things that depend on the compositor. Sourced by
# view.sh and launch.sh.
#
#   desktop_notify URGENCY SUMMARY [BODY]  desktop notification; no-op without notify-send
#   desktop_focus_pid PID                  raise that process's window; 1 if this desktop can't
#   desktop_refresh_match FPS              switch the focused monitor to its mode nearest FPS
#   desktop_refresh_restore                undo desktop_refresh_match; safe to call twice
#
# desktop_refresh_match leaves a one-line report in DESKTOP_MSG: returns 0 when
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
