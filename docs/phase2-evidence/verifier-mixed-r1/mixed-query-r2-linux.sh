#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
while [ ! -f "$R/verifier-r1.exit" ]; do sleep 5; done
test ! -e "$R/mixed-query-r2-src"
cp -a "$R/mixed-query-r1-src" "$R/mixed-query-r2-src"
tar -xzf "$R/mixed-query-r2-overlay.tar.gz" -C "$R/mixed-query-r2-src"
cd "$R/mixed-query-r2-src"
python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/mixed-query-r2-source.json"
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline --test query_multimodel --test query_scalar --test index_scalar_oracle --test index_lifecycle --test graph_collections --test index_vector --test index_spatial --test index_text "${flags[@]}" > "$R/logs/mixed-query-r2-$mode.log" 2>&1
 cargo run --release --locked --offline --example query_multimodel "${flags[@]}" -- "$TMPDIR/mixed-query-r2-example-$mode" > "$R/logs/mixed-query-r2-example-$mode.log" 2>&1
 printf 'MIXED_QUERY_R2_PASS %s\n' "$mode"
done
