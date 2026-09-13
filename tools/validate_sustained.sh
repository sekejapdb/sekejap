#!/bin/bash
set -euo pipefail
root=<scratch>
export TMPDIR="$root/tmp"
cargo test --release --offline --features sqlite-balance,compact-cells --test version_reuse --test reader_release --test byte_checkpoint --test overflow_lifecycle -- --nocapture > "$root/reuse-green.log" 2>&1
cargo test --release --offline -p kernel --lib --features sqlite-balance,compact-cells byte_policy > "$root/policy-tests.log" 2>&1
cargo build --release --offline --features sqlite-balance,compact-cells --bin lifecycle > "$root/build.log" 2>&1
cp target/release/lifecycle "$root/lifecycle-candidate"
"$root/lifecycle-candidate" --policy-crash-check "$root/policy-crashes-v2" > "$root/policy-crashes.log" 2>&1
"$root/lifecycle-candidate" --sustained "$root/smoke-v2" 1000 mixed_long 4096 1 4 e4-first > "$root/smoke.log" 2>&1
