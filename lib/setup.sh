#!/usr/bin/env bash
# protoHDMIcapture setup wizard. Run it as `./view.sh --setup`.
#
# Walks through the capture card, its format, a signal check, audio, display
# mode, refresh matching and the player backend, then writes the choices to
# the config file. Re-running it edits the existing config: every stage starts
# on the current value, so Enter keeps it.
#
# Options (normally passed by view.sh / launch.sh):
#   --from-launcher   started from the desktop menu: no test launch at the end,
#                     the menu launcher starts the feed itself
#
# Takes no video or audio lock; holds setup.lock so only one wizard runs.

set -uo pipefail

HERE=$(dirname "$(dirname "$(readlink -f "$0")")")
# shellcheck source=lib/card.sh
. "$HERE/lib/card.sh"
# shellcheck source=lib/desktop.sh
. "$HERE/lib/desktop.sh"
# shellcheck source=lib/ui.sh
. "$HERE/lib/ui.sh"

FROM_LAUNCHER=""
for arg in "$@"; do
    case "$arg" in
        --from-launcher) FROM_LAUNCHER=1 ;;
        *) echo "setup: unknown option $arg" >&2; exit 2 ;;
    esac
done

RUN_DIR="${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture"
mkdir -p "$RUN_DIR" || { echo "setup: cannot create $RUN_DIR" >&2; exit 1; }
exec {SETUP_LOCK}>"$RUN_DIR/setup.lock"
if ! flock -n "$SETUP_LOCK"; then
    echo "setup: another setup wizard is already running" >&2
    exit 1
fi
echo $$ >"$RUN_DIR/setup.pid"
PREVIEW="$RUN_DIR/setup-preview.config"

cleanup() {
    rm -f "$PREVIEW" "$RUN_DIR/setup.pid"
    ui_restore
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# The keys this wizard decides. Anything else in the file (DELAY, LAT_MS, ...)
# is carried over untouched.
MANAGED=(CARD_SERIAL DEV IGNORE_CARDS_RE FMT RES FPS AUDIO SRC WINDOW WINDOW_SIZE
         REFRESH BACKEND SCALING FULLSCREEN)

# CUR: what the file says now. NEW: what it will say; "" = not in the file.
declare -A CUR NEW
config_read CUR
for k in "${MANAGED[@]}"; do NEW[$k]="${CUR[$k]:-}"; done

# A knob set in the environment beats the file at play time; say so up front.
ENV_OVERRIDES=()
for k in "${CONFIG_KEYS[@]}"; do
    [ -n "${!k+x}" ] && ENV_OVERRIDES+=("$k")
done

# Built-in defaults, as in view.sh, for "current value" when the file is silent.
default_of() {
    case "$1" in
        FMT) echo yuyv422 ;; RES) echo 1920x1080 ;; FPS) echo 60 ;;
        AUDIO) echo loopback ;; WINDOW) echo fullscreen ;; WINDOW_SIZE) echo 1280x720 ;;
        REFRESH) echo keep ;; BACKEND) echo mpv ;; SCALING) echo balanced ;;
        *) echo "" ;;
    esac
}
cur() { local v="${NEW[$1]}"; [ -n "$v" ] && echo "$v" || default_of "$1"; }

# FULLSCREEN is the old spelling of WINDOW; the wizard always writes WINDOW.
if [ -n "${NEW[FULLSCREEN]}" ]; then
    [ -n "${NEW[WINDOW]}" ] || case "${NEW[FULLSCREEN]}" in
        yes) NEW[WINDOW]=fullscreen ;; no) NEW[WINDOW]=windowed ;;
    esac
    NEW[FULLSCREEN]=""
fi

# Apply NEW to a config file: set the non-empty keys, remove the empty ones.
apply_to() {
    local file="$1" k
    local -a set=() unset=()
    for k in "${MANAGED[@]}"; do
        if [ -n "${NEW[$k]}" ]; then set+=("$k=${NEW[$k]}"); else unset+=("$k"); fi
    done
    CONFIG_FILE="$file" config_write "${set[@]}" || return 1
    CONFIG_FILE="$file" config_unset "${unset[@]}"
}

