#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
[ "$(cat "$R/vector-r1.exit")" = 0 ]
[ "$(cat "$R/spatial-r2.exit")" = 101 ]
test ! -e "$R/family-fault-r1-src"
cp -a "$R/spatial-r1-src" "$R/family-fault-r1-src"
tar -xzf "$R/family-fault-r1-overlay.tar.gz" -C "$R/family-fault-r1-src"
cd "$R/family-fault-r1-src"
python3 - <<'PY'
import hashlib,json
from pathlib import Path
files=sorted([*Path('src').rglob('*.rs'),*Path('kernel/src').rglob('*.rs'),*Path('tests').rglob('*.rs'),*Path('kernel/tests').rglob('*.rs'),Path('Cargo.toml'),Path('Cargo.lock'),Path('kernel/Cargo.toml')])
Path('../family-fault-r1-source.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files},sort_keys=True,indent=2)+'\n')
for p in files:p.touch()
PY
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline --test index_spatial --test spatial_admission --test index_vector --test vector_admission --test index_lifecycle --test index_admission --test index_scalar_oracle --test graph_collections --test graph_admission "${flags[@]}" > "$R/logs/family-fault-r1-$mode.log" 2>&1
 for filter in vector_indexes::fault_tests:: spatial_indexes::fault_tests:: spatial_math::; do
  cargo test --release --locked --offline --lib "$filter" "${flags[@]}" >> "$R/logs/family-fault-r1-$mode.log" 2>&1
 done
 printf 'FAMILY_FAULT_R1_PASS %s\n' "$mode"
done
