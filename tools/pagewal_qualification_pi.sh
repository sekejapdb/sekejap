#!/bin/bash
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/pagewal-qualification-20260913"
src="$task_root/pagewal-qualification-src"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home" CARGO_BUILD_JOBS=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp" CARGO_TARGET_DIR="$task_root/free-candidate-target"
mkdir "$src"
tar -xzf "$art/source.tar.gz" -C "$src"
mkdir -p "$src/.cargo" "$art/tmp"
printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$task_root" > "$src/.cargo/config.toml"
cd "$src"
cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin pagewal_bench --bin pagewal_cap > "$art/build.log" 2>&1
cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin pagewal_repair >> "$art/build.log" 2>&1
for name in pagewal_bench pagewal_cap pagewal_repair; do cp "$CARGO_TARGET_DIR/release/$name" "$art/$name"; done
cargo test --release --offline --features sqlite-balance,compact-cells --lib --test pagewal --test pagewal_repair -- --test-threads=1 > "$art/tests.log" 2>&1
cp "$task_root/artifacts/pagewal-20260913/pilot" "$art/control"
sha256sum "$art/source.tar.gz" "$art/pagewal_bench" "$art/pagewal_cap" "$art/pagewal_repair" "$art/control" > "$art/hashes.txt"
python3 "$art/run.py" "$art" bench
python3 "$art/run.py" "$art" cap
date -u > "$art/complete"
