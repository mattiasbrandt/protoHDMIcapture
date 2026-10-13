# shellcheck shell=bash
# Mute and volume of this session's audio loopback, for the overlay's m, 9
# and 0 keys (`view.sh --audio`). Sourced by view.sh.
#
# They act on the loopback's playback stream (node output.protohdmicapture-
# audio, a sink input to pactl), never on the sink: other sound is untouched.
# The wanted state is kept in $RUN_DIR/audio.state, so a loopback restarted
# for an audio sync change comes back with it (audio_state_apply).
#
# The overlay starts helpers without waiting, so two can run at once: each
# reads, changes and records under audio.ctl.lock, or a volume change can
# write back the mute state another just replaced.
#
#   audio_stream_id          the stream's sink-input index, or nothing
#   audio_status ID          "volume=N mute=yes|no"
#   audio_control ACTION     status | mute | volume +N | volume -N; prints
#                            the status (status changes nothing)
#   audio_state_apply        re-apply audio.state to a new stream

audio_stream_id() {
    pactl list sink-inputs 2>/dev/null | awk '
        /^Sink Input #/ { id = substr($3, 2) }
        /node\.name = "output\.protohdmicapture-audio"/ { print id; exit }'
}

audio_status() {
    pactl list sink-inputs 2>/dev/null | awk -v want="$1" '
        /^Sink Input #/ { cur = substr($3, 2) }
        cur == want && /^[[:space:]]*Mute:/   { m = $2 }
        cur == want && /^[[:space:]]*Volume:/ { v = $5; sub(/%/, "", v) }
        END { if (v == "") exit 1; print "volume=" v " mute=" m }'
}

# _audio_locked FN ARGS...: run FN holding audio.ctl.lock.
_audio_locked() {
    local lk rc
    exec {lk}>>"$RUN_DIR/audio.ctl.lock" || { echo "cannot open $RUN_DIR/audio.ctl.lock" >&2; return 1; }
    if ! flock -w 5 "$lk"; then
        exec {lk}>&-
        echo "audio controls are busy (audio.ctl.lock held for 5 s)" >&2
        return 1
    fi
    "$@"; rc=$?
    exec {lk}>&-
    return "$rc"
}

audio_control() { _audio_locked _audio_control "$@"; }
_audio_control() {
    local id st vol mute
    id=$(audio_stream_id)
    [ -n "$id" ] || { echo "no audio loopback is running" >&2; return 1; }
    st=$(audio_status "$id") || { echo "cannot read the loopback's volume" >&2; return 1; }
    vol=${st#volume=}; vol=${vol%% *}; mute=${st##*mute=}
    case "$1" in
        status) echo "volume=$vol mute=$mute"; return 0 ;;
        mute)
            [ "$mute" = yes ] && mute=no || mute=yes
            pactl set-sink-input-mute "$id" "$mute" || { echo "pactl could not set mute" >&2; return 1; } ;;
        volume)
            [[ "${2:-}" =~ ^[+-][0-9]+$ ]] || { echo "volume +N or -N" >&2; return 2; }
            # 0..100: past 100 % the loopback only clips.
            vol=$(( vol + $2 )); (( vol < 0 )) && vol=0; (( vol > 100 )) && vol=100
            pactl set-sink-input-volume "$id" "$vol%" || { echo "pactl could not set the volume" >&2; return 1; } ;;
        *) echo "audio: status | mute | volume +N | volume -N" >&2; return 2 ;;
    esac
    # Written whole and renamed: a reader never sees half of it.
    printf 'VOLUME=%s\nMUTE=%s\n' "$vol" "$mute" >"$RUN_DIR/audio.state.tmp" \
        && mv -f "$RUN_DIR/audio.state.tmp" "$RUN_DIR/audio.state" \
        || { echo "cannot write $RUN_DIR/audio.state" >&2; return 1; }
    echo "volume=$vol mute=$mute"
}

# Give a restarted loopback's stream the session's state. WirePlumber
# usually restores a stream's last volume by its node name (it also carries
# the volume from one session to the next); applying it here keeps the
# restart independent of that. The stream appears a moment after
# pw-loopback starts.
audio_state_apply() { _audio_locked _audio_state_apply; }
_audio_state_apply() {
    [ -r "$RUN_DIR/audio.state" ] || return 0
    local vol mute id _
    vol=$(sed -n 's/^VOLUME=//p' "$RUN_DIR/audio.state")
    mute=$(sed -n 's/^MUTE=//p' "$RUN_DIR/audio.state")
    for _ in $(seq 20); do
        id=$(audio_stream_id); [ -n "$id" ] && break
        sleep 0.1
    done
    [ -n "$id" ] || { warn "audio: the new loopback stream did not appear; volume and mute not restored"; return 1; }
    pactl set-sink-input-volume "$id" "${vol:-100}%" && pactl set-sink-input-mute "$id" "${mute:-no}" \
        || { warn "audio: could not restore volume ${vol:-100}% and mute ${mute:-no}"; return 1; }
    dbg "audio: restored volume ${vol:-100}% mute ${mute:-no}"
}
