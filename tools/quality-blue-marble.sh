#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/pmtiler"
INPUT="${INPUT:-$ROOT/datasets/blue_marble_demo.tif}"
ZOOM="${ZOOM:-0-5}"
BASELINE_CHUNK="${BASELINE_CHUNK:-4}"
CANDIDATE_CHUNK="${CANDIDATE_CHUNK:-auto}"
OUT_DIR="${OUT_DIR:-$(mktemp -d /tmp/pmtiler-quality.XXXXXX)}"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
mkdir -p "$OUT_DIR/baseline" "$OUT_DIR/candidate"

baseline="$OUT_DIR/baseline/out.pmtiles"
candidate="$OUT_DIR/candidate/out.pmtiles"

"$BIN" raster "$INPUT" "$baseline" --zoom "$ZOOM" --chunk-tiles "$BASELINE_CHUNK" "$@"
"$BIN" raster "$INPUT" "$candidate" --zoom "$ZOOM" --chunk-tiles "$CANDIDATE_CHUNK" "$@"

sha256sum "$baseline" "$candidate"

if ! cmp -s "$baseline" "$candidate"; then
  printf 'quality check failed: archives differ\nbaseline=%s\ncandidate=%s\n' "$baseline" "$candidate" >&2
  exit 1
fi

printf 'quality check passed: byte-identical archives\nbaseline=%s\ncandidate=%s\n' "$baseline" "$candidate"
