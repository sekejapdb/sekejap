#!/bin/bash
set -euo pipefail
root=<scratch>
art="$root/artifacts/reuse-20260912"
export PATH="$root/toolchain/bin:$PATH"
export CARGO_HOME="$root/cargo-home" CARGO_BUILD_JOBS=2
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir -p "$art/tmp"
mkdir "$root/reuse-candidate-src"
tar -xzf "$art/candidate-final-source.tar.gz" -C "$root/reuse-candidate-src"
src="$root/reuse-candidate-src"
mkdir -p "$src/.cargo"
printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$root" > "$src/.cargo/config.toml"
cd "$src"
export CARGO_TARGET_DIR="$root/free-candidate-target"
cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin collections > "$art/candidate-build.log" 2>&1
cp "$CARGO_TARGET_DIR/release/collections" "$art/candidate"
# Same accepted engine and harness as the prior measured loop; fingerprint it.
cp "$root/artifacts/freelist-20260912/baseline" "$art/baseline"
cargo test --release --offline -p kernel --features sqlite-balance,compact-cells --lib --test persistent_free --test freelist --test resource_limits --test snapshot --no-fail-fast -- --test-threads=1 > "$art/tests.log" 2>&1
cargo test --release --offline --features sqlite-balance,compact-cells --test delete_packing --test overflow_lifecycle --test recovery_faults --test write_path --no-fail-fast -- --test-threads=1 >> "$art/tests.log" 2>&1
echo 0 > "$art/tests.status"
sha256sum "$art/baseline" "$art/candidate" > "$art/binary.sha256"
date -Is > "$art/build-tests.complete"
# Timing is a separate invocation, after the Mac significance gate.
if [[ "${1:-}" != bench ]]; then exit 0; fi
bash "$art/reuse_pi_matrix.sh"
