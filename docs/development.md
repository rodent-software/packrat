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
structure parser and AACS hook come from a local fork of `oxideav-bluray` under
`vendor/`, which fixes the `index.bdmv` object layout and accepts an explicit
`KEYDB.cfg` path. AACS keys remain user-supplied; see [Blu-ray and AACS
keys](blu-ray.md).

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
