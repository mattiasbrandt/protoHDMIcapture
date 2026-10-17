#!/usr/bin/env bash
# The Linux release archive (#14): the Rust viewer's binary with its desktop
# entry, icon, licenses and the data files, for a distro without a package.
# The profiles in capture-cards/ and sources/ are built into the binary
# (viewer/build.rs); the archive carries them as documentation.
#
# Usage: packaging/release-archive.sh [OUT_DIR]   (default: dist/)
# Builds viewer/ in release mode with the locked dependencies, then writes
# OUT_DIR/protohdmicapture-<version>-<arch>-linux.tar.gz and prints its path.
set -euo pipefail
root=$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)
out=${1:-$root/dist}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/viewer/Cargo.toml" | head -1)
[ -n "$version" ] || { echo "release-archive: no version in viewer/Cargo.toml" >&2; exit 1; }
arch=$(uname -m)
name="protohdmicapture-$version-$arch-linux"

cargo build --manifest-path "$root/viewer/Cargo.toml" --release --locked
target=$(cargo metadata --manifest-path "$root/viewer/Cargo.toml" --format-version 1 --no-deps |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
d="$stage/$name"
install -Dm755 "$target/release/protohdmicapture" "$d/bin/protohdmicapture"
# The release profile keeps debug info (debug = 1, for profiling); the
# archive ships without it, as makepkg's strip does for the package.
strip --strip-debug "$d/bin/protohdmicapture"
install -Dm644 "$root/packaging/protohdmicapture.desktop" "$d/share/applications/protohdmicapture.desktop"
# The app icon at every size, as the hicolor theme lays it out (#35).
(cd "$root/packaging/linux/icons" && find hicolor -type f) | while read -r f; do
    install -Dm644 "$root/packaging/linux/icons/$f" "$d/share/icons/$f"
done
install -Dm644 -t "$d/share/licenses/protohdmicapture" "$root/LICENSE-MIT" "$root/LICENSE-APACHE"
install -Dm644 -t "$d/share/doc/protohdmicapture" "$root/README.md" "$root/packaging/INSTALL.md"
install -Dm644 -t "$d/share/doc/protohdmicapture/capture-cards" "$root"/capture-cards/*.md
install -Dm644 -t "$d/share/doc/protohdmicapture/sources" "$root"/sources/*.md
install -Dm755 "$root/packaging/install.sh" "$d/install.sh"

mkdir -p "$out"
tar -C "$stage" --owner=0 --group=0 --sort=name -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"
