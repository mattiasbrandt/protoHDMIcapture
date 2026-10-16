#!/usr/bin/env bash
# Installs the release archive's files under a prefix (#14).
# Usage: ./install.sh [PREFIX]   (default: ~/.local; /usr/local needs root)
#        ./install.sh --uninstall [PREFIX]
set -euo pipefail
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
uninstall=no
if [ "${1:-}" = --uninstall ]; then
    uninstall=yes
    shift
fi
prefix=${1:-$HOME/.local}

files=(
    bin/protohdmicapture
    share/applications/protohdmicapture.desktop
    share/icons/hicolor/scalable/apps/protohdmicapture.svg
)
dirs=(share/licenses/protohdmicapture share/doc/protohdmicapture)

if [ "$uninstall" = yes ]; then
    for f in "${files[@]}"; do rm -fv "$prefix/$f"; done
    for d in "${dirs[@]}"; do rm -rfv "${prefix:?}/$d"; done
    exit 0
fi

for f in "${files[@]}"; do
    install -Dm"$( [ "${f%%/*}" = bin ] && echo 755 || echo 644 )" "$here/$f" "$prefix/$f"
    echo "installed $prefix/$f"
done
# The menu runs it by full path: a desktop session's PATH often lacks
# ~/.local/bin.
sed -i "s|^Exec=protohdmicapture |Exec=$prefix/bin/protohdmicapture |" "$prefix/share/applications/protohdmicapture.desktop"
for d in "${dirs[@]}"; do
    mkdir -p "$prefix/$d"
    cp -r "$here/$d/." "$prefix/$d/"
    echo "installed $prefix/$d/"
done
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: $prefix/bin is not on PATH; the menu entry works, a terminal needs the full path" >&2 ;;
esac
