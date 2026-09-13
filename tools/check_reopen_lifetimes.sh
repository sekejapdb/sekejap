#!/bin/bash
set -euo pipefail
root=<scratch>
export TMPDIR="$root/tmp"
set +e
cargo test --release --offline --features sqlite-balance,compact-cells --test version_reuse every_reopen -- --nocapture > "$root/reopen-lifetime-red.log" 2>&1
status=$?
set -e
if [[ $status == 0 ]]; then echo 'Expected repeated-reopen regression did not fail'; exit 1; fi
rg 'intermediate versions accumulate' "$root/reopen-lifetime-red.log"
