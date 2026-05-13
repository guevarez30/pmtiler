# Contributing

Thanks for helping improve `pmtiler`.

## Requirements

Install Rust, GDAL, libwebp, and pkg-config before building or testing.

Ubuntu/Debian:

```bash
sudo apt-get update
sudo apt-get install -y gdal-bin libgdal-dev libwebp-dev pkg-config
```

macOS:

```bash
brew install gdal webp
```

## Local Checks

Run these before opening a pull request:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

For raster behavior changes, also run the fixture checks:

```bash
tools/benchmark-blue-marble.sh
tools/quality-blue-marble.sh
```

## Large Test Data

Do not commit generated PMTiles archives, GeoTIFFs, VRTs, or downloaded raster
datasets. Put local data under `datasets/`; it is ignored by git.

When adding examples that need public data, document the download commands and
source attribution instead of vendoring the files.
