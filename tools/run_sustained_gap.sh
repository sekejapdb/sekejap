#!/bin/bash
set -euo pipefail
root=<scratch>
export TMPDIR="$root/tmp"
export SQLITE_TMPDIR="$root/tmp"
cargo fmt --package e4-prototype
cargo build --release --offline --features sqlite-balance,compact-cells --bin lifecycle > "$root/gap-build.log" 2>&1
cp target/release/lifecycle "$root/lifecycle-gap"
COPYFILE_DISABLE=1 tar --exclude=target --exclude=.git -czf "$root/gap-source.tar.gz" Cargo.toml Cargo.lock kernel src tests tools
"$root/lifecycle-gap" --sustained "$root/gap-smoke" 1000 mixed_short_gap 4096 1 4 e4-first > "$root/gap-smoke.log" 2>&1
bash tools/run_sustained_matrix.sh "$root/lifecycle-gap" <scratch> 100000 12 mixed_short_gap 1 4194304 1
