# Development

```sh
git clone https://github.com/rodent-software/packrat
cd packrat
cargo build
```

## Layout

The workspace is three crates:

- `packrat-core` — disc I/O: optical device access, DVD-Video and Blu-ray
  structure parsing, CSS/AACS decryption and remuxing. It has no UI and no user
  preferences.
- `packrat-engine` — orchestration between core and the window: saved settings,
  TVmaze/TMDb matching, planning a disc into the list of files to write, and the
  background worker threads. It has no graphical assumptions.
- `packrat-gui` — the `packrat` binary: the Iced desktop application, split into
  state and update logic (`main.rs`), the per-file model (`jobs.rs`) and the
  screen layouts (`view.rs`).

Planning decisions belong in `packrat-engine` rather than a screen, so the flow
stays testable without a display and the layouts cannot disagree about which
files a disc produces.

## libdvdcss

packrat never links or bundles `libdvdcss`; it loads the copy you install at
runtime. To exercise CSS decryption without a system-wide install, point
`PACKRAT_DVDCSS` at a library and check that it is found:

```sh
PACKRAT_DVDCSS=/path/to/libdvdcss.so.2 cargo run -- doctor
```

See [Installation](installation.md) for the details and per-platform sources.

## Blu-ray

Blu-ray structure and remuxing live in `packrat-core/src/bluray.rs`. The
playlist parser (`bluray/mpls.rs`), M2TS source-packet reader
(`bluray/source.rs`), and AACS VUK/CPS-unit decryptor (`bluray/aacs.rs`) are
packrat-owned. AACS keys remain user-supplied; see [Blu-ray and AACS
keys](blu-ray.md).

The MPEG-TS/PES reader lives in `packrat-core/src/media/ts.rs`, and the
Matroska writer/reader support lives in `packrat-core/src/mkv/`, adapted from
the MIT-licensed `oxideav-mkv` code. The shared packet/track types and
container traits live in `packrat-core/src/media/core/`, adapted from
`oxideav-core`; its license is kept alongside the module. `media.rs` remains
the seam the ripping code uses. The DVD filesystem, IFO/PGC, VOB/MPEG-PS, and
codec-header parsers live in `packrat-core/src/dvd/`, adapted from
`oxideav-dvd` (licenses are kept alongside each adapted module). The DVD
title-set facade in `disc.rs` (`DvdTitleSet`) keeps parser types out of the
public API.

## Releases

Releases are driven by [`dist`](https://axodotdev.github.io/cargo-dist) from the
configuration in `dist-workspace.toml`. Pushing a `v*` tag builds six targets —
x86_64/aarch64 Linux, x86_64/aarch64 macOS, and x86_64/aarch64 Windows — plus
shell and PowerShell installers, an MSI per Windows architecture, a Homebrew
formula and a `packrat-update` updater. `.github/workflows/release.yml` is
generated; run `dist init` after changing the configuration and commit the
result.

Preview a release without building it:

```sh
dist plan
```

Package-manager channels (apt/rpm, AUR, Winget, Scoop) are described in
[Distributing](distribution.md).
