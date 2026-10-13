# shellcheck shell=bash
# shellcheck disable=SC2034  # colour variables are used by the sourcing script
# Wizard UI: a centered-terminal layout, adapted from the wizard library in
# kbd-diag.sh. Sourced by lib/setup.sh and lib/debug.sh.
#
#   stage TITLE              clear, then "▸ Stage n/N · TITLE" (TOTAL_STAGES)
#   say / note / ui_warn / ui_bad / ui_ok TEXT     centered lines
#   pause [TEXT]             wait for Enter
#   confirm Q / confirm_yes Q        [y/N] / [Y/n]; 0 = yes
#   choose VAR PROMPT DEFAULT ITEM...  numbered menu; ITEM is "value|label"
#   ask VAR PROMPT DEFAULT [REGEX]     free text, re-asked until REGEX matches
#   ui_restore               put the terminal back (cursor, echo); for traps
#
# Prompts read from the terminal. With stdout not a terminal, colours are off.

if [[ -t 1 ]] && command -v tput >/dev/null 2>&1 && [[ "$(tput colors 2>/dev/null || echo 0)" -ge 8 ]]; then
    BOLD=$(tput bold); DIM=$(tput dim); RESET=$(tput sgr0); REV=$(tput rev)
    BLUE=$(tput setaf 4); GREEN=$(tput setaf 2); YELLOW=$(tput setaf 3); RED=$(tput setaf 1)
else
    BOLD=""; DIM=""; RESET=""; REV=""; BLUE=""; GREEN=""; YELLOW=""; RED=""
fi

COLS=$(tput cols 2>/dev/null || echo 80)
TOTAL_STAGES=${TOTAL_STAGES:-0}
_STAGE_INDEX=0

_clear() {
    [[ -t 1 ]] || return 0
    COLS=$(tput cols 2>/dev/null || echo 80)
    if command -v tput >/dev/null 2>&1; then tput clear; else printf '\033[2J\033[3J\033[H'; fi
}

_ui_width() { local plain; plain=$(printf '%s' "$1" | sed 's/\x1b\[[0-9;]*m//g; s/\x1b([A-Z]//g'); echo "${#plain}"; }

# center TEXT: one line centered in the terminal (ANSI-aware).
center() {
    local line="${1:-}" pad
    pad=$(( (COLS - $(_ui_width "$line")) / 2 )); (( pad < 0 )) && pad=0
    printf '%*s%s\n' "$pad" '' "$line"
}

# ceprompt FMT ARGS: centered printf without a newline, for prompts.
ceprompt() {
    local line pad
    # shellcheck disable=SC2059  # the format is the caller's
    line=$(printf "$@")
    pad=$(( (COLS - $(_ui_width "$line")) / 2 )); (( pad < 0 )) && pad=0
    printf '%*s%s' "$pad" '' "$line"
}

stage() {
    _clear
    _STAGE_INDEX=$((_STAGE_INDEX + 1))
    center ""
    center "${BOLD}${BLUE}▸ Stage $_STAGE_INDEX/$TOTAL_STAGES · $1${RESET}"
    center ""
}

say()     { center "$1"; }
note()    { center "${DIM}$1${RESET}"; }
ui_warn() { center "${YELLOW}⚠️  $1${RESET}"; }
ui_bad()  { center "${RED}✗ $1${RESET}"; }
ui_ok()   { center "${GREEN}✓ $1${RESET}"; }

