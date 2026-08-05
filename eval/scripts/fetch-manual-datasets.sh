#!/usr/bin/env bash
# Fetch the datasets whose direct links were verified externally.
# Idempotent; best-effort per dataset. Run from the eval/ directory.
set -uo pipefail
D=${SEKEJAP_DATA:-./data}/datasets
log(){ echo "[$(date -u +%H:%M:%S)] $*"; }

# tooling (ensure zstd/unzip/curl present)
if ! command -v zstd >/dev/null 2>&1 || ! command -v unzip >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq >/dev/null 2>&1 || true
  apt-get install -y -qq zstd unzip curl tar >/dev/null 2>&1 || true
fi

# ── NYC PostGIS workshop (spatial polygons) ──
if ls "$D"/nyc/*.shp >/dev/null 2>&1; then log "skip NYC (present)"; else
  log "NYC PostGIS workshop…"
  curl -fsSL "https://s3.amazonaws.com/s3.cleverelephant.ca/postgis-workshop-2020.zip" -o "$D/nyc/nyc.zip" \
    && unzip -o -q "$D/nyc/nyc.zip" -d "$D/nyc" && rm -f "$D/nyc/nyc.zip" \
    && log "NYC done" || log "NYC FAILED"
fi

# ── LDBC SNB SF1 (graph) ──
if [ -n "$(ls -A "$D/ldbc" 2>/dev/null)" ]; then log "skip LDBC (present)"; else
  log "LDBC SNB SF1…"
  curl -fsSL "https://swat.daanvdn.nl/ldbc/social_network-csv_basic-sf1.tar.zst" -o "$D/ldbc/ldbc.tar.zst" \
    && tar --use-compress-program=unzstd -xf "$D/ldbc/ldbc.tar.zst" -C "$D/ldbc" && rm -f "$D/ldbc/ldbc.tar.zst" \
    && log "LDBC done" || log "LDBC FAILED"
fi

# ── Cohere 1M embeddings (vector / filtered-ANN) ──
[ -e "$D/cohere/train.parquet" ] && log "skip Cohere train (present)" || {
  log "Cohere train.parquet (~3GB)…"
  curl -fsSL "https://assets.zilliz.com/benchmark/cohere_medium_1m/train.parquet" -o "$D/cohere/train.parquet" \
    && log "Cohere train done" || log "Cohere train FAILED"
}
[ -e "$D/cohere/test.parquet" ] && log "skip Cohere test (present)" || {
  log "Cohere test.parquet…"
  curl -fsSL "https://assets.zilliz.com/benchmark/cohere_medium_1m/test.parquet" -o "$D/cohere/test.parquet" \
    && log "Cohere test done" || log "Cohere test FAILED"
}

log "=== sizes ==="
du -sh "$D/nyc" "$D/ldbc" "$D/cohere" 2>/dev/null
log "fetch-manual-datasets.sh complete"
