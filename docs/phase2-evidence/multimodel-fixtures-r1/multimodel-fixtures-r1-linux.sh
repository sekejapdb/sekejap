#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
while [ ! -f "$R/query-r1.exit" ]; do sleep 5; done
[ "$(cat "$R/query-r1.exit")" = 0 ]
test ! -e "$R/multimodel-fixtures-r1-src"
cp -a "$R/query-r1-src" "$R/multimodel-fixtures-r1-src"
tar -xzf "$R/multimodel-fixtures-r1-overlay.tar.gz" -C "$R/multimodel-fixtures-r1-src"
mkdir "$R/multimodel-fixtures-r1-binaries"
cat > "$R/multimodel-fixtures-r1-inventory.py" <<'INNER'
import hashlib,json,sys
from pathlib import Path
files=sorted([*Path('src').rglob('*.rs'),*Path('kernel/src').rglob('*.rs'),*Path('tests').rglob('*.rs'),*Path('kernel/tests').rglob('*.rs'),Path('Cargo.toml'),Path('Cargo.lock'),Path('kernel/Cargo.toml')])
Path(sys.argv[1]).write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files},sort_keys=True,indent=2)+'\n')
for p in files:p.touch()
INNER
cd "$R/multimodel-fixtures-r1-src"
python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/multimodel-fixtures-r1-source.json"
export E4_COMPAT_ENGINE_REVISION=phase2-query-r1-plus-fixture-harness
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo build --release --locked --offline --bin multimodel_format_fixture "${flags[@]}" > "$R/logs/multimodel-fixtures-r1-build-$mode.log" 2>&1
 cp "$CARGO_TARGET_DIR/release/multimodel_format_fixture" "$R/multimodel-fixtures-r1-binaries/$mode"
done
for label in phase1 graph vector; do
 case "$label" in
 phase1) original=$R/frozen-source;;
 graph) original=$R/src;;
 vector) original=$R/vector-r1-src;;
 esac
 copied=$R/multimodel-fixtures-r1-old-$label
 test ! -e "$copied"
 cp -a "$original" "$copied"
 cd "$copied"
 python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/multimodel-fixtures-r1-old-$label-original.json"
 cp "$R/multimodel-fixtures-r1-src/src/bin/typed_admission_probe.rs" src/bin/
 python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/multimodel-fixtures-r1-old-$label-source.json"
 E4_COMPAT_ENGINE_REVISION=$label-preserved-source-with-probe cargo build --release --locked --offline --bin typed_admission_probe > "$R/logs/multimodel-fixtures-r1-build-old-$label.log" 2>&1
 cp "$CARGO_TARGET_DIR/release/typed_admission_probe" "$R/multimodel-fixtures-r1-binaries/old-$label"
done
cd "$R/multimodel-fixtures-r1-src"
python3 tools/multimodel_format_compat.py --default-bin "$R/multimodel-fixtures-r1-binaries/default" --retained-bin "$R/multimodel-fixtures-r1-binaries/retained" --work "$R/multimodel-fixtures-r1" --older-probe phase1 "$R/multimodel-fixtures-r1-binaries/old-phase1" 1 --older-probe graph "$R/multimodel-fixtures-r1-binaries/old-graph" 3 --older-probe vector "$R/multimodel-fixtures-r1-binaries/old-vector" 7
