#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
[ "$(cat "$R/text-r1.exit")" = 101 ]
test ! -e "$R/text-r2-src"
cp -a "$R/text-r1-src" "$R/text-r2-src"
tar -xzf "$R/text-r2-overlay.tar.gz" -C "$R/text-r2-src"
cd "$R/text-r2-src"
python3 - <<'PY'
import hashlib,json
from pathlib import Path
files=sorted([*Path('src').rglob('*.rs'),*Path('kernel/src').rglob('*.rs'),*Path('tests').rglob('*.rs'),*Path('kernel/tests').rglob('*.rs'),Path('Cargo.toml'),Path('Cargo.lock'),Path('kernel/Cargo.toml')])
Path('../text-r2-source.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files},sort_keys=True,indent=2)+'\n')
for p in files:p.touch()
PY
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline --test index_text --test text_admission --test index_spatial --test spatial_admission --test index_vector --test vector_admission --test index_lifecycle --test index_admission --test index_scalar_oracle --test graph_collections --test graph_admission "${flags[@]}" > "$R/logs/text-r2-$mode.log" 2>&1
 for filter in text_analyzer:: dense_v3::tests text_indexes::fault_tests:: text_indexes::validation_tests::; do
  cargo test --release --locked --offline --lib "$filter" "${flags[@]}" >> "$R/logs/text-r2-$mode.log" 2>&1
 done
 printf 'TEXT_R2_PASS %s\n' "$mode"
done
