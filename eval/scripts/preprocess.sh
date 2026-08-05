#!/usr/bin/env bash
# Normalize raw datasets -> canonical loadable files under $SEKEJAP_DATA/prepared/<class>/.
# Best-effort per step (logs + continues). Run from the eval/ directory.
set -uo pipefail
DUCK=${DUCKDB_BIN:-duckdb}
D=${SEKEJAP_DATA:-./data}/datasets
P=${SEKEJAP_DATA:-./data}/prepared
mkdir -p "$P"/{relational,spatial,graph,vector,search}
MAN="$P/MANIFEST.txt"; : > "$MAN"
log(){ echo "[$(date -u +%H:%M:%S)] $*"; }
man(){ echo "$*" | tee -a "$MAN"; }

# ─────────── RELATIONAL ───────────
log "RELATIONAL: TPC-H row counts"
for t in region nation supplier customer part partsupp orders lineitem; do
  f="$D/tpch-sf1/$t.csv"; [ -f "$f" ] && man "tpch/$t.csv rows=$(( $(wc -l < "$f") - 1 ))"
done
[ -f "$D/tpch-sf1/schema.sql" ] && { cp "$D/tpch-sf1/schema.sql" "$P/relational/tpch_schema.sql"; man "tpch schema.sql -> prepared/relational/tpch_schema.sql"; }
log "RELATIONAL: ClickBench parquet shape + DDL"
ln -sf "$D/clickbench/hits_10m.parquet" "$P/relational/hits_10m.parquet"
$DUCK -c "SELECT count(*) FROM read_parquet('$D/clickbench/hits_10m.parquet');" 2>/dev/null | tail -1 | xargs -I{} echo "clickbench/hits_10m.parquet rows={}" | tee -a "$MAN"
$DUCK -c "DESCRIBE SELECT * FROM read_parquet('$D/clickbench/hits_10m.parquet');" 2>/dev/null | wc -l | xargs -I{} echo "clickbench columns~={}" | tee -a "$MAN"

# ─────────── SPATIAL ───────────
log "SPATIAL: GeoNames -> 2M-point parquet"
$DUCK -c "COPY (
  SELECT column0::BIGINT AS geonameid, column1 AS name, column4::DOUBLE AS lat, column5::DOUBLE AS lon,
         column6 AS fclass, column7 AS fcode, column8 AS country, TRY_CAST(column14 AS BIGINT) AS population
  FROM read_csv('$D/geonames/allCountries.txt', delim='\t', header=false, quote='', all_varchar=true, ignore_errors=true)
  LIMIT 2000000
) TO '$P/spatial/geonames.parquet' (FORMAT PARQUET);" 2>&1 | tail -2
[ -f "$P/spatial/geonames.parquet" ] && man "spatial/geonames.parquet rows=$($DUCK -c "SELECT count(*) FROM '$P/spatial/geonames.parquet';" 2>/dev/null | tail -1)"

log "SPATIAL: NYC shapefiles -> WKT parquet"
for shp in $(find "$D/nyc" -name '*.shp' 2>/dev/null); do
  base=$(basename "$shp" .shp)
  $DUCK -c "INSTALL spatial; LOAD spatial;
    COPY (SELECT * EXCLUDE geom, ST_AsText(geom) AS wkt FROM ST_Read('$shp'))
    TO '$P/spatial/$base.parquet' (FORMAT PARQUET);" 2>&1 | tail -1
  [ -f "$P/spatial/$base.parquet" ] && man "spatial/$base.parquet rows=$($DUCK -c "SELECT count(*) FROM '$P/spatial/$base.parquet';" 2>/dev/null | tail -1)"
done

