# Installation

packrat is a single executable with no native build dependencies. Backing up an
unencrypted disc needs nothing else. Reading a **CSS-encrypted** DVD through the
raw device additionally needs `libdvdcss`, which packrat loads at runtime from a
copy you install — see [Enabling CSS decryption](#enabling-css-decryption).

## Installing packrat

### One-line installers

```sh
# Linux and macOS
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rodent-software/packrat/releases/latest/download/packrat-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/rodent-software/packrat/releases/latest/download/packrat-installer.ps1 | iex"
```

### Package managers

| Platform | Command |
| --- | --- |
| macOS (Homebrew) | `brew install rodent-software/tap/packrat` |
| Arch (AUR) | `yay -S packrat-bin` |
| Debian / Ubuntu (apt) | `sudo apt update && sudo apt install packrat` |
| Fedora / RHEL (COPR) | `sudo dnf copr enable rodent-software/packrat && sudo dnf install packrat` |
| Nix | `nix profile install github:rodent-software/packrat` |
| Windows (Winget) | `winget install RodentSoftware.Packrat` |
| Windows (Scoop) | `scoop bucket add rodent-software https://github.com/rodent-software/scoop-bucket && scoop install packrat` |

Package-manager builds declare `libdvdcss` as a dependency wherever the
platform's own repositories carry it, so on those channels CSS decryption works
without a separate step.

### From source

```sh
cargo install --git https://github.com/rodent-software/packrat packrat
```

## Why packrat does not bundle libdvdcss

`libdvdcss` decrypts discs protected with the Content Scramble System (CSS).
packrat deliberately **never links, bundles, mirrors, or downloads** it, and
instead loads a copy that you install.

This is not a licensing problem: `libdvdcss` is GPL-2.0-or-later and packrat is
GPL-3.0-or-later, which are compatible. The concern is that **distributing** a
CSS circumvention tool is legally sensitive in some jurisdictions. Many Linux
distributions — Debian, Ubuntu, Fedora and openSUSE among them — decline to ship
the library for exactly this reason, and instead provide a way for you to
install it yourself. packrat follows the same approach: the decision to obtain
the library, and the responsibility that goes with it, stays with you and your
distribution rather than with packrat.

Everything packrat does **without** `libdvdcss` still works:

- unencrypted discs, read straight from the device or a mounted folder;
- extracted `VIDEO_TS` folders and disc images that are already decrypted;
- discs whose encryption your operating system already handles for the mounted
  filesystem.

Only raw-device reads of a CSS-scrambled disc need the library.

## Enabling CSS decryption

Install `libdvdcss` from your distribution or package manager, then confirm that
packrat finds it with [`packrat doctor`](#checking-your-setup).

### Linux

| Distribution | Command |
| --- | --- |
| Arch / Manjaro | `sudo pacman -S libdvdcss` |
| Debian / Ubuntu | `sudo apt install libdvd-pkg && sudo dpkg-reconfigure libdvd-pkg` |
| Fedora | Enable [RPM Fusion](https://rpmfusion.org/Configuration), then `sudo dnf install libdvdcss` |
| openSUSE | Add the [Packman](https://en.opensuse.org/Additional_package_repositories#Packman) repository, then `sudo zypper install libdvdcss` |
| Void | `sudo xbps-install -S libdvdcss` |
| Gentoo | `sudo emerge libdvdcss` |
| NixOS | Add `libdvdcss` to `environment.systemPackages` (or `nix-shell -p libdvdcss`) |

Debian and Ubuntu's `libdvd-pkg` builds the library from source on your machine
rather than shipping a binary, so it needs a compiler; `dpkg-reconfigure` is the
step that actually fetches and builds it.

### macOS

```sh
brew install libdvdcss
```

The Homebrew formula for packrat depends on `libdvdcss`, so installing packrat
through Homebrew pulls it in automatically. Homebrew installs the dylib under
its prefix (`/opt/homebrew/lib` on Apple Silicon, `/usr/local/lib` on Intel),
both of which packrat searches. With MacPorts, use `sudo port install libdvdcss`.

### Windows

There is no entry for it in Winget, Scoop or Chocolatey. The most reliable
source is [MSYS2](https://www.msys2.org/), which packages the library for both
architectures:

```powershell
# x86_64
pacman -S mingw-w64-ucrt-x86_64-libdvdcss
# arm64
pacman -S mingw-w64-clang-aarch64-libdvdcss
```

Then copy `libdvdcss-2.dll` from the MSYS2 `bin` directory (`C:\msys64\ucrt64\bin`
for x86_64, `C:\msys64\clangarm64\bin` for arm64) next to `packrat.exe`, into a
directory on your `PATH`, or anywhere and point `PACKRAT_DVDCSS` at it.

Avoid downloading `libdvdcss` DLLs from random file-hosting sites; build it from
the [upstream source](https://download.videolan.org/pub/libdvdcss/) or use a
package manager.

## Configuring the library location

packrat looks for the library in this order:

1. `PACKRAT_DVDCSS`, if set — either the library file itself, or a directory
   containing it;
2. next to the `packrat` executable, so a portable copy travels with it;
3. well-known package-manager locations: Homebrew prefixes on macOS, and
   `/usr/lib`, `/usr/lib64`, `/usr/local/lib` and `/lib` on Linux; on Windows,
   the VLC install directory, which ships its own copy;
4. the platform loader's default search path (`libdvdcss.so.2` on Linux,
   `libdvdcss.2.dylib` on macOS, `libdvdcss-2.dll` on Windows).

To use a copy that is not in one of those places:

```sh
# Point at the file itself…
PACKRAT_DVDCSS=/opt/lib/libdvdcss.so.2 packrat

# …or at the directory that holds it.
PACKRAT_DVDCSS=/opt/lib packrat
```

## Checking your setup

```sh
packrat doctor
```

It reports the platform and whether CSS decryption is available, along with the
path of the library it found. When the library is missing it explains why and
where to get it. A working setup looks like:

```text
packrat  : 0.1.0
platform : linux (x86_64)
css      : available
libdvdcss: /usr/lib/x86_64-linux-gnu/libdvdcss.so.2
```
