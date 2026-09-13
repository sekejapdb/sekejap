#!/bin/bash
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/pagewal-20260913"
src="$task_root/pagewal-candidate-src"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home" CARGO_BUILD_JOBS=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp" CARGO_TARGET_DIR="$task_root/free-candidate-target"
mkdir "$src"
tar -xzf "$art/pilot-source.tar.gz" -C "$src"
mkdir -p "$src/.cargo" "$art/tmp"
printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$task_root" > "$src/.cargo/config.toml"
cd "$src"
cargo build --release --offline --features sqlite-balance,compact-cells --bin pagewal_bench > "$art/build.log" 2>&1
cp "$CARGO_TARGET_DIR/release/pagewal_bench" "$art/pilot"
cargo test --release --offline --features sqlite-balance,compact-cells --test pagewal -- --test-threads=1 > "$art/tests.log" 2>&1
sha256sum "$art/pilot" "$art/pilot-source.tar.gz" > "$art/hashes.txt"
mkdir "$art/matrix"
for rep in 1 2 3; do
  order='e4 pagewal sqlite'
  if [[ $rep = 2 ]]; then order='pagewal sqlite e4'; fi
  if [[ $rep = 3 ]]; then order='sqlite e4 pagewal'; fi
  for arm in $order; do
    python3 "$task_root/packing_time.py" prlimit --as=134217728 -- "$art/pilot" "$art/matrix/mixed-100k-r$rep/$arm" "$arm" 100000 256 4 mixed 1000 > "$art/matrix/mixed-100k-r$rep-$arm.log" 2>&1
    echo "completed Pi mixed-100k-r$rep $arm"
  done
done
for spec in 'load-100k 100000 256 0 load 1000' 'resize-1k 1000 256 6 resize 100' 'sustain-400k 400000 256 12 mixed 1000'; do
  read -r label n size cycles workload batch <<< "$spec"
  for arm in e4 pagewal sqlite; do
    python3 "$task_root/packing_time.py" prlimit --as=134217728 -- "$art/pilot" "$art/matrix/$label/$arm" "$arm" "$n" "$size" "$cycles" "$workload" "$batch" > "$art/matrix/$label-$arm.log" 2>&1
    echo "completed Pi $label $arm"
  done
done
date -u > "$art/matrix.complete"