# ─────────── GRAPH ───────────
log "GRAPH: LDBC persons + knows"
LDBCD="$D/ldbc/social_network-csv_basic-sf1/dynamic"
if [ -f "$LDBCD/person_0_0.csv" ]; then
  $DUCK -c "COPY (SELECT * FROM read_csv('$LDBCD/person_0_0.csv', delim='|', header=true, all_varchar=true)) TO '$P/graph/ldbc_person.csv' (HEADER);" 2>&1 | tail -1
  $DUCK -c "COPY (SELECT * FROM read_csv('$LDBCD/person_knows_person_0_0.csv', delim='|', header=true, all_varchar=true)) TO '$P/graph/ldbc_knows.csv' (HEADER);" 2>&1 | tail -1
  man "graph/ldbc_person.csv rows=$(( $(wc -l < "$P/graph/ldbc_person.csv") - 1 ))"
  man "graph/ldbc_knows.csv rows=$(( $(wc -l < "$P/graph/ldbc_knows.csv") - 1 ))"
fi
log "GRAPH: SNAP com-amazon edges"
if [ -f "$D/snap/com-amazon.ungraph.txt" ]; then
  { echo "src,dst"; grep -v '^#' "$D/snap/com-amazon.ungraph.txt" | tr '\t' ','; } > "$P/graph/snap_amazon_edges.csv"
  man "graph/snap_amazon_edges.csv rows=$(( $(wc -l < "$P/graph/snap_amazon_edges.csv") - 1 ))"
fi

# ─────────── VECTOR ───────────
log "VECTOR: SIFT/GloVe HDF5 -> parquet (base/queries/groundtruth)"
python3 - <<'PY' 2>&1 | tail -12
import h5py, numpy as np, pyarrow as pa, pyarrow.parquet as pq
import os
B=os.environ.get('SEKEJAP_DATA','./data')
jobs=[('sift',B+'/datasets/ann/sift-128-euclidean.hdf5'),
      ('glove',B+'/datasets/ann/glove-100-angular.hdf5')]
OUT=B+'/prepared/vector'
def vec_table(ids, arr):
    dim=arr.shape[1]
    flat=pa.array(np.ascontiguousarray(arr, dtype='float32').reshape(-1))
    return pa.table({'id': pa.array(ids.astype('int64')),
                     'vector': pa.FixedSizeListArray.from_arrays(flat, dim)})
for name,f in jobs:
    try:
        h=h5py.File(f,'r'); tr=np.array(h['train']); te=np.array(h['test']); gt=np.array(h['neighbors'])
        pq.write_table(vec_table(np.arange(tr.shape[0]), tr), f'{OUT}/{name}_base.parquet')
        pq.write_table(vec_table(np.arange(te.shape[0]), te), f'{OUT}/{name}_queries.parquet')
        gtt=pa.table({'query_id': pa.array(np.arange(gt.shape[0]),type=pa.int64()),
                      'neighbors': pa.array([r.tolist() for r in gt.astype('int32')], type=pa.list_(pa.int32()))})
        pq.write_table(gtt, f'{OUT}/{name}_groundtruth.parquet')
        print(f'{name}: base {tr.shape} queries {te.shape} gt {gt.shape}')
        h.close()
    except Exception as e:
        print(f'{name} FAILED: {e}')
PY
for n in sift glove; do [ -f "$P/vector/${n}_base.parquet" ] && man "vector/${n}_base.parquet rows=$($DUCK -c "SELECT count(*) FROM '$P/vector/${n}_base.parquet';" 2>/dev/null | tail -1)"; done
log "VECTOR: Cohere schema"
$DUCK -c "DESCRIBE SELECT * FROM read_parquet('$D/cohere/train.parquet');" 2>&1 | tail -8 | tee -a "$MAN"
$DUCK -c "SELECT count(*) FROM read_parquet('$D/cohere/train.parquet');" 2>/dev/null | tail -1 | xargs -I{} echo "cohere/train.parquet rows={}" | tee -a "$MAN"

# ─────────── SEARCH ───────────
log "SEARCH: BEIR counts"
for ds in scifact nfcorpus fiqa; do
  c="$D/beir/$ds/corpus.jsonl"; q="$D/beir/$ds/queries.jsonl"
  [ -f "$c" ] && man "beir/$ds corpus=$(wc -l < "$c") queries=$(wc -l < "$q")"
done

log "=== PREPARED TREE ==="; find "$P" -type f | sort | tee -a "$MAN"
log "preprocess complete"