# A feed started from the wizard sees exactly what the saved config will say:
# a preview copy of the file with NEW applied, and none of the managed keys in
# its environment. PROTOHDMI_SETUP_CHILD keeps it from offering setup itself.
run_child() {
    rm -f "$PREVIEW"
    [ -r "$CONFIG_FILE" ] && cp "$CONFIG_FILE" "$PREVIEW"
    apply_to "$PREVIEW" || { ui_bad "could not write a preview config: $CONFIG_ERROR"; return 1; }
    local -a envargs=()
    local k
    for k in "${MANAGED[@]}"; do envargs+=(-u "$k"); done
    env "${envargs[@]}" CONFIG_FILE="$PREVIEW" PROTOHDMI_SETUP_CHILD=1 "$@"
}

# --- stage list --------------------------------------------------------------
STAGES=(welcome card format signal audio display)
desktop_can refresh_match && STAGES+=(refresh)
STAGES+=(backend review)
[ -z "$FROM_LAUNCHER" ] && STAGES+=(test)
TOTAL_STAGES=$(( ${#STAGES[@]} - 1 ))   # the welcome screen is not numbered

# --- stages ------------------------------------------------------------------
stage_welcome() {
    _clear
    center ""
    center "${BOLD}${BLUE}📺  protoHDMIcapture setup${RESET}"
    center "${DIM}low-latency HDMI capture viewer · re-run any time with ./view.sh --setup${RESET}"
    center ""
    say "Picks your capture card and its best mode, checks the signal and the"
    say "audio, and sets how the picture is shown. Nothing is written until the"
    say "review at the end."
    center ""
    note "Before you start: the HDMI source is on and outputting, HDCP is off on"
    note "the source (consoles ship with it on), and the card sits on a USB 3 port."
    center ""
    if [ -r "$CONFIG_FILE" ]; then
        say "Editing ${BOLD}$CONFIG_FILE${RESET}"
        note "Every question starts on its current value; Enter keeps it."
    else
        say "No settings yet; they will be written to ${BOLD}$CONFIG_FILE${RESET}"
    fi
    if [ "${#ENV_OVERRIDES[@]}" -gt 0 ]; then
        center ""
        ui_warn "Set in your environment, and winning over the file: ${ENV_OVERRIDES[*]}"
    fi
    center ""
    pause
}

stage_card() {
    stage "🎥 Capture card"
    local -a nodes=() serials=() types=() items=()
    local n serial type
    while IFS=$'\t' read -r n serial type; do
        nodes+=("$n"); serials+=("$serial"); types+=("$type")
    done < <(card_list)
    if [ "${#nodes[@]}" -eq 0 ]; then
        ui_bad "No video capture device found."
        note "Plug the card in (USB 3 port), then press Enter to look again."
        pause
        _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_card; return
    fi

    local def="" i
    for i in "${!nodes[@]}"; do
        local label="${types[$i]}  ${DIM}${nodes[$i]}${serials[$i]:+ · serial ${serials[$i]}}${RESET}"
        items+=("$i|$label")
        if [ -n "${serials[$i]}" ] && [ "${serials[$i]}" = "${CUR[CARD_SERIAL]:-}" ]; then def=$i; fi
        [ -z "$def" ] && [ "${nodes[$i]}" = "${CUR[DEV]:-}" ] && def=$i
    done
    say "Capture devices found (cameras show up here too):"
    center ""
    local pick
    if [ "${#nodes[@]}" -eq 1 ]; then
        center "${items[0]#*|}"
        center ""
        confirm_yes "Use this one?" || { say "Nothing else to pick; plug the card in and re-run setup."; exit 1; }
        pick=0
    else
        choose pick "Which one is the HDMI capture card?" "$def" "${items[@]}" || exit 1
    fi
    CARD_DEV="${nodes[$pick]}"
    CARD_TYPE="${types[$pick]}"
    if [ -n "${serials[$pick]}" ]; then
        NEW[CARD_SERIAL]="${serials[$pick]}"
        NEW[DEV]=""
        center ""
        ui_ok "Pinned by USB serial ${serials[$pick]}: found wherever it enumerates."
    else
        NEW[CARD_SERIAL]=""
        NEW[DEV]="$CARD_DEV"
        center ""
        ui_warn "This card reports no serial, so it is pinned by node ($CARD_DEV)."
        note "Node numbers can change when devices are replugged; re-run setup if it moves."
    fi
    center ""
    pause
}

# "fmt WxH fps" rows for the card's modes, best first: highest fps, then the
# largest picture, then uncompressed over compressed.
list_modes() {
    v4l2-ctl -d "$CARD_DEV" --list-formats-ext 2>/dev/null | awk '
        /\[[0-9]+\]: \047/ { split($0, a, "\047"); cc = a[2] }
        /Size: Discrete/  { size = $3 }
        /Interval: Discrete/ {
            fps = $4; gsub(/[(]/, "", fps); fps = int(fps + 0.5)
            fmt = (cc == "YUYV") ? "yuyv422" : (cc == "MJPG") ? "mjpeg" : (cc == "NV12") ? "nv12" : ""
            if (fmt == "") next
            split(size, wh, "x")
            raw = (fmt == "mjpeg") ? 0 : 1
            printf "%d\t%d\t%d\t%s\t%s\t%d\n", fps, wh[1] * wh[2], raw, fmt, size, fps
        }' | sort -t$'\t' -k1,1nr -k2,2nr -k3,3nr -u | cut -f4- | head -12
}

stage_format() {
    stage "📐 Picture format"
    local -a rows=()
    mapfile -t rows < <(list_modes)
    if [ "${#rows[@]}" -eq 0 ]; then
        ui_warn "The card lists no modes it can deliver."
        note "Some cards only list modes while a source is locked. Is the source on?"
        center ""
        if confirm_yes "Look again?"; then _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_format; return; fi
        ask RES_IN "Resolution" "$(cur RES)" '^[0-9]+x[0-9]+$'
        ask FPS_IN "Frames per second" "$(cur FPS)" '^[0-9]+$'
        ask FMT_IN "Format (yuyv422 | mjpeg | nv12)" "$(cur FMT)" '^(yuyv422|mjpeg|nv12)$'
        NEW[RES]="$RES_IN"; NEW[FPS]="$FPS_IN"; NEW[FMT]="$FMT_IN"
        return
    fi
    local -a items=()
    local r fmt size fps desc cur_key i=0
    cur_key="$(cur FMT) $(cur RES) $(cur FPS)"
    for r in "${rows[@]}"; do
        IFS=$'\t' read -r fmt size fps <<<"$r"
        case "$fmt" in
            yuyv422) desc="uncompressed YUYV" ;;
            mjpeg)   desc="MJPEG (compressed, needs decoding)" ;;
            *)       desc="$fmt" ;;
        esac
        local tag=""
        [ "$i" = 0 ] && tag="recommended"
        items+=("$fmt $size $fps|${size} @ ${fps} fps · ${desc}${tag:+|$tag}")
        i=$((i + 1))
    done
    say "Modes $CARD_TYPE offers, best first:"
    note "Uncompressed skips a decode step; it needs the USB 3 bandwidth."
    center ""
    local pick
    choose pick "Mode" "$cur_key" "${items[@]}" || exit 1
    local f r p
    read -r f r p <<<"$pick"
    NEW[FMT]="$f"; NEW[RES]="$r"; NEW[FPS]="$p"
}

stage_signal() {
    stage "📡 Signal check"
    local fourcc w h
    case "${NEW[FMT]}" in yuyv422) fourcc=YUYV ;; mjpeg) fourcc=MJPG ;; nv12) fourcc=NV12 ;; *) fourcc="" ;; esac
    w="${NEW[RES]%x*}"; h="${NEW[RES]#*x}"
    say "Capturing 2 seconds at ${BOLD}${NEW[RES]} @ ${NEW[FPS]} fps (${NEW[FMT]})${RESET} to see"
    say "whether frames really arrive at that rate."
    center ""
    local out frames secs t0 t1 rc
    out=$(v4l2-ctl -d "$CARD_DEV" ${fourcc:+--set-fmt-video=width=$w,height=$h,pixelformat=$fourcc} \
                   --set-parm="${NEW[FPS]}" 2>&1)
    if grep -qi 'busy' <<<"$out"; then
        ui_bad "The card is busy: a feed is probably running. Close it first."
        center ""
        if confirm_yes "Try again?"; then _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_signal; fi
        return
    fi
    frames=$(( NEW[FPS] * 2 ))
    t0=$(date +%s.%N)
    timeout 6 v4l2-ctl -d "$CARD_DEV" --stream-mmap --stream-count="$frames" --stream-to=/dev/null >/dev/null 2>&1
    rc=$?
    t1=$(date +%s.%N)
    secs=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }')
    local got
    got=$(v4l2-ctl -d "$CARD_DEV" --get-fmt-video 2>/dev/null | sed -n 's/.*Width\/Height *: *//p')
    if [ "$rc" = 124 ]; then
        SIGNAL_RESULT="no frames in 6 s"
        ui_bad "No frames in 6 seconds: no signal, or HDCP is on at the source."
        note "Check the source is on and outputting, and HDCP is off, then try again."
    else
        MEASURED_FPS=$(awk -v f="$frames" -v s="$secs" 'BEGIN { printf "%.1f", f / s }')
        SIGNAL_RESULT="$MEASURED_FPS fps measured"
        if awk -v m="$MEASURED_FPS" -v t="${NEW[FPS]}" 'BEGIN { exit !(m >= t * 0.8) }'; then
            ui_ok "$MEASURED_FPS fps measured (${got:-?})"
        else
            ui_warn "Only $MEASURED_FPS fps of ${NEW[FPS]} (${got:-?}). The first frames include start-up;"
            note "a much lower number means the mode is not what the card really delivers."
        fi
    fi
    center ""
    if [ "$rc" = 124 ] && confirm_yes "Try again?"; then _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_signal; return; fi
    pause
}

