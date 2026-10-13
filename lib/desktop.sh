# shellcheck shell=bash
# shellcheck disable=SC2034  # DESKTOP_MSG is an output read by the caller
# Desktop module: the few things that depend on the compositor. Sourced by
# view.sh and launch.sh.
#
#   desktop_notify URGENCY SUMMARY [BODY]  desktop notification; no-op without notify-send
#   desktop_focus_pid PID                  raise that process's window; 1 if this desktop can't
#   desktop_refresh_match FPS RECORD       switch the focused monitor to its mode nearest FPS;
#                                          RECORD (a file) keeps the way back until restored
#   desktop_refresh_restore                undo desktop_refresh_match; safe to call twice;
#                                          1 (DESKTOP_MSG says why) if the undo failed
#   desktop_refresh_recover RECORD         a RECORD left by a session that was killed:
#                                          restore that monitor if it is still at the
#                                          rate the session set, else leave it; 1 if it
#                                          could not tell (DESKTOP_MSG says what it did)
#   desktop_place_window PID WINDOW SIZE   shape the player's window where its own flags
#                                          cannot (size, border); 1 if nothing was done
#   desktop_can FEATURE                    0 if this adapter implements refresh_match or
#                                          place_window
#   desktop_outputs                        "name WxH@Hz [focused]" per monitor, if known
#   desktop_active_window / desktop_focus_window ID
#                                          remember and restore which window has focus
#   desktop_open_terminal APPID TITLE CMD...
#                                          run CMD in a new terminal window, detached;
#                                          1 if no terminal could be found
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
    # --: a message starting with "-" (e.g. "--tune needs ...") is text, not an option.
    notify-send -a protoHDMIcapture -u "$1" -- "$2" "${3:-}"
}

_DESKTOP_RESTORE=""
_DESKTOP_RECORD=""

