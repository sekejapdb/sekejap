#!/bin/bash
# Frozen diagnostic source; no production engine promotion.
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/scan-trace-20260914"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home"
export CARGO_TARGET_DIR="$art/target" CARGO_BUILD_JOBS=1 TMPDIR="$art/tmp"
# The Rust test harness runs work on another thread. glibc's default arena
# reservation can thrash mmap under RLIMIT_AS although live heap fits. This
# diagnostic still uses the same 128 MiB cap; allocator policy is explicit.
export MALLOC_ARENA_MAX=1
mkdir -p "$art/src" "$art/tmp"
tar -xzf "$task_root/artifacts/scan-trace-source-20260914.tar.gz" -C "$art/src"
mkdir -p "$art/src/.cargo"
printf '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="%s/vendor"\n' "$task_root" > "$art/src/.cargo/config.toml"
cd "$art/src"
uname -a > "$art/platform.txt"
sha256sum "$task_root/artifacts/scan-trace-source-20260914.tar.gz" > "$art/source.sha256"
cargo test --release --offline --features sqlite-balance,compact-cells,test-support -p sekejap-kernel --test law1_heap --no-run > "$art/build.log" 2>&1
for binary in "$CARGO_TARGET_DIR"/release/deps/law1_heap-*; do
 if [ -x "$binary" ] && [ -f "$binary" ]; then
  sha256sum "$binary" > "$art/binary.sha256"
  file "$binary" >> "$art/platform.txt"
  for run in 1 2 3; do
   export E4_LAW1_ARTIFACTS="$art/run-$run"
   (ulimit -v 131072; "$binary" --nocapture --test-threads=1) > "$art/run-$run.log" 2>&1
  done
 fi
done
touch "$art/complete"
