# Installing the Rust viewer

The Rust viewer (`protohdmicapture`, in `viewer/`) is the coming replacement
for `view.sh` + mpv (#10). It is opt-in until it does everything view.sh
does; both read the same settings file, `~/.config/protohdmicapture/config`.
With no settings file, it opens the setup wizard in its window.

It needs a Wayland desktop with a Vulkan driver, and PipeWire for the sound.
Refresh matching and window placement need Hyprland; other desktops get the
feed without them.

## Arch Linux

From the AUR (once published): `protohdmicapture`. To build the package
from this checkout instead:

```bash
cd packaging && makepkg -si
```

The PKGBUILD builds a tagged release (`v<version>` on GitHub).

## Other distributions: the release archive

```bash
tar xf protohdmicapture-<version>-x86_64-linux.tar.gz
cd protohdmicapture-<version>-x86_64-linux
./install.sh                  # into ~/.local; ./install.sh /usr/local as root
./install.sh --uninstall      # the same prefix removes it again
```

Run-time libraries: glibc, libgcc, PipeWire's libpipewire, ALSA's
libasound, libdbus, Wayland's libwayland-client, libxkbcommon, the Vulkan
loader (libvulkan) and a Vulkan driver.

To build the archive: `packaging/release-archive.sh [OUT_DIR]` (Rust
toolchain, clang and the libraries' development files).

## From source

```bash
cargo build --manifest-path viewer/Cargo.toml --release
viewer/target/release/protohdmicapture
```

`d` opens the dashboard with `xdg-terminal-exec`, else `$TERMINAL`, else
`x-terminal-emulator`; on Hyprland it goes to another monitor than the
viewer's. With one open, `d` raises it.

## License

MIT OR Apache-2.0, at your option: `LICENSE-MIT`, `LICENSE-APACHE`.
