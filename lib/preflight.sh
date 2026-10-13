# shellcheck shell=bash
# Preflight: the tools a run needs, checked before anything starts, so a
# missing one is named instead of surfacing later as a wrong error ("no
# capture device found" for a missing v4l2-ctl). Sourced by view.sh and
# lib/setup.sh.
#
#   preflight_check BACKEND AUDIO REFRESH DEBUG [TOOL...]
#       fills PREFLIGHT_MISSING (required, each the tool, plus its package
#       on Arch)
#       and PREFLIGHT_OFF (extras, each "what is off, and why"); 1 if
#       anything required is missing. TOOL...: more required tools (the
#       wizard's ffmpeg).
#
# Required stops a run; an extra only turns a feature off. Package names are
# Arch's, from `pacman -Qo` (2026-10-13), and shown only where pacman is;
# elsewhere the tool name is the hint.

declare -A _PF_PKG=(
    [v4l2-ctl]=v4l-utils [flock]=util-linux [mpv]=mpv [gst-launch-1.0]=gstreamer
    [ffplay]=ffmpeg [ffmpeg]=ffmpeg [pactl]=libpulse [pw-loopback]=pipewire-audio
    [pw-link]=pipewire [jq]=jq [socat]=socat [python3]=python
)

_PF_ARCH=""; command -v pacman >/dev/null 2>&1 && _PF_ARCH=1
_pf_pkg() { [ -n "$_PF_ARCH" ] && [ -n "${_PF_PKG[$1]:-}" ] && printf ' (Arch package: %s)' "${_PF_PKG[$1]}"; }

_pf_need() {
    command -v "$1" >/dev/null 2>&1 && return 0
    PREFLIGHT_MISSING+=("$1$(_pf_pkg "$1")")
}

preflight_check() {
    local backend="$1" audio="$2" refresh="$3" debug="$4" t
    shift 4
    PREFLIGHT_MISSING=() PREFLIGHT_OFF=()
    # view.sh waits on the player and the loopback with `wait -n -p`.
    if (( BASH_VERSINFO[0] < 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] < 1) )); then
        PREFLIGHT_MISSING+=("bash 5.1 or newer (this is $BASH_VERSION)")
    fi
    _pf_need v4l2-ctl
    _pf_need flock
    case "$backend" in
        mpv)    _pf_need mpv ;;
        gst)    _pf_need gst-launch-1.0 ;;
        ffplay) _pf_need ffplay ;;
    esac
    # The card's audio input is found through pactl; the loopback plays it.
    case "$audio" in
        loopback) _pf_need pactl; _pf_need pw-loopback; _pf_need pw-link ;;
        mpv)      _pf_need pactl ;;
    esac
    for t in "$@"; do _pf_need "$t"; done

    # lib/desktop.sh fell back to its generic adapter for want of jq.
    [ -z "${DESKTOP_NOTE:-}" ] || PREFLIGHT_OFF+=("$DESKTOP_NOTE")
    if [ "$refresh" = match ] && [ "${DESKTOP:-}" = hyprland ] && ! command -v python3 >/dev/null 2>&1; then
        PREFLIGHT_OFF+=("REFRESH=match is off: it needs python3$(_pf_pkg python3)")
    fi
    if [ "$debug" = window ] && ! command -v socat >/dev/null 2>&1; then
        PREFLIGHT_OFF+=("the dashboard shows no player numbers: it reads mpv through socat$(_pf_pkg socat)")
    fi
    [ "${#PREFLIGHT_MISSING[@]}" -eq 0 ]
}
