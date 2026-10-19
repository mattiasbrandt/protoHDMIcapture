# Installing the Rust viewer

The Rust viewer (`protohdmicapture`, in `viewer/`) is the viewer the
releases ship; `view.sh` + mpv, run from a checkout, is the older
alternative ([docs/view-sh.md](../docs/view-sh.md)). Both read the same
settings file, `~/.config/protohdmicapture/config`.
With no settings file, it opens the setup wizard in its window.

On Linux it needs a Wayland desktop with a Vulkan driver, and PipeWire for
the sound. Refresh matching and window placement need Hyprland; other
desktops get the feed without them. Windows: see [Windows](#windows).

## Linux: the release archive

One archive for every distro with glibc 2.36 and PipeWire 0.3.65 or
later: Debian 12, Ubuntu 24.04, Fedora, Arch and newer. Each release is installed and run on
those in CI before it is published.

```bash
tar xf protohdmicapture-<version>-x86_64-linux.tar.gz
cd protohdmicapture-<version>-x86_64-linux
./install.sh                  # into ~/.local; ./install.sh /usr/local as root
./install.sh --uninstall      # the same prefix removes it again
```

Run-time libraries: glibc 2.36 or later, libgcc, PipeWire's libpipewire,
ALSA's libasound, libdbus, Wayland's libwayland-client, libxkbcommon, the
Vulkan loader (libvulkan) and a Vulkan driver. A desktop with PipeWire has
them all; by package:

| Distro | Packages |
|---|---|
| Ubuntu, Debian | `libpipewire-0.3-0 libasound2 libdbus-1-3 libwayland-client0 libxkbcommon0 libvulkan1 mesa-vulkan-drivers` |
| Fedora | `pipewire-libs alsa-lib dbus-libs libwayland-client libxkbcommon vulkan-loader mesa-vulkan-drivers` |
| Arch | `libpipewire alsa-lib dbus wayland libxkbcommon vulkan-icd-loader` and a `vulkan-driver` |

To build the archive: `packaging/release-archive.sh [OUT_DIR]` (Rust
toolchain, clang and the libraries' development files). Built on a newer
distro, it needs that distro's glibc.

## Windows

Download `protohdmicapture-<version>-x86_64-windows.msi` from the GitHub
release and double-click it. It installs for your user only, with no
administrator prompt, into `%LOCALAPPDATA%\Programs\protoHDMIcapture`, and
adds **protoHDMIcapture** to the Start menu. The first start opens the setup
wizard. A newer version installed over it replaces it; Settings → Apps
removes it. Your settings (`%APPDATA%\protohdmicapture`) stay when it is
removed.

The installer is not signed, so Windows SmartScreen may say it does not
know the publisher: **More info → Run anyway**.

It needs Windows 10 or 11 (x64) with a DirectX 12 graphics driver. Nothing
else: the Visual C++ runtime is built in. On Windows the viewer captures
through Media Foundation and plays the card's sound through WASAPI;
refresh matching and `--monitor` are Hyprland only.

To check a download: `SHA256SUMS` on the release lists every file's
SHA-256 (`Get-FileHash <file>` in PowerShell, `sha256sum -c SHA256SUMS`
on Linux).

To build the installer: `packaging\windows\build-msi.ps1 [-OutDir dist]`
(the Rust toolchain for x86_64-pc-windows-msvc, Visual Studio's C++ build
tools, and WiX 7: `dotnet tool install --global wix --version 7.0.0`; WiX 7's
[Open Source Maintenance Fee](https://docs.firegiant.com/wix/osmf/) EULA is
accepted for this project in the script). `packaging\windows\test-msi.ps1
-Msi <file>` installs, checks and removes it. A `v*` tag builds and tests
both packages in CI (`.github/workflows/release.yml`) and publishes them
with `SHA256SUMS`.

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