stage_audio() {
    stage "🔊 Audio"
    local DEV="$CARD_DEV" SRC=""
    local matched=""
    resolve_audio_source && matched="$SRC"
    local -a items=()
    if [ -n "$matched" ]; then
        say "The card's own audio input was found by its serial:"
        note "$matched"
        items+=("auto|Use the card's audio (found automatically each start)|recommended")
    else
        ui_warn "No audio source matched the card's serial."
    fi
    local s
    while read -r s; do
        [ -n "$s" ] && [ "$s" != "$matched" ] && items+=("src:$s|$s")
    done < <(pactl list short sources 2>/dev/null | awk '$2 !~ /\.monitor$/ {print $2}')
    items+=("none|No audio (video only)")
    local def
    if [ "${NEW[AUDIO]:-}" = none ]; then def=none
    elif [ -n "${NEW[SRC]}" ]; then def="src:${NEW[SRC]}"
    else def=auto
    fi
    center ""
    local pick
    choose pick "Audio" "$def" "${items[@]}" || exit 1
    case "$pick" in
        auto)  NEW[AUDIO]=""; NEW[SRC]="" ;;
        none)  NEW[AUDIO]=none; NEW[SRC]="" ;;
        src:*) NEW[AUDIO]=""; NEW[SRC]="${pick#src:}" ;;
    esac
}

