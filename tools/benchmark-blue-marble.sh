#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/pmtiler"
INPUT="${INPUT:-$ROOT/datasets/blue_marble_demo.tif}"
ZOOM="${ZOOM:-0-5}"
TILE_SIZE="${TILE_SIZE:-512}"
FORMAT="${FORMAT:-webp}"
QUALITY="${QUALITY:-100}"
WEBP_METHOD="${WEBP_METHOD:-4}"
OUT_DIR="${OUT_DIR:-$(mktemp -d /tmp/pmtiler-bench.XXXXXX)}"
CHUNKS="${CHUNKS:-auto 4 8}"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
mkdir -p "$OUT_DIR"

printf 'input=%s\nzoom=%s\nout_dir=%s\n\n' "$INPUT" "$ZOOM" "$OUT_DIR"

for chunk in $CHUNKS; do
  out="$OUT_DIR/chunk-$chunk.pmtiles"
  started="$(date +%s)"
  "$BIN" raster "$INPUT" "$out" \
    --zoom "$ZOOM" \
    --chunk-tiles "$chunk" \
    --format "$FORMAT" \
    --tile-size "$TILE_SIZE" \
    --quality "$QUALITY" \
    --webp-method "$WEBP_METHOD" \
    "$@"
  ended="$(date +%s)"
  bytes="$(wc -c < "$out" | tr -d ' ')"
  printf 'chunk=%s elapsed=%ss bytes=%s output=%s\n\n' "$chunk" "$((ended - started))" "$bytes" "$out"
done
