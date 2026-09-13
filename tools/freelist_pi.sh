#!/bin/bash
set -euo pipefail
root=<scratch>
art="$root/artifacts/freelist-20260912"
export PATH="$root/toolchain/bin:$PATH"
export CARGO_HOME="$root/cargo-home" CARGO_BUILD_JOBS=2
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir -p "$art/tmp"
mkdir "$root/free-candidate-src" "$root/free-baseline-src"
tar -xzf "$art/candidate-source.tar.gz" -C "$root/free-candidate-src"
tar -xzf "$root/write-source.tar.gz" -C "$root/free-baseline-src"
cp "$root/free-candidate-src/src/bin/collections.rs" "$root/free-baseline-src/src/bin/collections.rs"
for arm in baseline candidate; do
  src="$root/free-$arm-src"
  mkdir -p "$src/.cargo"
  printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$root" > "$src/.cargo/config.toml"
  cd "$src"
  export CARGO_TARGET_DIR="$root/free-$arm-target"
  cargo build --release --offline --features sqlite-balance,compact-cells --bin collections > "$art/$arm-build.log" 2>&1
  cp "$CARGO_TARGET_DIR/release/collections" "$art/$arm"
done
cd "$root/free-candidate-src"
# Preserve negative safety findings; they remain a reason not to promote.
set +e
cargo test --release --offline -p kernel --features sqlite-balance,compact-cells --lib --test embedded_free --test freelist --test resource_limits --no-fail-fast -- --test-threads=1 > "$art/tests.log" 2>&1
test_status=$?
set -e
echo "$test_status" > "$art/tests.status"
sha256sum "$art/baseline" "$art/candidate" > "$art/binary.sha256"
sha256sum src/bin/collections.rs kernel/src/free_pages.rs kernel/src/store.rs kernel/src/meta.rs kernel/src/pool.rs kernel/src/wal.rs > "$art/source.sha256"
if cmp -s "$art/baseline" "$art/candidate"; then exit 1;fi
mkdir "$art/matrix"
for spec in 'batch-1 1 1000 2 none' 'batch-100 100 10000 3 none' 'mixed-400000 1000 400000 12 none' 'held-100000 1000 100000 4 held'; do
  read -r label batch rows cycles reader <<< "$spec"
  for arm in baseline candidate sqlite; do
    engine=e4;binary="$art/$arm"
    if [[ $arm = sqlite ]]; then engine=sqlite;binary="$art/baseline";fi
    COLLECTION_BATCH=$batch python3 "$root/packing_time.py" prlimit --as=134217728 -- "$binary" "$art/matrix/$label/$arm" "$engine" "$rows" off "$cycles" mixed "$reader" > "$art/matrix/$label-$arm.log" 2>&1
    echo "completed $label $arm"
  done
done
date -Is > "$art/matrix.complete"
