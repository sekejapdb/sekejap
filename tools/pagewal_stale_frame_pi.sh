#!/bin/bash
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/pagewal-correctness-20260913"
src="$task_root/pagewal-correctness-src"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home" CARGO_BUILD_JOBS=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp" CARGO_TARGET_DIR="$task_root/free-candidate-target"
mkdir "$src"
tar -xzf "$art/source.tar.gz" -C "$src"
mkdir -p "$src/.cargo" "$art/tmp"
printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$task_root" > "$src/.cargo/config.toml"
cd "$src"
cargo test --release --offline --features sqlite-balance,compact-cells --lib --test pagewal --test pagewal_repair --test btree_reuse -- --test-threads=1 > "$art/tests.log" 2>&1
cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin pagewal_bench > "$art/build.log" 2>&1
cp "$CARGO_TARGET_DIR/release/pagewal_bench" "$art/pagewal-v3"
cp "$task_root/artifacts/pagewal-qualification-20260913/pagewal_bench" "$art/pagewal-v2"
sha256sum "$art/source.tar.gz" "$art/pagewal-v2" "$art/pagewal-v3" > "$art/hashes.txt"
python3 "$art/run.py" "$art"
date -u > "$art/complete"
