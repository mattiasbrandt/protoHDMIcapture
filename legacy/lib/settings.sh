# shellcheck shell=bash
# shellcheck disable=SC2034  # SETTING_FROM, SETTINGS_ERRORS, SETTINGS_NOTES are read by the caller
# Settings module: one row per setting, and everything that follows from the
# rows. Sourced by lib/card.sh (so by view.sh and lib/setup.sh), in
# protoHDMIcapture legacy (legacy/).
#
#   settings_resolve          fill every setting from the environment, else the
#                             saved settings (CONFIG_FILE), else its default;
#                             SETTING_FROM[name] = env | saved | default
#   settings_validate         check every value against its row; 1 with
#                             SETTINGS_ERRORS when any is out of bounds
#   settings_with FLAG        the names carrying a flag (save, wizard, pin,
#                             suggest, empty); `ignored-by=PLAYER` for the
#                             player-specific settings PLAYER does not honour;
#                             no argument for every name
#   settings_fact NAME WHAT   one fact: default | values | players | flags
#
# SETTINGS_NOTES collects what resolve wants said (a deprecated alias in use).
# Reading and writing the config file stays in lib/card.sh (_config_line,
# config_read, config_write), which this module calls at run time.
#
# The rows are settings.rows at the repo root, read here at run time (the
# format is described there).
_SETTINGS_ROWS_FILE="$(dirname "$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")")/settings.rows"

SETTINGS=()
declare -A _S_DEFAULT=() _S_VALUES=() _S_PLAYERS=() _S_FLAGS=()
_settings_load() {
    local line name def values players flags
    while IFS= read -r line; do
        [ -n "${line// /}" ] && [[ "$line" != '#'* ]] || continue
        IFS='|' read -r name def values players flags <<<"$line"
        name="${name// /}"
        def="${def%"${def##*[! ]}"}"; values="${values// /}"; players="${players// /}"; flags="${flags// /}"
        SETTINGS+=("$name")
        _S_DEFAULT[$name]="$def"; _S_VALUES[$name]="$values"
        _S_PLAYERS[$name]="$players"; _S_FLAGS[$name]="$flags"
    done <"$_SETTINGS_ROWS_FILE"
}
_settings_load || { echo "settings: cannot read $_SETTINGS_ROWS_FILE" >&2; return 1 2>/dev/null || exit 1; }

_settings_has_flag() { [[ ",${_S_FLAGS[$1]}," == *",$2,"* ]]; }

settings_with() {
    local n
    for n in "${SETTINGS[@]}"; do
        case "${1:-}" in
            "") echo "$n" ;;
            ignored-by=*)
                [ "${_S_PLAYERS[$n]}" != all ] && ! [[ ",${_S_PLAYERS[$n]}," == *",${1#ignored-by=},"* ]] && echo "$n" ;;
            *) _settings_has_flag "$n" "$1" && echo "$n" ;;
        esac
    done
    return 0
}

settings_fact() {
    case "$2" in
        default) printf '%s\n' "${_S_DEFAULT[$1]}" ;;
        values)  printf '%s\n' "${_S_VALUES[$1]}" ;;
        players) printf '%s\n' "${_S_PLAYERS[$1]}" ;;
        flags)   printf '%s\n' "${_S_FLAGS[$1]}" ;;
        *) echo "settings_fact NAME default|values|players|flags" >&2; return 2 ;;
    esac
}

# Is VALUE allowed for NAME? Prints what is allowed when not.
_settings_check() {
    local name="$1" v="$2" spec="${_S_VALUES[$1]}"
    [ -z "$v" ] && { _settings_has_flag "$name" empty || [ "$spec" = text ] || [ -z "${_S_DEFAULT[$name]}" ]; } && return 0
    case "$spec" in
        text) return 0 ;;
        size) [[ "$v" =~ ^[0-9]+x[0-9]+$ ]] && return 0; echo "WIDTHxHEIGHT" ;;
        enum:*)
            # Member by member: a substring test would let "mpv,gst" through.
            local m
            local -a members
            IFS=',' read -ra members <<<"${spec#enum:}"
            for m in "${members[@]}"; do [ "$v" = "$m" ] && return 0; done
            echo "one of: ${members[*]}" ;;
        int:*|num:*)
            local lo="${spec#*:}" hi; hi="${lo#*..}"; lo="${lo%..*}"
            local re='^-?[0-9]+$'; [[ "$spec" == num:* ]] && re='^-?[0-9]+(\.[0-9]+)?$'
            if [[ "$v" =~ $re ]] && awk -v v="$v" -v lo="$lo" -v hi="$hi" 'BEGIN { exit !(v >= lo && v <= hi) }'; then
                return 0
            fi
            echo "$([[ "$spec" == int:* ]] && echo "a whole number" || echo "a number") from $lo to $hi" ;;
    esac
    return 1
}

