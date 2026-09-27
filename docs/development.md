# Development

```sh
git clone https://github.com/rodent-software/packrat
cd packrat
cargo build
```

## libdvdcss

packrat never links or bundles `libdvdcss`; it loads the copy you install at
runtime. To exercise CSS decryption without a system-wide install, point
`PACKRAT_DVDCSS` at a library and check that it is found:

```sh
PACKRAT_DVDCSS=/path/to/libdvdcss.so.2 cargo run -- doctor
```

See [Installation](installation.md) for the details and per-platform sources.

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
