# Development

```sh
git clone https://github.com/rodent-software/packrat
cd packrat
cargo build --features dvdcss
```

## Docker

```sh
docker build -t packrat .
# Runs the interactive guide; set /library as the destination when prompted.
docker run --rm -it --device /dev/sr0 -v /mnt/media:/library packrat
```

Tagged releases are built for Linux (x86_64), macOS (x86_64/arm64) and Windows
(x86_64) by `.github/workflows/release.yml`.
