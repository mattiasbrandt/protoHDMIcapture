#!/usr/bin/env bash
# tools/process-globals (#25), on a scratch tree: process-global state
# with no `// process-global: <reason>` fails, as the dashboard's LIVE did
# (#21); marked on its line or the line above it passes; a marked
# thread_local! covers its statics; a const, a &'static type, a use line
# and a comment are not state.
set -uo pipefail
# shellcheck source=tools/harness/lib.sh
. "$(dirname "$0")/lib.sh"

D="$HARNESS_TMP/src"
# expect RC FILE-CONTENT [LINE-REGEX] -> the check on a tree of that one file
expect() {
    local want="$1" content="$2" re="${3:-}" out rc
    rm -rf "$D"; mkdir -p "$D"
    printf '%s\n' "$content" >"$D/a.rs"
    out=$(tools/process-globals "$D" 2>&1); rc=$?
    [ "$rc" = "$want" ] || { no "exit $rc, expected $want, for:"$'\n'"$content"$'\n'"$out"; return; }
    [ -z "$re" ] || grep -Eq -- "$re" <<<"$out" || no "no line matches /$re/ in: $out"
}

# Unmarked: each kind fails, named by file and line.
expect 1 'static LIVE: LazyLock<Live> = LazyLock::new(Live::new);' 'a\.rs:1: static LIVE'
expect 1 $'fn f() {\n    static N: AtomicUsize = AtomicUsize::new(0);\n}' 'a\.rs:2: static N'
expect 1 'pub(crate) static mut COUNT: u32 = 0;' 'a\.rs:1: '
expect 1 $'thread_local! {\n    static X: Cell<u8> = const { Cell::new(0) };\n}' 'a\.rs:1: thread_local!'
expect 1 'fn f() -> &OnceLock<u8> { todo!() }' 'a\.rs:1: '
expect 1 'lazy_static! { static ref R: Regex = Regex::new("x").unwrap(); }' 'a\.rs:1: '
# A marker with no reason is no marker.
expect 1 $'// process-global:\nstatic N: u8 = 0;' 'a\.rs:2: '
# A marker two lines up is not the line above.
expect 1 $'// process-global: counts\n\nstatic N: u8 = 0;' 'a\.rs:3: '

# Marked: on the line, or the line above (between a doc comment and it).
expect 0 'static N: AtomicU64 = AtomicU64::new(0); // process-global: temp names'
expect 0 $'/// Temp files.\n// process-global: temp names\nstatic N: AtomicU64 = AtomicU64::new(0);'
# A marked thread_local! covers its statics, and only until it closes.
expect 0 $'// process-global: one event loop\nthread_local! {\n    /// doc\n    static L: RefCell<u8> = const { RefCell::new(0) };\n}'
expect 1 $'// process-global: one event loop\nthread_local! {\n    static L: u8 = 0;\n}\nstatic AFTER: u8 = 0;' 'a\.rs:5: static AFTER'

# Not state.
expect 0 $'const N: u32 = 1;\nfn name() -> &\'static str { "x" }\nuse std::sync::OnceLock;\n// a LazyLock was here (#21)\n/// static X is gone'

# protoHDMIcapture's own tree passes (each of its statics is marked).
tools/process-globals >/dev/null 2>&1 || no "src has unmarked process-global state: run tools/process-globals"
finish