# Any desktop: the default-terminal spec first, then $TERMINAL, then Debian's
# alternative. Detached with setsid so it outlives the caller, and so whether
# the terminal command blocks until its window closes does not matter.
desktop_open_terminal() {
    local app="$1" title="$2"; shift 2
    if command -v xdg-terminal-exec >/dev/null 2>&1; then
        setsid -f xdg-terminal-exec --app-id="$app" --title="$title" -- "$@" >/dev/null 2>&1
    elif [ -n "${TERMINAL:-}" ] && command -v "$TERMINAL" >/dev/null 2>&1; then
        setsid -f "$TERMINAL" -e "$@" >/dev/null 2>&1
    elif command -v x-terminal-emulator >/dev/null 2>&1; then
        setsid -f x-terminal-emulator -e "$@" >/dev/null 2>&1
    else
        return 1
    fi
}

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
    # + resize + centre for windowed, maximize + no border for borderless,
    # true fullscreen for fullscreen. Each mode first undoes what the others
    # set, so this also serves switching modes on a live window (the f key).
    desktop_place_window() {
        local pid="$1" mode="$2" size="$3" sel i
        sel="pid:$1"
        # The window maps some time after the process starts; wait for it.
        for i in $(seq 30); do
            hyprctl clients -j 2>/dev/null | jq -e --argjson p "$pid" 'any(.[]; .pid == $p)' >/dev/null && break
            [ "$i" = 30 ] && { DESKTOP_MSG="window: pid $pid never mapped a window"; return 1; }
            sleep 0.1
        done
        _hl() { hyprctl dispatch "$1" >/dev/null 2>&1; }
        case "$mode" in
        fullscreen)
            _hl "hl.dsp.window.set_prop({ prop = \"border_size\", value = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.set_prop({ prop = \"rounding\", value = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.fullscreen({ mode = \"fullscreen\", action = \"set\", window = \"$sel\" })"
            DESKTOP_MSG="window: fullscreen" ;;
        windowed)
            _hl "hl.dsp.window.fullscreen({ mode = \"fullscreen\", action = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.fullscreen({ mode = \"maximized\", action = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.set_prop({ prop = \"border_size\", value = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.set_prop({ prop = \"rounding\", value = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.float({ action = \"enable\", window = \"$sel\" })"
            _hl "hl.dsp.window.resize({ x = ${size%x*}, y = ${size#*x}, window = \"$sel\" })"
            _hl "hl.dsp.window.center({ window = \"$sel\" })"
            DESKTOP_MSG="window: floating ${size}, centred" ;;
        borderless)
            _hl "hl.dsp.window.fullscreen({ mode = \"fullscreen\", action = \"unset\", window = \"$sel\" })"
            _hl "hl.dsp.window.set_prop({ prop = \"border_size\", value = \"0\", window = \"$sel\" })"
            _hl "hl.dsp.window.set_prop({ prop = \"rounding\", value = \"0\", window = \"$sel\" })"
            _hl "hl.dsp.window.fullscreen({ mode = \"maximized\", action = \"set\", window = \"$sel\" })"
            DESKTOP_MSG="window: maximized, no border" ;;
        esac
    }

    desktop_refresh_match() {
        # Mode selection lives in pick-refresh-mode.py: Python braces inside
        # $( ) are brittle and unlintable.
        local plan name target restore cur chosen
        # On failure the script prints only its reason, on stderr.
        if ! plan=$("$_DESKTOP_LIB/pick-refresh-mode.py" "$1" \
                    < <(hyprctl monitors -j 2>/dev/null) 2>&1) || [ -z "$plan" ]; then
            DESKTOP_MSG="could not work out a refresh plan (${plan:-no reason given}); leaving the display alone"
            return 1
        fi
        IFS='|' read -r name target restore cur chosen <<<"$plan"
        if [ "$cur" = "$chosen" ]; then
            DESKTOP_MSG="display: $name already at ${cur}Hz, nothing to change"
            return 0
        fi
        # Recorded before the switch: a kill between the two must not lose
        # the way back (desktop_refresh_recover sorts out which it was).
        printf '%s|%s|%s|%s\n' "$name" "$chosen" "$cur" "$restore" >"$2"
        if ! hyprctl eval "$target" >/dev/null 2>&1; then
            rm -f "$2"
            DESKTOP_MSG="refresh switch failed; leaving the display alone"
            return 1
        fi
        _DESKTOP_RESTORE="$restore"
        _DESKTOP_RECORD="$2"
        DESKTOP_MSG="display: $name ${cur}Hz -> ${chosen}Hz (restored on exit)"
    }

    desktop_refresh_restore() {
        [ -n "$_DESKTOP_RESTORE" ] || return 0
        local restore="$_DESKTOP_RESTORE"
        _DESKTOP_RESTORE=""
        rm -f "$_DESKTOP_RECORD"
        hyprctl eval "$restore" >/dev/null 2>&1 && return 0
        DESKTOP_MSG="could not restore the display's refresh rate; run: hyprctl eval '$restore'"
        return 1
    }

    desktop_refresh_recover() {
        local name set was restore now
        IFS='|' read -r name set was restore <"$1" || { rm -f "$1"; DESKTOP_MSG="unreadable $1; dropped"; return 1; }
        rm -f "$1"
        now=$(hyprctl monitors -j 2>/dev/null | jq -r --arg n "$name" '.[] | select(.name == $n) | .refreshRate' 2>/dev/null)
        if [ -z "$now" ]; then
            DESKTOP_MSG="a killed session left $name at ${set}Hz, but its rate cannot be read now; to restore: hyprctl eval '$restore'"
            return 1
        fi
        if ! awk -v a="$now" -v b="$set" 'BEGIN { exit !((a - b) ^ 2 < 0.0001) }'; then
            DESKTOP_MSG="display: $name is at ${now}Hz, not the ${set}Hz a killed session set; left as it is"
            return 0
        fi
        if ! hyprctl eval "$restore" >/dev/null 2>&1; then
            DESKTOP_MSG="could not restore $name to ${was}Hz after a killed session; run: hyprctl eval '$restore'"
            return 1
        fi
        DESKTOP_MSG="display: $name ${set}Hz -> ${was}Hz, restored after a killed session"
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
    # Only the hyprland adapter switches rates, so a record here came from a
    # session under another adapter (DESKTOP=generic forced); leave it.
    desktop_refresh_recover() {
        DESKTOP_MSG="a killed session's refresh record is left for the hyprland adapter: $1"
        return 1
    }
    ;;
*)
    printf '!! unknown DESKTOP=%s (hyprland | generic)\n' "$DESKTOP" >&2
    exit 1
    ;;
esac
