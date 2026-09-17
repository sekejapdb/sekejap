#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR E4_COMPAT_ENGINE_REVISION=a69838c-plus-phase2-lifecycle-r1-captured-source
# Never compile or qualify while the timed R7 comparison is active.
while [ ! -f "$R/multimodel-r7.exit" ]; do sleep 5; done
[ "$(cat "$R/phase2-workspace-r1.exit")" = 0 ]
test ! -e "$R/lifecycle-r1-src"
cp -a "$R/multimodel-r7-src" "$R/lifecycle-r1-src"
tar -xzf "$R/lifecycle-r1-overlay.tar.gz" -C "$R/lifecycle-r1-src"
cd "$R/lifecycle-r1-src"
python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/lifecycle-r1-source.json"
python3 - "$R" <<'PY'
import hashlib,json,pathlib,sys
r=pathlib.Path(sys.argv[1]); before=json.loads((r/'multimodel-r7-source.json').read_text()); now=json.loads((r/'lifecycle-r1-source.json').read_text())
assert all(now[p]==h for p,h in before.items()), 'R7 captured source changed'
assert set(now)-set(before)=={'src/bin/phase2_lifecycle_fixture.rs'}
overlay=json.loads((r/'lifecycle-r1-overlay.json').read_text())
for p,h in overlay.items(): assert hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()==h
PY
mkdir "$R/lifecycle-r1-binaries"
for mode in default retained; do
 features=()
 if [ "$mode" = retained ]; then features=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo build --release --locked --offline "${features[@]}" --bin phase2_lifecycle_fixture --bin format_compat > "$R/logs/lifecycle-r1-build-$mode.log" 2>&1
 cp "$R/target/release/phase2_lifecycle_fixture" "$R/lifecycle-r1-binaries/lifecycle-$mode"
 cp "$R/target/release/format_compat" "$R/lifecycle-r1-binaries/format-compat-$mode"
 python3 tools/format_reference_compat.py --corpus docs/format-v1-baseline --index-sha256 6bd933a1a63c6f2c2c3af0ac6f6e4d62c29c011a5ccdcbf66d32835fdb88ec26 --baseline-bin "$R/reference/bin/format_compat-baseline" --current-bin "$R/lifecycle-r1-binaries/format-compat-$mode" --work "$R/lifecycle-r1-phase1-$mode" > "$R/logs/lifecycle-r1-phase1-$mode.log" 2>&1
done
sha256sum "$R/lifecycle-r1-binaries/"* > "$R/lifecycle-r1-binaries.sha256"
python3 tools/phase2_lifecycle_compat.py --default-bin "$R/lifecycle-r1-binaries/lifecycle-default" --retained-bin "$R/lifecycle-r1-binaries/lifecycle-retained" --older-probe old-five "$R/quant-fixture-r1-binaries/old-five" 31 --work "$R/lifecycle-r1" > "$R/logs/lifecycle-r1-matrix.log" 2>&1
python3 - "$R" <<'PY'
import json,pathlib,sys
r=pathlib.Path(sys.argv[1])
for mode in ('default','retained'):
 d=json.loads((r/f'lifecycle-r1-phase1-{mode}/REPORT.json').read_text());assert d['result']=='PASS' and d['source_unchanged'];assert len(d['arms'])==10
d=json.loads((r/'lifecycle-r1/REPORT.json').read_text());assert d['result']=='PASS'
assert len(d['fixtures'])==80 and len(d['same_revision_cross_build_arms'])==160 and len(d['admission_arms'])==160
print('PASS:20 preserved Phase1 semantic arms;80 candidate fixtures;160 lifecycle semantic arms;160 older admission arms')
PY
