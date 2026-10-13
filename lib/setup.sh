#!/usr/bin/env bash
# protoHDMIcapture setup wizard. Run it as `./view.sh --setup`.
#
# Walks through the capture card, its USB link, its format, a signal check,
# audio, display mode, refresh matching and the player backend, then writes
# the choices to the config file and a Markdown diagnostics report. Re-running it edits the existing config: every stage starts
# on the current value, so Enter keeps it.
#
# Options (normally passed by view.sh / launch.sh):
#   --from-launcher   started in a terminal opened by the menu launcher, which
#                     starts the feed itself once this exits: no test launch,
#                     and a last "press Enter" so the result can be read
#   --then-play       started by view.sh on first run, which plays right after:
#                     no test launch
#
# Takes no video or audio lock; holds setup.lock so only one wizard runs.
# On exit it leaves $RUN_DIR/setup.done saying "saved" or "cancelled", which
# is how a waiting launcher knows whether to start the feed.

set -uo pipefail

HERE=$(dirname "$(dirname "$(readlink -f "$0")")")
# shellcheck source=lib/card.sh
. "$HERE/lib/card.sh"
# shellcheck source=lib/desktop.sh
. "$HERE/lib/desktop.sh"
# shellcheck source=lib/ui.sh
. "$HERE/lib/ui.sh"
# shellcheck source=lib/capture-cards.sh
. "$HERE/lib/capture-cards.sh"
# shellcheck source=lib/sources.sh
. "$HERE/lib/sources.sh"
# shellcheck source=lib/preflight.sh
. "$HERE/lib/preflight.sh"

FROM_LAUNCHER=""; THEN_PLAY=""
for arg in "$@"; do
    case "$arg" in
        --from-launcher) FROM_LAUNCHER=1 ;;
        --then-play) THEN_PLAY=1 ;;
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

SAVED=""
SYNC_SHELL=""
cleanup() {
    sync_stop
    if [ -n "$SAVED" ]; then echo saved; else echo cancelled; fi >"$RUN_DIR/setup.done"
    rm -f "$PREVIEW" "$RUN_DIR/setup.pid"
    ui_restore
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# The settings this wizard decides (flag wizard in lib/settings.sh). Anything
# else in the file (LAT_MS, LEVELS, ...) is carried over untouched.
mapfile -t MANAGED < <(settings_with wizard)

# CUR: what the file says now. NEW: what it will say; "" = not in the file.
declare -A CUR NEW
config_read CUR
for k in "${MANAGED[@]}"; do NEW[$k]="${CUR[$k]:-}"; done

# A setting in the environment beats the file at play time; say so up front.
ENV_OVERRIDES=()
for k in "${CONFIG_KEYS[@]}"; do
    [ -n "${!k+x}" ] && ENV_OVERRIDES+=("$k")
done

# The current value: what the file will say, else the setting's default.
cur() { local v="${NEW[$1]}"; [ -n "$v" ] && echo "$v" || settings_fact "$1" default; }

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
# run_child [--exec] CMD...: --exec replaces the calling (sub)shell with CMD,
# so a backgrounded child's $! is the child itself and can be stopped by PID.
run_child() {
    local ex=""
    [ "${1:-}" = --exec ] && { ex="exec"; shift; }
    rm -f "$PREVIEW"
    [ -r "$CONFIG_FILE" ] && cp "$CONFIG_FILE" "$PREVIEW"
    apply_to "$PREVIEW" || { ui_bad "could not write a preview config: $CONFIG_ERROR"; return 1; }
    local -a envargs=()
    local k
    for k in "${MANAGED[@]}"; do envargs+=(-u "$k"); done
    $ex env "${envargs[@]}" CONFIG_FILE="$PREVIEW" PROTOHDMI_SETUP_CHILD=1 "$@"
}

# --- stage list --------------------------------------------------------------
STAGES=(welcome card usb source format signal audio display)
desktop_can refresh_match && STAGES+=(refresh)
STAGES+=(backend sync debug review)
[ -z "$FROM_LAUNCHER$THEN_PLAY" ] && STAGES+=(test)
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
    # What the feed it sets up will need (mpv, the loopback), plus ffmpeg
    # for the signal check.
    if ! preflight_check mpv loopback match off ffmpeg; then
        center ""
        ui_bad "Missing, needed before setup can go on:"
        ui_list "${PREFLIGHT_MISSING[@]}"
        center ""
        say "Install them, then run ./view.sh --setup again."
        exit 1
    fi
    if [ "${#PREFLIGHT_OFF[@]}" -gt 0 ]; then
        center ""
        ui_list "${PREFLIGHT_OFF[@]}"
    fi
    center ""
    pause
}

