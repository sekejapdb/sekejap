#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export E4_COMPAT_ENGINE_REVISION=59d1cbc770284f160ffda53cc1ee545167733d11
run() { local name=$1; shift; echo "STAGE=$name"; if "$@" > "$R/logs/$name.log" 2>&1; then tail -8 "$R/logs/$name.log"; else tail -60 "$R/logs/$name.log"; return 1; fi; }
echo 'acb36ef2a06573e7c61224534a2fb98084d0f992ac6f116c1469e62622a4bd71  <scratch>' | sha256sum -c -
tar -xzf "$R/harness.tar.gz" -C "$R/baseline-src"
cd "$R/baseline-src"
test -z "$(git diff HEAD -- src/pagewal.rs src/pagewal kernel/src src/collections.rs Cargo.toml Cargo.lock)"
cp "$CARGO_TARGET_DIR/release/pagewal_repair" "$R/bin/pagewal_repair-59d1cbc"
run build-baseline cargo build --release --locked --offline --bin format_compat
cp "$CARGO_TARGET_DIR/release/format_compat" "$R/bin/format_compat-baseline"
# Tracked engine sources are identical to59d1cbc; a separate source directory
# and feature build distinguish the current side. Future runs substitute HEAD.
cp -a "$R/baseline-src" "$R/current-src"
cd "$R/current-src"
run build-compact cargo build --release --locked --offline --bin format_compat --features compact-cells,sqlite-balance
cp "$CARGO_TARGET_DIR/release/format_compat" "$R/bin/format_compat-compact"
run build-retained cargo build --release --locked --offline --bin format_compat --features compact-cells,sqlite-balance,keyspace-append,slotref-split
cp "$CARGO_TARGET_DIR/release/format_compat" "$R/bin/format_compat-retained"
INDEX_SHA=$(sha256sum "$R/corpus/INDEX.json" | cut -d ' ' -f1)
for mode in compact retained; do
 run "compat-$mode" python3 tools/format_reference_compat.py --corpus "$R/corpus" --index-sha256 "$INDEX_SHA" --baseline-bin "$R/bin/format_compat-baseline" --current-bin "$R/bin/format_compat-$mode" --work "$R/compat-$mode"
done
mkdir "$R/repair-copy"
cp -a "$R/corpus/compact-off-wal-pending/." "$R/repair-copy/"
python3 - "$R" <<'PY'
import sys,json,subprocess
from pathlib import Path
sys.path.insert(0,str(Path.cwd()/'tools'))
from format_reference_compat import inventory
r=Path(sys.argv[1]); before=inventory(r/'repair-copy')
p=subprocess.run([str(r/'bin/pagewal_repair-59d1cbc'),str(r/'repair-copy'),str(r/'repair-output'),'16777216'],capture_output=True,text=True)
(r/'logs/repair-cli.log').write_text(p.stdout+p.stderr)
assert p.returncode==0,p.stderr
report=json.loads(p.stdout); assert (r/'repair-output/COMPLETE.json').is_file(); assert before==inventory(r/'repair-copy')
(r/'repair-smoke.json').write_text(json.dumps({'result':'PASS','exit_code':p.returncode,'source_unchanged':True,'source_inventory':before,'report':report},indent=2))
print('REPAIR_CLI_PASS')
PY
mkdir -p docs/format-v1-baseline
cp -a "$R/corpus/." docs/format-v1-baseline/
echo READY_FOR_PINNED_TESTS
while [ ! -f "$R/format_v1_compat.rs.ready" ]; do sleep 5; done
cp "$R/format_v1_compat.rs.ready" tests/format_v1_compat.rs
for mode in default compact retained; do
 args=(); if [ "$mode" = compact ]; then args=(--features compact-cells,sqlite-balance); fi
 if [ "$mode" = retained ]; then args=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 run "fixtures-$mode" cargo test --release --locked --offline "${args[@]}" --test format_v1_compat -- --test-threads=1
done
sha256sum "$R"/bin/* > "$R/binaries.sha256"
tar -czf "$R/reference-artifacts.tar.gz" -C "$R" bin corpus harness.tar.gz binaries.sha256
mkdir "$R/reports"
cp "$R/compat-compact/REPORT.json" "$R/reports/compact.json"
cp "$R/compat-retained/REPORT.json" "$R/reports/retained.json"
cp "$R/current-src/tests/format_v1_compat.rs" "$R/reports/format_v1_compat.rs"
tar -czf "$R/reference-evidence.tar.gz" -C "$R" logs reports repair-smoke.json binaries.sha256 repair-output/COMPLETE.json
sha256sum "$R/reference-artifacts.tar.gz" "$R/reference-evidence.tar.gz"
echo REFERENCE_QUALIFIED
