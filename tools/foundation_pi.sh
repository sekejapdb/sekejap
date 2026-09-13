#!/bin/bash
set -euo pipefail
task_root=<scratch>
art="$task_root/artifacts/fixed-work-20260913"
src="$task_root/fixed-work-src"
export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home" CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="$task_root/free-candidate-target"
mkdir "$src"
tar -xzf "$art/source.tar.gz" -C "$src"
mkdir -p "$src/.cargo"
printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$task_root" > "$src/.cargo/config.toml"
cd "$src"
python3 tools/run_foundation.py lean "$art/lean"
cp "$CARGO_TARGET_DIR/release/foundation_scale" "$art/foundation_scale"
sha256sum "$art/source.tar.gz" "$art/foundation_scale" > "$art/hashes.txt"
python3 tools/run_foundation.py scale "$art/scale" --binary "$art/foundation_scale"
python3 tools/run_foundation.py large "$art/large" --binary "$art/foundation_scale"
date -u > "$art/complete"
