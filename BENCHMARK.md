# Benchmarks

Benchmarks are from the development VM using:

- source: `/home/raftdev/maps/datasets/naturalvue-us/naturalvue-us-conus-3857-webp-q75.tif`
- input size: `15G`
- CRS: `EPSG:3857`
- raster size: `457054 x 222136`
- source layout: COG, `512x512` internal tiles, WebP compression, overviews
- bounds: `-126.0015,24.9985,-59.9985,50.0015`
- output format: WebP, `512px` tiles

## Blue Marble Demo z0-6

Command:

```bash
target/release/pmtiler raster datasets/blue_marble_demo.tif /tmp/blue-marble-bench-default.pmtiles --zoom 0-6
```

Results after native chunk buffers, direct libwebp encoding, direct archive
streaming, and default `--chunk-tiles 8`:

```text
Tiles:              5,461
Elapsed:            65.2 seconds
Throughput:         83.8 tiles/s
Output PMTiles:     53.7 MiB
Zoom range:         0..6
Strategy:           EPSG:4326 fast path
Chunk size:         8x8 tiles
```

Equivalent wide-render comparison before default chunking was enabled:

```text
Command:            target/release/pmtiler raster datasets/blue_marble_demo.tif /tmp/blue-marble-bench.pmtiles --zoom 0-6
Elapsed:            115.1 seconds
Throughput:         47.4 tiles/s
z6 elapsed:         84.1 seconds
```

## NaturalVue CONUS z0-10

Command:

```bash
target/release/pmtiler raster \
  /home/raftdev/maps/datasets/naturalvue-us/naturalvue-us-conus-3857-webp-q75.tif \
  /home/raftdev/raft-tech/df-go-pmtiles/data/naturalvue-us-conus-z10.pmtiles \
  --zoom 0-10 \
  --bounds=-126.0015,24.9985,-59.9985,50.0015 \
  --format webp \
  --tile-size 512 \
  --workers 8 \
  --chunk-tiles 16 \
  --warp-memory 4G \
  --warp-threads all
```

Results:

```text
Tiles:              23,428
Elapsed:            407.1 seconds
Output PMTiles:     1019M
Zoom range:         0..10
```

## NaturalVue CONUS z11

Command:

```bash
target/release/pmtiler raster \
  /home/raftdev/maps/datasets/naturalvue-us/naturalvue-us-conus-3857-webp-q75.tif \
  /home/raftdev/raft-tech/df-go-pmtiles/data/naturalvue-us-conus-z11-benchmark.pmtiles \
  --zoom 11 \
  --bounds=-126.0015,24.9985,-59.9985,50.0015 \
  --format webp \
  --tile-size 512 \
  --workers 8 \
  --chunk-tiles 4 \
  --strategy auto
```

Results:

```text
Tiles:              69,184
Elapsed:            1117.8 seconds (18m 37.8s)
Output PMTiles:     3.2G
Zoom range:         11..11
Strategy:           same-crs-webmercator
Workers:            8
Chunk size:         4x4 tiles
Peak observed RSS:  ~7.3G near completion
```

Inspect summary:

```text
version:            3
tile type:          webp
zoom:               11..11
addressed tiles:    69,184
tile entries:       69,184
tile contents:      69,184
tile data:          3411987210 bytes
```

## Notes

The z11 run was stable with `--workers 8 --chunk-tiles 4`. Larger chunks reduce
chunk overhead but increase per-worker memory and can hide progress if chunk
rendering takes a long time.

## NaturalVue CONUS z0-11

Command:

```bash
target/release/pmtiler raster \
  /home/raftdev/maps/datasets/naturalvue-us/naturalvue-us-conus-3857-webp-q75.tif \
  /home/raftdev/raft-tech/df-go-pmtiles/data/naturalvue-us-conus-z0-z11.pmtiles \
  --zoom 0-11 \
  --bounds=-126.0015,24.9985,-59.9985,50.0015 \
  --format webp \
  --tile-size 512 \
  --workers 8 \
  --chunk-tiles 8 \
  --strategy auto
```

Results:

```text
Tiles:              92,612
Elapsed:            1514.7 seconds (25m 14.7s)
Output PMTiles:     4.2G
Zoom range:         0..11
Strategy:           same-crs-webmercator
Workers:            8
Chunk size:         8x8 tiles
Peak observed RSS:  ~8.2G during final render/assembly
```

Inspect summary:

```text
version:            3
tile type:          webp
zoom:               0..11
addressed tiles:    92,612
tile entries:       92,612
tile contents:      92,612
tile data:          4.2 GiB
root:               168 B @ 127 B
metadata:           343 B @ 295 B
leaves:             513.0 KiB @ 638 B
```

## Notes

The raster path now streams encoded tile bytes into the final archive while
reserving directory space up front. The same-CRS and EPSG:4326 fast paths keep
chunk pixels in native interleaved buffers, then encode WebP through libwebp
instead of creating one GDAL MEM dataset and `/vsimem` file per tile.