# Which chip a USB id means comes from the capture card profiles in
# capture-cards/ (lib/capture-cards.sh). Cheap boxes borrow ids, so the
# format list, not the model number, has the last word.

# vid:pid of the USB device behind a video node, or nothing.
_usb_id() {
    local d
    d=$(card_usb_dir "$1")
    [ -n "$d" ] && echo "$(cat "$d/idVendor"):$(cat "$d/idProduct")"
}

stage_usb() {
    stage "🔌 USB link"
    # The card may report no product string, so nothing here depends on one.
    USB_DIR=$(card_usb_dir "$CARD_DEV")
    if [ -z "$USB_DIR" ]; then
        USB_RESULT="not a USB device"
        ui_warn "$CARD_DEV is not on USB; nothing to check here."
        center ""; pause; return
    fi
    VIDPID="$(cat "$USB_DIR/idVendor"):$(cat "$USB_DIR/idProduct")"
    LINK_SPEED=$(cat "$USB_DIR/speed" 2>/dev/null)
    capture_card_lookup "$VIDPID"
    CHIP="$CC_CHIP"
    SUPPORT=""
    if [ -n "$CC_STATUS" ]; then
        local level date rest
        read -r level date rest <<<"$CC_STATUS"
        [ "$date" = - ] && date=""
        SUPPORT="$level${date:+ $date}${rest:+ · $rest}"
    fi
    say "USB id  : ${BOLD}$VIDPID${RESET} · $CHIP"
    say "Port    : $(basename "$USB_DIR") · USB $(tr -d ' ' <"$USB_DIR/version" 2>/dev/null)"
    [ -n "$SUPPORT" ] && say "Support : ${SUPPORT^}"
    note "$CC_HINT"
    if [ "${#CC_QUIRKS[@]}" -gt 0 ]; then
        center ""
        say "Known about this card (${CC_FILE#"$HERE/"}):"
        ui_list "${CC_QUIRKS[@]//\`/}"   # Markdown code ticks read as noise here
    fi
    center ""
    # A USB 2 chip on a USB 2 link is as good as it gets, not a fault.
    case "$LINK_SPEED:$CC_LINK" in
        5000:*|10000:*|20000:*)
            USB_RESULT="ok"
            ui_ok "Link: $LINK_SPEED Mbit/s, enough for uncompressed 1080p60" ;;
        480:480)
            USB_RESULT="ok"
            ui_ok "Link: 480 Mbit/s (USB 2), all this chip uses" ;;
        480:*)
            USB_RESULT="USB 2 link"
            ui_bad "Link: 480 Mbit/s, a USB 2 port or hub"
            note "Uncompressed 1080p60 needs USB 3: move the card to a rear USB 3 port." ;;
        *)
            USB_RESULT="speed ${LINK_SPEED:-unknown}"
            ui_warn "Link: ${LINK_SPEED:-unknown} Mbit/s" ;;
    esac
    center ""
    pause
}

# A device name as a regex matching only itself, for IGNORE_CARDS_RE. A
# single quote becomes "." (any character): the config cannot hold one.
_regex_literal() { printf '%s' "$1" | sed "s/[][\\.*^\$+?(){}|]/\\\\&/g; s/'/./g"; }

# PATTERN without its alternatives that match NAME. Splits on the "|" between
# alternatives, not on an escaped one inside a name; a hand-written group
# like (a|b) is split too, which only matters if it matches the card.
_ignore_without() {
    local p="${1//\\|/$'\x01'}" out="" a
    local -a parts
    IFS='|' read -ra parts <<<"$p"
    for a in "${parts[@]}"; do
        a="${a//$'\x01'/\\|}"
        grep -Eq -- "$a" <<<"$2" && continue
        out+="${out:+|}$a"
    done
    printf '%s' "$out"
}

