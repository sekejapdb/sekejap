#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
while [ ! -f "$R/text-r2.exit" ]; do sleep 5; done
[ "$(cat "$R/text-r2.exit")" = 0 ]
test ! -e "$R/current-reader-r1-src"
cp -a "$R/text-r2-src" "$R/current-reader-r1-src"
tar -xzf "$R/current-reader-r1-overlay.tar.gz" -C "$R/current-reader-r1-src"
cd "$R/current-reader-r1-src"
python3 - <<'PY'
import hashlib,json
from pathlib import Path
files=sorted([*Path('src').rglob('*.rs'),*Path('kernel/src').rglob('*.rs'),*Path('tests').rglob('*.rs'),*Path('kernel/tests').rglob('*.rs'),Path('Cargo.toml'),Path('Cargo.lock'),Path('kernel/Cargo.toml')])
Path('../current-reader-r1-source.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files},sort_keys=True,indent=2)+'\n')
for p in files:p.touch()
PY
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline --lib pagewal::current_reader:: "${flags[@]}" > "$R/logs/current-reader-r1-$mode.log" 2>&1
 printf 'CURRENT_READER_R1_PASS %s\n' "$mode"
done
