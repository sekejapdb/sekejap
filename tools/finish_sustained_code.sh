#!/bin/bash
# Validate the checked-in implementation; no dependency on ephemeral drafts.
set -euo pipefail
root=<scratch>
export TMPDIR="$root/tmp"
export SQLITE_TMPDIR="$root/tmp"
cargo fmt --package sekejap-core --check
cargo test --release --offline --workspace --features sqlite-balance,compact-cells -- --test-threads=1 > "$root/direct-workspace-tests.log" 2>&1
cargo test --release --offline --features sqlite-balance,compact-cells --test codec_allocations -- --nocapture > "$root/direct-allocations.log" 2>&1
cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin lifecycle > "$root/direct-build.log" 2>&1
cp target/release/lifecycle "$root/lifecycle-direct"
tar --exclude=target --exclude=.git -czf "$root/direct-source.tar.gz" Cargo.toml Cargo.lock kernel src tests tools
if [[ ! -e "$root/direct-policy-crashes" ]]; then
  "$root/lifecycle-direct" --policy-crash-check "$root/direct-policy-crashes" > "$root/direct-policy-crashes.log" 2>&1
fi
if [[ ! -e "$root/direct-smoke" ]]; then
  "$root/lifecycle-direct" --sustained "$root/direct-smoke" 1000 mixed_long 4096 1 4 e4-first > "$root/direct-smoke.log" 2>&1
fi
echo 'Direct codec and reader publication validation complete'