stage_card() {
    stage "🎥 Capture card"
    local -a nodes=() serials=() ports=() types=() items=()
    local n serial port type
    while IFS='|' read -r n serial port type; do
        nodes+=("$n"); serials+=("$serial"); ports+=("$port"); types+=("$type")
    done < <(card_list)
    if [ "${#nodes[@]}" -eq 0 ]; then
        ui_bad "No video capture device found."
        note "Plug the card in (USB 3 port), then press Enter to look again."
        pause
        _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_card; return
    fi

    local def="" i
    for i in "${!nodes[@]}"; do
        local id="${serials[$i]:+serial ${serials[$i]}}"
        [ -n "$id" ] || id="${ports[$i]:+port ${ports[$i]}}"
        local label="${types[$i]}  ${DIM}${nodes[$i]}${id:+ · $id}${RESET}"
        if IGNORE_CARDS_RE="${NEW[IGNORE_CARDS_RE]}" card_ignored "${types[$i]}"; then
            items+=("$i|$label|ignored")
        else
            items+=("$i|$label")
        fi
        if [ -n "${serials[$i]}" ] && [ "${serials[$i]}" = "${CUR[CARD_SERIAL]:-}" ]; then def=$i; fi
        if [ -z "$def" ] && [ -n "${ports[$i]}" ] && [ "${ports[$i]}" = "${CUR[CARD_PORT]:-}" ]; then def=$i; fi
        [ -z "$def" ] && [ "${nodes[$i]}" = "${CUR[DEV]:-}" ] && def=$i
    done
    # Nothing chosen before: recommend the first device whose USB id is a
    # capture chip this wizard knows, rather than whichever camera is first.
    local rec=""
    for i in "${!nodes[@]}"; do
        local vp
        vp=$(_usb_id "${nodes[$i]}")
        capture_card_lookup "$vp"; [ $? -le 1 ] && { rec=$i; break; }
    done
    if [ -n "$rec" ]; then
        items[rec]="${items[rec]}|capture chip"
        [ -n "$def" ] || def=$rec
    fi
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
        NEW[CARD_PORT]=""
        NEW[DEV]=""
        center ""
        ui_ok "Pinned by USB serial ${serials[$pick]}: found wherever it enumerates."
    elif [ -n "${ports[$pick]}" ]; then
        NEW[CARD_SERIAL]=""
        NEW[CARD_PORT]="${ports[$pick]}"
        NEW[DEV]=""
        center ""
        ui_ok "This card reports no serial, so it is pinned by its USB port."
        note "Found again after a replug into the same port; re-run setup if you move it."
    else
        NEW[CARD_SERIAL]=""
        NEW[CARD_PORT]=""
        NEW[DEV]="$CARD_DEV"
        center ""
        ui_warn "This card reports no serial, so it is pinned by node ($CARD_DEV)."
        note "Node numbers can change when devices are replugged; re-run setup if it moves."
    fi
    ignore_others "$pick" nodes types
    center ""
    pause
}

# Ignored devices (IGNORE_CARDS_RE): the other video devices, offered one by
# one so no scan, and no DEV left over from a replug, opens them as the
# capture card. A name that would also match the chosen card is refused, and
# a chosen card the list already matched is taken off it.
ignore_others() {
    local pick="$1" i lit vp
    local -n _nodes="$2" _types="$3"
    local pat="${NEW[IGNORE_CARDS_RE]}"
    if IGNORE_CARDS_RE="$pat" card_ignored "$CARD_TYPE"; then
        pat=$(_ignore_without "$pat" "$CARD_TYPE")
        center ""
        ui_warn "This card was on the ignored list; it is taken off."
    fi
    for i in "${!_nodes[@]}"; do
        [ "$i" = "$pick" ] && continue
        IGNORE_CARDS_RE="$pat" card_ignored "${_types[$i]}" && continue
        lit=$(_regex_literal "${_types[$i]}")
        center ""
        if grep -Eq -- "$lit" <<<"$CARD_TYPE"; then
            note "${_types[$i]} (${_nodes[$i]}) has the capture card's name, so it cannot be ignored by name."
            continue
        fi
        vp=$(_usb_id "${_nodes[$i]}")
        capture_card_lookup "$vp"
        if [ $? -le 1 ]; then
            say "${_types[$i]} (${_nodes[$i]}) is a capture card too."
            confirm "Ignore it from now on?" || continue
        else
            say "${_types[$i]} (${_nodes[$i]}) is not the capture card."
            confirm_yes "Ignore it from now on (a camera)?" || continue
        fi
        pat+="${pat:+|}$lit"
    done
    NEW[IGNORE_CARDS_RE]="$pat"
}

stage_source() {
    stage "🎮 Source"
    say "What is plugged into the capture card's HDMI input?"
    center ""
    local -a items=()
    local id
    while read -r id; do
        if [ "$id" = generic ]; then
            items+=("generic|Something else · the checklist for any HDMI source")
        else
            local level date
            read -r level date <<<"$(source_status "$id")"
            local name
            name=$(source_name "$id")
            # "|" separates choose's fields; a name must not carry one.
            items+=("$id|${name//|//}${level:+|$level}")
        fi
    done < <(source_ids)
    local def="${NEW[SOURCE]:-generic}"
    [ -e "$SOURCES_DIR/$def.md" ] || def=generic
    local pick
    choose pick "Source" "$def" "${items[@]}" || exit 1
    if [ "$pick" = generic ]; then NEW[SOURCE]=""; else NEW[SOURCE]="$pick"; fi
    center ""
    say "Set these on the $(source_name "$pick") now; the signal check comes next:"
    local -a tips=()
    mapfile -t tips < <(source_tips "$pick")
    ui_list "${tips[@]}"
    center ""
    pause
}

