# Benchmarks

Benchmarks are from the development VM using:

- source: `/home/raftdev/maps/datasets/naturalvue-us/naturalvue-us-conus-3857-webp-q75.tif`
- input size: `15G`
- CRS: `EPSG:3857`
- raster size: `457054 x 222136`
- source layout: COG, `512x512` internal tiles, WebP compression, overviews
- bounds: `-126.0015,24.9985,-59.9985,50.0015`
- output format: WebP, `512px` tiles

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
rendering takes a long time. The current implementation renders a zoom into
memory by chunk, then writes tile data into the final archive; future benchmark
work should focus on streaming completed chunks into the archive sooner and
reducing per-chunk overhead.
