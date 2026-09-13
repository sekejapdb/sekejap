#!/bin/bash
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/pagewal-qualification-20260913"
src="$task_root/pagewal-qualification-src"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home" CARGO_BUILD_JOBS=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp" CARGO_TARGET_DIR="$task_root/free-candidate-target"
test -f "$art/confirm-results.json"
cp "$art/short-cap.rs" "$src/src/bin/pagewal_cap.rs"
cd "$src"
cargo build --release --offline --features sqlite-balance,compact-cells --bin pagewal_cap > "$art/short-cap-build.log" 2>&1
cp "$CARGO_TARGET_DIR/release/pagewal_cap" "$art/pagewal_cap_short"
sha256sum "$art/short-cap.rs" "$art/pagewal_cap_short" > "$art/short-cap-hashes.txt"
python3 "$art/run-confirm.py" "$art" cap-short
python3 "$task_root/packing_time.py" prlimit --as=134217728 -- "$art/pagewal_repair" "$art/matrix/mixed-100k-r3/pagewal-v2/db" "$art/repair-100k" 1048576 > "$art/repair-100k.log" 2>&1
date -u > "$art/extra.complete"