stage_display() {
    stage "🖥️  Display mode"
    say "How the picture is shown, as in a game's display settings:"
    center ""
    local pick
    choose pick "Display mode" "$(cur WINDOW)" \
        "fullscreen|Fullscreen · lowest latency (direct scanout)|recommended" \
        "borderless|Borderless · maximized, no border, the bar stays visible" \
        "windowed|Windowed · a normal window you can move and resize" || exit 1
    NEW[WINDOW]="$pick"
    if [ "$pick" = windowed ]; then
        center ""
        local size cs
        cs="$(cur WINDOW_SIZE)"
        local -a sizes=("1280x720|1280 × 720" "1600x900|1600 × 900" "1920x1080|1920 × 1080")
        [[ " 1280x720 1600x900 1920x1080 " == *" $cs "* ]] || sizes+=("$cs|$cs")
        sizes+=("custom|Custom size…")
        choose size "Window size" "$cs" "${sizes[@]}" || exit 1
        [ "$size" = custom ] && ask size "Window size (WIDTHxHEIGHT)" "$cs" '^[0-9]+x[0-9]+$'
        NEW[WINDOW_SIZE]="$size"
    else
        NEW[WINDOW_SIZE]=""
    fi
}

stage_refresh() {
    stage "⏱️  Refresh rate"
    local mon
    mon=$(desktop_outputs | awk '/ focused$/ { print $1 " at " $2 }')
    say "A ${NEW[FPS]} fps feed on a display that refreshes at a different rate judders:"
    say "frames are held for an uneven number of refreshes. While the feed runs,"
    say "the monitor it opens on can switch to its mode nearest ${NEW[FPS]} Hz."
    [ -n "$mon" ] && { center ""; note "Focused monitor now: $mon"; }
    center ""
    local pick
    choose pick "Refresh" "$(cur REFRESH)" \
        "match|Match the feed while it runs, restore afterwards|recommended" \
        "keep|Leave the display alone" || exit 1
    NEW[REFRESH]="$pick"
}