# _closest_mode RES FPS ROW...: the row ("fmt|WxH|fps") nearest the source's
# suggested RES and FPS, frame rate first (a game at 30 fps feels worse than
# one at 720p), then picture size; ties keep list order, which is best first.
_closest_mode() {
    local sres="$1" sfps="$2"; shift 2
    local sa=$(( ${sres%x*} * ${sres#*x} ))
    printf '%s\n' "$@" | awk -F'|' -v sa="$sa" -v sf="$sfps" '
        function abs(x) { return x < 0 ? -x : x }
        { split($2, wh, "x"); df = abs($3 - sf); da = abs(wh[1] * wh[2] - sa)
          if (NR == 1 || df < bf || (df == bf && da < ba)) { best = $0; bf = df; ba = da } }
        END { print best }'
}

# "fmt|WxH|fps" rows for the card's modes, best first: highest fps, then the
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
            printf "%d|%d|%d|%s|%s|%d\n", fps, wh[1] * wh[2], raw, fmt, size, fps
        }' | sort -t'|' -k1,1nr -k2,2nr -k3,3nr -u | cut -d'|' -f4- | head -12
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
    # The source's suggestion, if it has one, picks the starting row; the
    # card's own list always decides what can be chosen.
    local sname="" sres="" sfps="" kv near="" near_key=""
    while read -r kv; do
        case "$kv" in RES=*) sres="${kv#RES=}" ;; FPS=*) sfps="${kv#FPS=}" ;; esac
    done < <(source_suggest "${NEW[SOURCE]}")
    # A malformed suggestion (a typo in a profile) is ignored, not an
    # arithmetic error in the wizard.
    [[ "$sres" =~ ^[0-9]+x[0-9]+$ ]] || sres=""
    [[ "$sfps" =~ ^[0-9]+$ ]] || sfps=""
    if [ -n "$sres" ] && [ -n "$sfps" ]; then
        sname=$(source_name "${NEW[SOURCE]}")
        near=$(_closest_mode "$sres" "$sfps" "${rows[@]}")
        near_key="${near//|/ }"
    fi
    local -a items=()
    local r fmt size fps desc cur_key def_key i=0 have_cur=""
    cur_key="$(cur FMT) $(cur RES) $(cur FPS)"
    for r in "${rows[@]}"; do
        IFS='|' read -r fmt size fps <<<"$r"
        case "$fmt" in
            yuyv422) desc="uncompressed YUYV" ;;
            mjpeg)   desc="MJPEG (compressed, needs decoding)" ;;
            *)       desc="$fmt" ;;
        esac
        local tag=""
        [ "$i" = 0 ] && tag="recommended"
        if [ "$fmt $size $fps" = "$near_key" ]; then
            local why="closest to $sname's suggestion"
            [ "$size" = "$sres" ] && [ "$fps" = "$sfps" ] && why="$sname suggests this"
            tag="${tag:+$tag · }$why"
        fi
        items+=("$fmt $size $fps|${size} @ ${fps} fps · ${desc}${tag:+|$tag}")
        [ "$fmt $size $fps" = "$cur_key" ] && have_cur=1
        i=$((i + 1))
    done
    # Start on the saved mode if this card offers it, else on the source's
    # nearest, else on the best.
    local def_label="current"
    if [ -n "$have_cur" ]; then def_key="$cur_key"
    elif [ -n "$near_key" ]; then def_key="$near_key"; def_label="suggested"
    else def_key="${rows[0]//|/ }"; def_label="best"
    fi
    say "Modes $CARD_TYPE offers, best first:"
    note "Uncompressed skips a decode step; it needs the USB 3 bandwidth."
    if [ -n "$near" ]; then
        local nsize="${near#*|}"; nsize="${nsize%|*}"
        local nfps="${near##*|}"
        if [ "$nsize" != "$sres" ] || [ "$nfps" != "$sfps" ]; then
            center ""
            ui_warn "$sname suggests $sres @ $sfps; this card can't capture that."
            note "Closest at the best frame rate: $nsize @ $nfps."
            [ "$nsize" != "$sres" ] && note "Tip: set the $sname's output resolution to ${nsize#*x}p, so the card is not scaling."
        fi
    fi
    center ""
    local pick
    CHOOSE_DEFAULT_LABEL="$def_label" choose pick "Mode" "$def_key" "${items[@]}" || exit 1
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
    local out frames secs t0 t1 rc failed=""
    PICTURE=""
    # Busy, unplugged or refused mode: report it, never measure past it.
    out=$(v4l2-ctl -d "$CARD_DEV" ${fourcc:+--set-fmt-video=width=$w,height=$h,pixelformat=$fourcc} \
                   --set-parm="${NEW[FPS]}" 2>&1)
    rc=$?
    if grep -qi 'busy' <<<"$out"; then
        failed="the card is busy: a feed is probably running. Close it first."
    elif [ "$rc" != 0 ]; then
        failed="could not set the mode (v4l2-ctl exit $rc): $(tail -1 <<<"$out")"
    else
        frames=$(( NEW[FPS] * 2 ))
        t0=$(date +%s.%N)
        out=$(timeout 6 v4l2-ctl -d "$CARD_DEV" --stream-mmap --stream-count="$frames" --stream-to=/dev/null 2>&1 >/dev/null)
        rc=$?
        t1=$(date +%s.%N)
        if [ "$rc" = 124 ]; then
            failed="no frames in 6 seconds: no signal, or HDCP is on at the source."
        elif [ "$rc" != 0 ]; then
            failed="capture failed (v4l2-ctl exit $rc): $(tail -1 <<<"${out:-no error text}")"
        fi
    fi
    if [ -n "$failed" ]; then
        SIGNAL_RESULT="failed: $failed"
        MEASURED_FPS=""
        ui_bad "${failed^}"
        note "Check the source is on and outputting, HDCP is off, and the card is plugged in."
        center ""
        if confirm_yes "Try again?"; then _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_signal; return; fi
        pause
        return
    fi
    # Only a clean capture of all the frames gets measured.
    secs=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", b - a }')
    local got
    got=$(v4l2-ctl -d "$CARD_DEV" --get-fmt-video 2>/dev/null | sed -n 's/.*Width\/Height *: *//p')
    MEASURED_FPS=$(awk -v f="$frames" -v s="$secs" 'BEGIN { printf "%.1f", f / s }')
    SIGNAL_RESULT="$MEASURED_FPS fps measured"
    if awk -v m="$MEASURED_FPS" -v t="${NEW[FPS]}" 'BEGIN { exit !(m >= t * 0.8 && m <= t * 1.2) }'; then
        ui_ok "$MEASURED_FPS fps measured (${got:-?})"
    else
        ui_warn "$MEASURED_FPS fps measured for ${NEW[FPS]} requested (${got:-?})."
        note "Start-up costs a little; a number far off means the mode is not what the card delivers."
    fi
    # Some cards (the MS2109) stream black frames at full rate with nothing
    # plugged in, so frames alone do not prove a source.
    picture_check
    case "$PICTURE" in
        black)
            SIGNAL_RESULT="$MEASURED_FPS fps measured, but the picture is uniformly black"
            ui_warn "The picture is uniformly black."
            note "No source, the source is asleep, or HDCP is on. Some cards stream black when nothing is plugged in."
            center ""
            if confirm_yes "Try again?"; then _STAGE_INDEX=$((_STAGE_INDEX - 1)); stage_signal; return; fi
            ;;
        ok) ;;
        *)
            SIGNAL_RESULT="$SIGNAL_RESULT; picture not checked ($PICTURE)"
            ui_warn "Could not check the picture: $PICTURE" ;;
    esac
    center ""
    pause
}

