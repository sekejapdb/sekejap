#!/bin/bash
# Run only after the original Pi candidate process has terminated.
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/native-requal-20260914/r2"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home"
export CARGO_TARGET_DIR="$art/targets/candidate" CARGO_BUILD_JOBS=1 MALLOC_ARENA_MAX=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
export E4_LAW1_ARTIFACTS="$art/aligned-run/candidate-final-law1"
test ! -e "$art/aligned-run/candidate-final-workspace.log"
grep -q 'PASS aligned full workspace baseline' "$art/aligned-runner.log"
cd "$art/src/candidate"
cp "$art/candidate-pack-shape.rs" core/kernel/tests/pack_shape.rs
sha256sum core/kernel/tests/pack_shape.rs > "$art/aligned-run/candidate-final-fixture-hash.txt"
cargo test --release --offline --workspace --features sqlite-balance,compact-cells -- --test-threads=1 > "$art/aligned-run/candidate-final-workspace.log" 2>&1
cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin foundation_scale --bin pagewal_bench --bin pagewal_cap --bin foundation_space > "$art/aligned-run/candidate-final-build.log" 2>&1
mkdir -p "$art/bin/candidate"
for binary in foundation_scale pagewal_bench pagewal_cap foundation_space; do
 cp "$CARGO_TARGET_DIR/release/$binary" "$art/bin/candidate/$binary"
done
sha256sum "$art/bin/"*/* > "$art/aligned-run/final-binary-hashes.txt"
test "$(sha256sum "$art"/bin/*/foundation_scale | cut -d' ' -f1 | sort -u | wc -l)" -eq 2
touch "$art/aligned-run/correctness-complete"
echo 'COMPLETE Pi native correctness with verified format fixture'