stage_backend() {
    stage "🎬 Player"
    local -a items=("mpv|mpv · every option, the default|recommended")
    [ -n "${WAYLAND_DISPLAY:-}" ] && items+=("gst|GStreamer · fewest stages, Wayland only, fewer options")
    items+=("ffplay|ffplay · reference player, for comparison")
    local pick
    choose pick "Player" "$(cur BACKEND)" "${items[@]}" || exit 1
    NEW[BACKEND]="$pick"
    if [ "$pick" = mpv ]; then
        center ""
        choose pick "Scaling" "$(cur SCALING)" \
            "balanced|Balanced · sharp upscale, low GPU cost|recommended" \
            "fast|Fast · bilinear, lowest GPU time, softer" \
            "config|Your mpv.conf · whatever it sets (may be expensive)" || exit 1
        NEW[SCALING]="$pick"
    else
        NEW[SCALING]=""
    fi
}

CHANGED=0
stage_review() {
    stage "📋 Review"
    local k old new
    CHANGED=0
    for k in "${MANAGED[@]}"; do
        old="${CUR[$k]:-}"; new="${NEW[$k]}"
        [ "$old" = "$new" ] && continue
        CHANGED=$((CHANGED + 1))
        if [ -z "$old" ]; then
            center "${GREEN}+ $k=$new${RESET}"
        elif [ -z "$new" ]; then
            center "${RED}- $k=$old${RESET}"
        else
            center "${YELLOW}~ $k: $old → $new${RESET}"
        fi
    done
    if [ "$CHANGED" = 0 ]; then
        ui_ok "No changes: $CONFIG_FILE already says all of this."
        center ""
        pause
        return
    fi
    center ""
    if confirm_yes "Write these to $CONFIG_FILE?"; then
        apply_to "$CONFIG_FILE" || { ui_bad "Could not write: $CONFIG_ERROR"; exit 1; }
        ui_ok "Saved."
        SAVED=1
    else
        note "Nothing written."
    fi
    center ""
    pause
}

stage_test() {
    stage "🚀 Try it"
    say "Starts the feed with these settings. Close the window (or press q in it)"
    say "to come back here."
    center ""
    confirm_yes "Start a test feed now?" || return
    run_child "$HERE/view.sh" >"$RUN_DIR/setup-test.log" 2>&1
    local rc=$?
    center ""
    if [ "$rc" = 0 ]; then
        ui_ok "Feed closed."
    else
        ui_bad "The feed exited with status $rc. Last lines of its log:"
        tail -5 "$RUN_DIR/setup-test.log" | sed 's/\x1b\[[0-9;]*m//g' | while IFS= read -r l; do note "$l"; done
    fi
    center ""
    pause
}

# --- run ---------------------------------------------------------------------
SAVED=""
MEASURED_FPS=""
SIGNAL_RESULT=""
CARD_DEV=""
CARD_TYPE=""
for s in "${STAGES[@]}"; do "stage_$s"; done

_clear
center ""
if [ -n "$SAVED" ]; then
    center "${BOLD}${GREEN}✅ Setup complete${RESET}"
elif [ "$CHANGED" = 0 ]; then
    center "${BOLD}${GREEN}✅ Nothing to change${RESET}"
else
    center "${BOLD}${YELLOW}Setup closed without saving${RESET}"
fi
center ""
say "Card    : $CARD_TYPE (${CARD_DEV})"
say "Mode    : ${NEW[RES]} @ ${NEW[FPS]} fps, ${NEW[FMT]}${SIGNAL_RESULT:+ · $SIGNAL_RESULT}"
say "Display : $(cur WINDOW)$([ "$(cur WINDOW)" = windowed ] && echo " $(cur WINDOW_SIZE)") · player $(cur BACKEND)"
center ""
note "Play: ./view.sh, or protoHDMIcapture in your app launcher."
note "Change any of this: ./view.sh --setup"
center ""
