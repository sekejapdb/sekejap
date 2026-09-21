#!/bin/bash
set -euo pipefail
root=<scratch>
export PATH="$root/toolchain/bin:$PATH" CARGO_HOME="$root/cargo-home"
export TMPDIR="$root/tmp" SQLITE_TMPDIR="$root/tmp" CARGO_BUILD_JOBS=2
cd "$root/src"
cargo test --offline --release -p kernel --features sqlite-balance,compact-cells --lib --test resource_limits -- --test-threads=1 > "$root/artifacts/final-tests.log" 2>&1
cargo build -p sekejap-dist --offline --release --features sqlite-balance,compact-cells --bin lifecycle > "$root/artifacts/final-build.log" 2>&1
cp target/release/lifecycle "$root/artifacts/lifecycle-pi"
sha256sum "$root/artifacts/lifecycle-pi" > "$root/artifacts/lifecycle-pi.sha256"
prlimit --as=134217728 -- env E4_LIMIT_DATA_BYTES=83886080 "$root/artifacts/lifecycle-pi" --resource-crash-check "$root/artifacts/final-crashes" > "$root/artifacts/final-crashes.log" 2>&1
date -Is > "$root/artifacts/final-validation-complete"
bash "$root/resource_pi_matrix.sh" address-space
