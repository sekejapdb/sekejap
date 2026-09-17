#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
[ "$(cat "$R/multimodel-fixtures-r1.exit")" = 0 ]
test ! -e "$R/crash-r1-src"
cp -a "$R/query-r1-src" "$R/crash-r1-src"
tar -xzf "$R/crash-r1-overlay.tar.gz" -C "$R/crash-r1-src"
mkdir "$R/crash-r1-binaries"
cd "$R/crash-r1-src"
python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/crash-r1-source.json"
export E4_COMPAT_ENGINE_REVISION=phase2-query-r1-plus-crash-harness
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo build --release --locked --offline --bin phase2_crash_probe "${flags[@]}" > "$R/logs/crash-r1-build-$mode.log" 2>&1
 cp "$CARGO_TARGET_DIR/release/phase2_crash_probe" "$R/crash-r1-binaries/$mode"
done
python3 tools/phase2_crash_qualify.py --default-bin "$R/crash-r1-binaries/default" --retained-bin "$R/crash-r1-binaries/retained" --work-dir "$R/crash-r1"
