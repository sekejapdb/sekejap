#!/bin/sh
set -eu
ROOT=<scratch>
export TMPDIR="$ROOT/tmp"
cargo test --release --offline --test reader_release > "$ROOT/default-reader-tests.log" 2>&1
cargo test --offline --features sqlite-balance,compact-cells --test reader_release > "$ROOT/debug-reader-tests.log" 2>&1
cargo run -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin lifecycle -- --crash-check "$ROOT/crashes" > "$ROOT/crashes.log" 2>&1