declare -A SETTING_FROM=()
SETTINGS_NOTES=()
settings_resolve() {
    SETTING_FROM=(); SETTINGS_NOTES=()
    local n env_pin="" line rc
    local -A saved=()
    # Environment first. An empty value counts only where empty means
    # something (IPC= disables the socket); elsewhere it is unset.
    for n in "${SETTINGS[@]}"; do
        if [ -n "${!n+x}" ] && { [ -n "${!n}" ] || _settings_has_flag "$n" empty; }; then
            SETTING_FROM[$n]="env"
            _settings_has_flag "$n" pin && [ -n "${!n}" ] && env_pin=1
        else
            unset "$n"
        fi
    done
    if [ -r "$CONFIG_FILE" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            _config_line "$line"; rc=$?
            if [ "$rc" = 2 ]; then
                SETTINGS_NOTES+=("$CONFIG_FILE: ignoring unknown line: $line")
            elif [ "$rc" = 0 ]; then
                saved[$_CK]="$_CV"
            fi
        done <"$CONFIG_FILE"
    fi
    for n in "${SETTINGS[@]}"; do
        [ -n "${SETTING_FROM[$n]:-}" ] && continue
        # The pins are one setting: a card picked in the environment is not
        # overridden by another pinned in the file.
        if [ -n "$env_pin" ] && _settings_has_flag "$n" pin && [ -n "${saved[$n]:-}" ]; then
            SETTINGS_NOTES+=("a capture card is picked in the environment; the saved $n=${saved[$n]} is not used")
        fi
        if [ -n "${saved[$n]+x}" ] && ! { [ -n "$env_pin" ] && _settings_has_flag "$n" pin; } \
           && { [ -n "${saved[$n]}" ] || _settings_has_flag "$n" empty; }; then
            printf -v "$n" '%s' "${saved[$n]}"; SETTING_FROM[$n]=saved
        else
            printf -v "$n" '%s' "${_S_DEFAULT[$n]}"; SETTING_FROM[$n]=default
        fi
    done
    # An old name translates to its replacement unless that was set itself.
    local new from
    for n in "${SETTINGS[@]}"; do
        [[ ",${_S_FLAGS[$n]}," == *",deprecated="* ]] || continue
        [ "${SETTING_FROM[$n]}" = default ] && continue
        new="${_S_FLAGS[$n]#*deprecated=}"; new="${new%%,*}"
        from="${SETTING_FROM[$n]}"
        if [ "${SETTING_FROM[$new]}" != default ]; then
            SETTINGS_NOTES+=("$n=${!n} is ignored because $new=${!new} is set; remove $n")
        else
            case "$n=${!n}" in
                FULLSCREEN=yes) printf -v "$new" fullscreen ;;
                FULLSCREEN=no)  printf -v "$new" windowed ;;
                *) continue ;;   # an invalid value is reported by settings_validate
            esac
            SETTING_FROM[$new]="$from"
            SETTINGS_NOTES+=("$n is deprecated; use $new=${!new}")
        fi
    done
    return 0
}

SETTINGS_ERRORS=()
settings_validate() {
    SETTINGS_ERRORS=()
    local n why src
    for n in "${SETTINGS[@]}"; do
        why=$(_settings_check "$n" "${!n}") && continue
        case "${SETTING_FROM[$n]:-default}" in env) src="environment" ;; saved) src="$CONFIG_FILE" ;; *) src="default" ;; esac
        SETTINGS_ERRORS+=("$n=${!n} ($src) is not $why")
    done
    [ "${#SETTINGS_ERRORS[@]}" = 0 ]
}