# ui_list ITEM...: dim bullets, each wrapped with a hanging indent, centred
# as one block so the bullets line up (plain text only: no colour codes).
ui_list() {
    local w=$(( COLS - 8 )) item line
    (( w > 76 )) && w=76
    (( w < 20 )) && w=20
    local -a rows=()
    for item in "$@"; do
        local first=1
        while IFS= read -r line; do
            if [ -n "$first" ]; then rows+=("• $line"); first=""; else rows+=("  $line"); fi
        done < <(fold -s -w "$(( w - 2 ))" <<<"$item" | sed 's/ *$//')
    done
    local rw=0 n
    for line in "${rows[@]}"; do n=${#line}; (( n > rw )) && rw=$n; done
    local left=$(( (COLS - rw) / 2 )); (( left < 0 )) && left=0
    for line in "${rows[@]}"; do printf '%*s%s%s%s\n' "$left" '' "$DIM" "$line" "$RESET"; done
}

pause() {
    ceprompt '%s%s%s ' "$DIM" "${1:-⏎  Press Enter to continue}" "$RESET"
    read -r _ || true
}

confirm() {
    local reply=""
    ceprompt '%s❓ %s [y/N]%s ' "$YELLOW" "$1" "$RESET"
    read -r reply || true
    [[ "$reply" =~ ^[Yy] ]]
}

# Enter means yes, but end of input means no: a closed terminal must never
# answer "yes" to starting something.
confirm_yes() {
    local reply=""
    ceprompt '%s❓ %s [Y/n]%s ' "$YELLOW" "$1" "$RESET"
    read -r reply || return 1
    ! [[ "$reply" =~ ^[Nn] ]]
}

# choose VAR PROMPT DEFAULT ITEM...
# Each ITEM is "value|label" (or just "value"). The item whose value equals
# DEFAULT is marked and taken on a bare Enter. A trailing tag after a second
# "|" ("value|label|recommended") is shown next to the label.
choose() {
    local -n __choose_out="$1"
    local prompt="$2" default="$3"; shift 3
    local -a values=() labels=() tags=()
    local item v rest l t i def_i=""
    for item in "$@"; do
        v="${item%%|*}"; rest="${item#*|}"
        [ "$rest" = "$item" ] && rest="$v"
        l="${rest%%|*}"; t=""
        [ "$l" != "$rest" ] && t="${rest#*|}"
        values+=("$v"); labels+=("$l"); tags+=("$t")
    done
    # Rows are built first and the block is centered as a whole, so numbers
    # and labels line up in columns instead of each row centering itself.
    local w=0 n
    for l in "${labels[@]}"; do n=$(_ui_width "$l"); (( n > w )) && w=$n; done
    local -a rows=()
    local rw=0
    for i in "${!values[@]}"; do
        local mark=""
        [ "${values[$i]}" = "$default" ] && { mark=" ${GREEN}◂ ${CHOOSE_DEFAULT_LABEL:-current}${RESET}"; def_i=$((i + 1)); }
        [ -n "${tags[$i]}" ] && mark="$mark ${BLUE}◂ ${tags[$i]}${RESET}"
        local pad=$(( w - $(_ui_width "${labels[$i]}") ))
        rows+=("$(printf '%s%2d%s  %s%*s%s' "$BOLD" $((i + 1)) "$RESET" "${labels[$i]}" "$pad" '' "$mark")")
        n=$(_ui_width "${rows[-1]}"); (( n > rw )) && rw=$n
    done
    local left=$(( (COLS - rw) / 2 )); (( left < 0 )) && left=0
    for l in "${rows[@]}"; do printf '%*s%s\n' "$left" '' "$l"; done
    center ""
    local reply
    while true; do
        if [ -n "$def_i" ]; then
            ceprompt '%s%s [1-%d, Enter = %d]:%s ' "$BOLD" "$prompt" "${#values[@]}" "$def_i" "$RESET"
        else
            ceprompt '%s%s [1-%d]:%s ' "$BOLD" "$prompt" "${#values[@]}" "$RESET"
        fi
        read -r reply || reply=""
        [ -z "$reply" ] && [ -n "$def_i" ] && reply="$def_i"
        if [[ "$reply" =~ ^[0-9]+$ ]] && (( reply >= 1 && reply <= ${#values[@]} )); then
            __choose_out="${values[$((reply - 1))]}"
            return 0
        fi
        # EOF with no default: give up rather than loop forever.
        [ -t 0 ] || return 1
    done
}

# ask VAR PROMPT DEFAULT [REGEX]
ask() {
    local -n __ask_out="$1"
    local prompt="$2" default="$3" re="${4:-}" reply
    while true; do
        ceprompt '%s%s [Enter = "%s"]:%s ' "$BOLD" "$prompt" "$default" "$RESET"
        read -r reply || reply=""
        [ -z "$reply" ] && reply="$default"
        if [ -z "$re" ] || [[ "$reply" =~ $re ]]; then
            __ask_out="$reply"
            return 0
        fi
        center "${YELLOW}\"$reply\" is not valid here${RESET}"
        [ -t 0 ] || return 1
    done
}

ui_restore() {
    [[ -t 0 ]] && stty sane 2>/dev/null
    [[ -t 1 ]] && command -v tput >/dev/null 2>&1 && tput cnorm 2>/dev/null
    return 0
}
