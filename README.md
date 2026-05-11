# pmtiler

`pmtiler` creates PMTiles archives from raster map tiles and GDAL-readable
raster sources.

Use it to:

- pack an existing XYZ tile directory into a `.pmtiles` file
- render a GeoTIFF, COG, or VRT mosaic into raster PMTiles
- inspect PMTiles header, metadata, and archive layout

## Requirements

`pmtiler raster` uses GDAL. Install GDAL before building or running raster
commands.

Ubuntu/Debian:

```bash
sudo apt-get install -y gdal-bin libgdal-dev pkg-config
```

macOS:

```bash
brew install gdal
```

Build from source:

```bash
cargo build --release
```

Run the binary:

```bash
target/release/pmtiler --help
```

## Commands

```text
pmtiler pack [OPTIONS] <TILE_DIR> <OUTPUT.pmtiles>
pmtiler raster <RASTER_OR_VRT> <OUTPUT.pmtiles> --zoom MIN-MAX --bounds W,S,E,N [OPTIONS]
pmtiler inspect <ARCHIVE.pmtiles>
```

## Pack A Tile Directory

`pmtiler pack` reads an XYZ tile directory and writes a PMTiles v3 archive.

Supported layouts:

```text
<TILE_DIR>/<z>/<x>/<y>.webp
<TILE_DIR>/<z>/<x>/<y>.png
<TILE_DIR>/<z>/<x>/<y>.jpg
```

Example:

```bash
pmtiler pack \
  --name BlueMarble \
  --description "Blue Marble raster tiles" \
  --tile-size 512 \
  datasets/blue_marble_tiles \
  datasets/blue_marble.pmtiles
```

Options:

```text
--name <text>          Metadata name
--description <text>   Metadata description
--attribution <text>   Metadata attribution
--tile-size <px>       Tile size metadata value [default: 512]
```

## Render A Raster

`pmtiler raster` renders a GDAL-readable raster source into Web Mercator raster
tiles and writes them into a PMTiles archive.

Inputs can be files such as:

- GeoTIFF
- Cloud Optimized GeoTIFF
- VRT mosaics
- other raster formats supported by the installed GDAL build

Example:

```bash
pmtiler raster datasets/blue_marble_demo.tif datasets/blue_marble.pmtiles \
  --zoom 0-5 \
  --bounds=-180,-85.05112878,180,85.05112878 \
  --format webp \
  --tile-size 512
```

Plan a render without writing tiles:

```bash
pmtiler raster mosaic.vrt output.pmtiles \
  --zoom 0-13 \
  --bounds=-125,24,-66,50 \
  --format webp \
  --tile-size 512 \
  --plan
```

Raster options:

```text
--plan                 Print the tile job plan without rendering
--zoom <z|min-max>     Zoom or zoom range, for example 0-13
--bounds <w,s,e,n>     Lon/lat bounds
--format <fmt>         png, jpeg, jpg, or webp [default: webp]
--tile-size <px>       Output tile size [default: 512]
--workers <n>          Native render workers [default: host parallelism]
--chunk-tiles <n>      Experimental chunk width/height in tiles [default: disabled]
--warp-memory <size>   GDAL warp memory, suffix K/M/G allowed [default: 512M]
--warp-threads <n|all> GDAL warp compute threads [default: all]
--resampling <method>  nearest, bilinear, cubic, cubicspline, lanczos, average [default: bilinear]
--strategy <strategy>  auto, same-crs, geographic, or gdal-warp [default: auto]
--warp-option <K=V>    Extra GDAL warp option, repeatable
```

## Performance Example

NaturalVue CONUS COG rendered on the development VM:

```bash
pmtiler raster naturalvue-us-conus-3857-webp-q75.tif naturalvue-us-conus-z10.pmtiles \
  --zoom 0-10 \
  --bounds=-126.0015,24.9985,-59.9985,50.0015 \
  --format webp \
  --tile-size 512 \
  --workers 8 \
  --chunk-tiles 16
```

Results:

```text
Input GeoTIFF:     15G
Input CRS:         EPSG:3857
Input layout:      COG, 512x512 internal tiles, WebP compression, overviews
Output PMTiles:    1019M
Zoom range:        0..10
Tile count:        23,428
Elapsed time:      407.1 seconds
Output format:     WebP, 512px tiles
```

## Build A VRT From TIFFs

For a directory of GeoTIFFs, build a VRT first and pass the VRT to `pmtiler`.

```bash
gdalbuildvrt mosaic.vrt /path/to/tiffs/*.tif

pmtiler raster mosaic.vrt output.pmtiles \
  --zoom 0-13 \
  --bounds=-125,24,-66,50 \
  --format webp \
  --tile-size 512
```

For large raster collections, tiled GeoTIFFs with overviews are recommended:

```bash
gdal_translate input.tif output.tif \
  -of GTiff \
  -co TILED=YES \
  -co BLOCKXSIZE=512 \
  -co BLOCKYSIZE=512 \
  -co COMPRESS=JPEG \
  -co JPEG_QUALITY=85 \
  -co PHOTOMETRIC=YCBCR

gdaladdo -r average output.tif 2 4 8 16 32 64 128 256
```

## Inspect PMTiles

Use `inspect` to print archive metadata and section offsets.

```bash
pmtiler inspect datasets/blue_marble.pmtiles
```

Example output:

```text
file: datasets/blue_marble.pmtiles
version: 3
tile type: webp
zoom: 0..5
bounds: [-180.0000000, -85.0511288, 180.0000000, 85.0511288]
addressed tiles: 1365
tile entries: 1365
tile contents: 1365
```

## Serving PMTiles

The output `.pmtiles` file is a single portable archive. Place it in the data
directory used by a PMTiles server or static file host.

Example:

```bash
pmtiler raster mosaic.vrt /srv/pmtiles/conus.pmtiles \
  --zoom 0-13 \
  --bounds=-125,24,-66,50 \
  --format webp \
  --tile-size 512
```
