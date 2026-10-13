#!/usr/bin/env bash
# The refresh record (7d645ed, 49be2cf, 2a6d6d5), with a stand-in hyprctl:
# recovery restores only a monitor still at the rate set, and removes the
# record only once settled; a failed restore keeps it for the next run.
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"

mkdir -p "$HARNESS_TMP/bin"
cat >"$HARNESS_TMP/bin/hyprctl" <<'EOF'
#!/bin/sh
case "$1" in
monitors) [ -n "$RATE" ] && echo "[{\"name\":\"DP-1\",\"refreshRate\":$RATE}]" || echo "[]" ;;
eval) exit "${EVAL_RC:-0}" ;;
esac
EOF
chmod +x "$HARNESS_TMP/bin/hyprctl"
REC="$HARNESS_TMP/refresh.restore"

# recover RATE EVAL_RC -> "status kept|removed"
recover() {
    echo 'DP-1|59.97|100|hl.monitor({})' >"$REC"
    RATE="$1" EVAL_RC="$2" PATH="$HARNESS_TMP/bin:$PATH" DESKTOP=hyprland bash -c '
        . lib/desktop.sh; desktop_refresh_recover "$1"; echo "$? $([ -e "$1" ] && echo kept || echo removed)"' _ "$REC"
}
[ "$(recover "" 0)"        = "1 kept" ]    || no "monitor unreadable: record not kept"
[ "$(recover 59.973 1)"    = "1 kept" ]    || no "restore failed: record not kept"
[ "$(recover 59.973 0)"    = "0 removed" ] || no "restored: record not removed"
[ "$(recover 84.964 0)"    = "0 removed" ] || no "changed by hand: record not removed"

# restore EVAL_RC -> "status kept|removed"
restore() {
    echo x >"$REC"
    EVAL_RC="$1" PATH="$HARNESS_TMP/bin:$PATH" DESKTOP=hyprland bash -c '
        . lib/desktop.sh; _DESKTOP_RESTORE="hl.monitor({})"; _DESKTOP_RECORD="$1"
        desktop_refresh_restore; echo "$? $([ -e "$1" ] && echo kept || echo removed)"' _ "$REC"
}
[ "$(restore 1)" = "1 kept" ]    || no "exit restore failed: record not kept"
[ "$(restore 0)" = "0 removed" ] || no "exit restore worked: record not removed"
finish
