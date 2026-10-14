#!/usr/bin/env bash
# Builds present-shim.so into poc/rust/target/present-shim/ (ignored by git).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out="$here/../target/present-shim"
xml=/usr/share/wayland-protocols/stable/presentation-time/presentation-time.xml
mkdir -p "$out"
wayland-scanner client-header "$xml" "$out/presentation-time-client-protocol.h"
wayland-scanner private-code "$xml" "$out/presentation-time-protocol.c"
cc -O2 -Wall -Wextra -shared -fPIC -I"$out" -o "$out/present-shim.so" \
    "$here/present-shim.c" "$out/presentation-time-protocol.c" -lwayland-client -ldl -lpthread
echo "$out/present-shim.so"