# PICTURE: "ok", "black" (flat and dark across a few frames), or why it could
# not be checked. Uses ffmpeg's signalstats on frames at the chosen mode.
picture_check() {
    if ! command -v ffmpeg >/dev/null; then PICTURE="ffmpeg is not installed"; return; fi
    local out rc
    # -k: ffmpeg blocked in a V4L2 read can ignore SIGTERM (seen on the MS2130
    # with no source); the KILL makes the 6 seconds a real bound.
    out=$(timeout -k 2 6 ffmpeg -hide_banner -nostdin -f v4l2 -input_format "${NEW[FMT]}" \
            -video_size "${NEW[RES]}" -framerate "${NEW[FPS]}" -i "$CARD_DEV" \
            -frames:v 5 -vf signalstats,metadata=print -f null - 2>&1)
    rc=$?
    if [ "$rc" != 0 ]; then
        PICTURE="ffmpeg exit $rc: $(grep -v '^\[' <<<"$out" | tail -1)"
        return
    fi
    # Flat (luma spread within 8 levels) and dark (mean at most 40 of 255).
    PICTURE=$(awk -F= '
        /signalstats\.YMIN=/ { if (lo == "" || $2 < lo) lo = $2 }
        /signalstats\.YMAX=/ { if ($2 > hi) hi = $2 }
        /signalstats\.YAVG=/ { if ($2 > avg) avg = $2; n++ }
        END {
            if (!n) { print "no picture statistics from ffmpeg"; exit }
            print (hi - lo <= 8 && avg <= 40) ? "black" : "ok"
        }' <<<"$out")
}

stage_audio() {
    stage "🔊 Audio"
    local DEV="$CARD_DEV" SRC=""
    local matched=""
    resolve_audio_source && matched="$SRC"
    local -a items=()
    if [ -n "$matched" ]; then
        say "The card's own audio input, found on its USB device:"
        note "$matched"
        items+=("auto|Use the card's audio (found automatically each start)|recommended")
    else
        ui_warn "No audio input found on the card's USB device."
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
    say "the monitor that has focus can switch to its mode nearest ${NEW[FPS]} Hz."
    [ -n "$mon" ] && { center ""; note "Focused monitor now: $mon"; }
    center ""
    local pick
    # With nothing in the file, start on "match": that is what a menu launch
    # does today, and writing "keep" by default would quietly turn it off.
    local def="${NEW[REFRESH]:-match}"
    choose pick "Refresh" "$def" \
        "match|Match the feed while it runs, restore afterwards|recommended" \
        "keep|Leave the display alone" || exit 1
    NEW[REFRESH]="$pick"
}

# mpv is the player: the overlay and the live keys exist only there. gst and
# ffplay stay for latency comparisons and fault isolation, set by hand
# (BACKEND=gst | ffplay); a config that already says so is asked about.
stage_backend() {
    stage "🎬 Player"
    local pick=mpv
    if [ "${NEW[BACKEND]:-mpv}" != mpv ]; then
        ui_warn "The config plays with BACKEND=${NEW[BACKEND]}, a diagnostic player."
        note "It has no overlay and no live keys (Tab, audio sync, volume); mpv has all of them."
        center ""
        confirm_yes "Switch to mpv?" || pick="${NEW[BACKEND]}"
        center ""
    fi
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

# --- A/V sync -----------------------------------------------------------------
# A background feed without audio, plus the --tune loop in this terminal.
# Background jobs of a script ignore Ctrl-C, so the feed is always stopped
# explicitly: here, and from the EXIT trap if the wizard is interrupted.
# SYNC_SHELL is the wizard's own preview view.sh (started with --exec), never
# a pid read from the shared run directory: by the time of cleanup that file
# may name another session's player. TERM lets view.sh stop its own player,
# loopback and refresh switch through its EXIT trap.
sync_stop() {
    [ -n "$SYNC_SHELL" ] || return 0
    kill -TERM "$SYNC_SHELL" 2>/dev/null
    wait "$SYNC_SHELL" 2>/dev/null
    SYNC_SHELL=""
    rm -f "$RUN_DIR/tune.out"
}

_lock_free() { ( flock -n 9 ) 9>"$RUN_DIR/$1.lock"; }

stage_sync() {
    stage "🎚️  Audio sync"
    if [ "${NEW[AUDIO]}" = none ]; then
        note "No audio was chosen, so there is nothing to line up."
        center ""; pause; return
    fi
    say "Audio through the PipeWire loopback arrives before the picture, which"
    say "takes the card, USB and display path. DELAY holds the audio back to match."
    note "Current: DELAY=$(cur DELAY) s. Needs a live picture with sound, e.g. console menu clicks."
    center ""
    confirm_yes "Line them up now? (a video window opens, the tuner runs here)" || return
    while ! _lock_free video || ! _lock_free audio; do
        ui_warn "A feed is running. Close it first."
        confirm_yes "Check again?" || return
    done
    local before
    before=$(desktop_active_window)
    rm -f "$RUN_DIR/tune.out"
    run_child --exec env AUDIO=none WINDOW=windowed WINDOW_SIZE=1280x720 REFRESH=keep \
        "$HERE/view.sh" </dev/null >"$RUN_DIR/setup-video.log" 2>&1 &
    SYNC_SHELL=$!
    # Ready when the player named in video.pid is our own preview's child: a
    # feed someone else started in the meantime does not count.
    local i ok="" vp
    for i in $(seq 50); do
        vp=$(cat "$RUN_DIR/video.pid" 2>/dev/null)
        if [ -n "$vp" ] && [ "$(ps -o ppid= -p "$vp" 2>/dev/null | tr -d ' ')" = "$SYNC_SHELL" ]; then
            ok=1; break
        fi
        kill -0 "$SYNC_SHELL" 2>/dev/null || break
        sleep 0.1
    done
    if [ -z "$ok" ]; then
        ui_bad "The video did not start. Last lines of its log:"
        tail -4 "$RUN_DIR/setup-video.log" | sed 's/\x1b\[[0-9;]*m//g' | while IFS= read -r l; do note "$l"; done
        sync_stop; center ""; pause; return
    fi
    sleep 1   # let the window map before taking focus back
    desktop_focus_window "$before" || note "Click back into this terminal to use the tuner keys."
    center ""
    run_child env TUNE_EMBEDDED=1 TUNE_OUT="$RUN_DIR/tune.out" DELAY="$(cur DELAY)" \
        "$HERE/view.sh" --tune
    local kept=""
    [ -s "$RUN_DIR/tune.out" ] && kept=$(cat "$RUN_DIR/tune.out")
    sync_stop
    center ""
    if [ -n "$kept" ]; then
        NEW[DELAY]="$kept"
        ui_ok "DELAY=$kept will be saved."
    else
        note "DELAY left at $(cur DELAY)."
    fi
    center ""
    pause
}

stage_debug() {
    stage "🐞 Troubleshooting"
    say "Normally off. Turn it on while chasing stutter, drops or audio drift;"
    say "./view.sh --debug does the same for a single run."
    center ""
    local pick
    choose pick "Debug output" "$(cur DEBUG)" \
        "off|Off|recommended" \
        "log|Log · every decision, verbose player output, a perf line every 5 s" \
        "window|Log + live dashboard in its own terminal" || exit 1
    if [ "$pick" = off ]; then NEW[DEBUG]=""; else NEW[DEBUG]="$pick"; fi
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

# --- report ------------------------------------------------------------------
# Everything a person (or an AI agent) needs to diagnose the setup, written
# whether or not the config was saved. Old reports are pruned to the last 10.
REPORT_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/protohdmicapture"
write_report() {
    mkdir -p "$REPORT_DIR" || return 1
    REPORT="$REPORT_DIR/setup-$(date +%Y%m%d-%H%M%S).md"
    local verdict k
    {
        echo "# protoHDMIcapture setup report"
        echo
        echo "- Generated: $(date -Is) by protoHDMIcapture $(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo '?')"
        echo "- Kernel: $(uname -r) · desktop adapter: $DESKTOP · session: ${WAYLAND_DISPLAY:+wayland }${DISPLAY:+x11}"
        echo "- Config: \`$CONFIG_FILE\` ($([ -n "$SAVED" ] && echo saved || { [ "$CHANGED" = 0 ] && echo unchanged || echo 'not saved'; }))"
        echo
        echo "## Summary"
        echo
        echo "| Check | Result | |"
        echo "|---|---|---|"
        echo "| Card | ${CARD_TYPE:-?} (\`${CARD_DEV:-?}\`, serial \`${NEW[CARD_SERIAL]:-none}\`, port \`${NEW[CARD_PORT]:-${CARD_DEV:+$(card_port "$CARD_DEV")}}\`) | |"
        echo "| Source | $(source_name "${NEW[SOURCE]}")${NEW[SOURCE]:+ (\`${NEW[SOURCE]}\`)} | |"
        echo "| Chip | ${CHIP:-?} (\`${VIDPID:-?}\`)${SUPPORT:+ · support: $SUPPORT} | |"
        verdict="✅"; [ "${USB_RESULT:-}" = ok ] || verdict="⚠️"
        echo "| USB link | ${LINK_SPEED:-?} Mbit/s | $verdict |"
        verdict="✅"; [ -n "$MEASURED_FPS" ] && [ "${PICTURE:-}" = ok ] || verdict="⚠️"
        echo "| Mode | ${NEW[RES]} @ ${NEW[FPS]} fps, ${NEW[FMT]} | |"
        echo "| Signal | ${SIGNAL_RESULT:-not checked} | $verdict |"
        echo "| Audio | $(if [ "${NEW[AUDIO]}" = none ]; then echo none; else echo "${NEW[SRC]:-card audio, found on its USB device}"; fi) | |"
        echo "| Display | $(cur WINDOW)$([ "$(cur WINDOW)" = windowed ] && echo " $(cur WINDOW_SIZE)"), refresh $(cur REFRESH) | |"
        echo "| Player | $(cur BACKEND)$([ "$(cur BACKEND)" = mpv ] && echo ", scaling $(cur SCALING)") | |"
        echo
        if [ -n "$SAVED" ]; then echo "## Config changes"; else echo "## Proposed config changes (not saved)"; fi
        echo
        echo '```diff'
        for k in "${MANAGED[@]}"; do
            [ "${CUR[$k]:-}" = "${NEW[$k]}" ] && continue
            [ -n "${CUR[$k]:-}" ] && echo "-$k=${CUR[$k]}"
            [ -n "${NEW[$k]}" ] && echo "+$k=${NEW[$k]}"
        done
        echo '```'
        echo
        echo "Config file now:"
        echo
        echo '```'
        cat "$CONFIG_FILE" 2>/dev/null || echo "(none)"
        echo '```'
        echo
        echo "## Raw diagnostics"
        _report_block "lsusb" lsusb
        _report_block "lsusb -t" lsusb -t
        [ -n "${VIDPID:-}" ] && _report_block "lsusb -v -d $VIDPID (excerpt)" \
            bash -c "lsusb -v -d $VIDPID 2>/dev/null | grep -E 'bcdUSB|bcdDevice|iManufacturer|iProduct|iSerial|MaxPower|bInterfaceClass'"
        _report_block "USB device speeds (sysfs)" bash -c '
            for d in /sys/bus/usb/devices/[0-9]*-[0-9]*; do
                [ -r "$d/idVendor" ] || continue
                printf "%-10s %s:%s %6s Mbit/s  %s\n" "$(basename "$d")" "$(cat "$d/idVendor")" \
                    "$(cat "$d/idProduct")" "$(cat "$d/speed" 2>/dev/null)" "$(cat "$d/product" 2>/dev/null || echo "(no product string)")"
            done'
        _report_block "v4l2-ctl --list-devices" v4l2-ctl --list-devices
        if [ -n "${CARD_DEV:-}" ]; then
            _report_block "v4l2-ctl --info" v4l2-ctl -d "$CARD_DEV" --info
            _report_block "v4l2-ctl --list-formats-ext" v4l2-ctl -d "$CARD_DEV" --list-formats-ext
            _report_block "v4l2-ctl --get-fmt-video / --get-parm" bash -c "v4l2-ctl -d $CARD_DEV --get-fmt-video; v4l2-ctl -d $CARD_DEV --get-parm"
            _report_block "DV timings (UVC cards do not support these; failure is normal)" \
                bash -c "v4l2-ctl -d $CARD_DEV --get-dv-timings 2>&1; v4l2-ctl -d $CARD_DEV --query-dv-timings 2>&1"
            _report_block "v4l2-ctl --list-ctrls" v4l2-ctl -d "$CARD_DEV" --list-ctrls
        fi
        _report_block "pactl list short sources" pactl list short sources
        _report_block "pactl list short sinks" pactl list short sinks
        _report_block "arecord -l" arecord -l
        _report_block "Kernel log: USB / UVC (last 30)" bash -c "journalctl -k -b --no-pager 2>/dev/null | grep -iE 'uvc|usb ${USB_DIR##*/}[: ]|${VIDPID:-xxxx}' | tail -30"
        _report_block "Players" bash -c 'mpv --version 2>/dev/null | head -1; gst-launch-1.0 --version 2>/dev/null | head -2; ffplay -version 2>/dev/null | head -1'
    } >"$REPORT"
    # Keep the newest 10.
    find "$REPORT_DIR" -maxdepth 1 -name 'setup-*.md' -printf '%T@ %p\n' 2>/dev/null \
        | sort -rn | tail -n +11 | cut -d' ' -f2- | xargs -r rm -f
}

_report_block() {
    local title="$1"; shift
    echo
    echo "### $title"
    echo
    echo '```'
    "$@" 2>&1 | sed 's/\x1b\[[0-9;]*m//g'
    echo '```'
}

# --- run ---------------------------------------------------------------------
USB_DIR=""; VIDPID=""; LINK_SPEED=""; CHIP=""; SUPPORT=""; USB_RESULT=""
REPORT=""
MEASURED_FPS=""
SIGNAL_RESULT=""; PICTURE=""
CARD_DEV=""
CARD_TYPE=""
for s in "${STAGES[@]}"; do "stage_$s"; done
write_report || REPORT=""

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
say "Card    : $CARD_TYPE (${CARD_DEV}) · ${LINK_SPEED:-?} Mbit/s"
say "Mode    : ${NEW[RES]} @ ${NEW[FPS]} fps, ${NEW[FMT]}${SIGNAL_RESULT:+ · $SIGNAL_RESULT}"
say "Display : $(cur WINDOW)$([ "$(cur WINDOW)" = windowed ] && echo " $(cur WINDOW_SIZE)") · player $(cur BACKEND)"
center ""
if [ -n "$REPORT" ]; then
    say "📄 Report: ${BOLD}$REPORT${RESET}"
    command -v wl-copy >/dev/null 2>&1 && note "Copy it for an AI agent or a forum post:  wl-copy < $REPORT"
    center ""
fi
note "Change any of this: ./view.sh --setup"
center ""
if [ -n "$FROM_LAUNCHER" ]; then
    if [ -n "$SAVED" ]; then pause "⏎  Press Enter to start the feed"; else pause "⏎  Press Enter to close"; fi
elif [ -z "$THEN_PLAY" ]; then
    note "Play: ./view.sh, or protoHDMIcapture in your app launcher."
    center ""
fi
