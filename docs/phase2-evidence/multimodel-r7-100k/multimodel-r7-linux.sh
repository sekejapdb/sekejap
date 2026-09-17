#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
while [ ! -f "$R/multimodel-r6.exit" ]; do sleep 5; done
[ "$(cat "$R/multimodel-r6.exit")" = 0 ]
test ! -e "$R/multimodel-r7-src"
cp -a "$R/multimodel-r6-src" "$R/multimodel-r7-src"
tar -xzf "$R/multimodel-r7-overlay.tar.gz" -C "$R/multimodel-r7-src"
cd "$R/multimodel-r7-src"
python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/multimodel-r7-source.json"
cargo build --release --locked --offline --features compact-cells,sqlite-balance,keyspace-append,slotref-split --bin phase2_multimodel_bench > "$R/logs/multimodel-r7-build.log" 2>&1
mkdir "$R/multimodel-r7-binaries" "$R/multimodel-smoke-r7"
cp "$R/target/release/phase2_multimodel_bench" "$R/multimodel-r7-binaries/bench"
sha256sum "$R/multimodel-r7-binaries/bench" > "$R/multimodel-r7-binaries/binary.sha256"
for specification in e4-atomic sqlite-atomic e4-resumable; do
 IFS=- read -r engine policy <<< "$specification"
 "$R/multimodel-r7-binaries/bench" "$engine" 10000 32 "$R/multimodel-smoke-r7/$specification" "$policy" batch > "$R/logs/multimodel-smoke-r7-$specification.log" 2>&1
done
python3 - "$R" <<'PY'
import json,pathlib,sys
r=pathlib.Path(sys.argv[1]); total=0
for p in sorted((r/'logs').glob('multimodel-smoke-r7-*.log')):
 d=json.loads(p.read_text().splitlines()[-1]); result=d['result']
 assert not result.get('refused',False),(p.name,result)
 assert result['reader_mode']=='batch'
 assert len(result['crud'])==3
 print(p.name,'completed all3rounds and oldsnapshot checks');total+=1
assert total==3
PY
printf '0\n' > "$R/multimodel-smoke-r7.exit"
for rows in 10000 100000; do
 python3 tools/phase2_multimodel_bench.py --bin "$R/multimodel-r7-binaries/bench" --root "$R/multimodel-r7-$rows" --sizes "$rows" --reader-modes none,batch,short,held --trials 3 --dimension 32 --include-resumable > "$R/logs/multimodel-r7-$rows.log" 2>&1
 python3 - "$R/multimodel-r7-$rows/report.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]));assert d['capture_complete'] and len(d['arms'])==36
for arm in d['arms']:
 result=arm['raw']['result'];assert arm['returncode']==0
 if result.get('refused',False):assert result['refusal_oracle']['committed_state_verified'] is True
print(d['completion_counts'])
PY
 printf '0\n' > "$R/multimodel-r7-$rows.exit"
done
