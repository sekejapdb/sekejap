#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
while [ ! -f "$R/current-reader-r1.exit" ]; do sleep 5; done
[ "$(cat "$R/current-reader-r1.exit")" = 0 ]
test ! -e "$R/query-r1-src"
cp -a "$R/current-reader-r1-src" "$R/query-r1-src"
tar -xzf "$R/query-r1-overlay.tar.gz" -C "$R/query-r1-src"
cd "$R/query-r1-src"
python3 - <<'INNER'
import hashlib,json
from pathlib import Path
files=sorted([*Path('src').rglob('*.rs'),*Path('kernel/src').rglob('*.rs'),*Path('tests').rglob('*.rs'),*Path('kernel/tests').rglob('*.rs'),Path('Cargo.toml'),Path('Cargo.lock'),Path('kernel/Cargo.toml')])
Path('../query-r1-source.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files},sort_keys=True,indent=2)+'\n')
for p in files:p.touch()
INNER
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline --test query_scalar --test index_scalar_oracle --test index_lifecycle --test graph_collections --test index_vector --test index_spatial --test index_text "${flags[@]}" > "$R/logs/query-r1-$mode.log" 2>&1
 printf 'QUERY_R1_PASS %s\n' "$mode"
done
