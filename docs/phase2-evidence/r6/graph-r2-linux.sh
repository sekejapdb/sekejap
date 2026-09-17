#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch> SQLITE_TMPDIR=<scratch>
cd "$R/src"
cargo test --release --locked --offline --test graph_admission --test graph_collections --lib graph_write_boundaries -- --test-threads=1 > "$R/logs/graph-fault-r2.log" 2>&1
cargo test --release --locked --offline --workspace -- --test-threads=1 > "$R/logs/suite-graph-default-r2.log" 2>&1
printf 'GRAPH_DEFAULT_SUITE_PASS\n'
cargo test --release --locked --offline --workspace --features compact-cells,sqlite-balance -- --test-threads=1 > "$R/logs/suite-graph-compact-r2.log" 2>&1
printf 'GRAPH_COMPACT_SUITE_PASS\n'
cargo test --release --locked --offline --workspace --features compact-cells,sqlite-balance,keyspace-append,slotref-split -- --test-threads=1 > "$R/logs/suite-graph-retained-r2.log" 2>&1
printf 'GRAPH_RETAINED_SUITE_PASS\n'
