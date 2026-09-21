#!/bin/bash
set -euo pipefail
art="$1"
case "$art" in
 <scratch>)
  export PATH=<scratch>:$PATH
  export CARGO_HOME=<scratch>;;
 <scratch>)
  export CARGO_HOME=<scratch>;;
 *) exit 2;;
esac
export CARGO_TARGET_DIR="$art/targets/baseline" CARGO_BUILD_JOBS=1
export TMPDIR="$art/tmp" E4_LAW1_ARTIFACTS="$art/baseline-law1-lean"
# Do not overlap regression compilation with timed benchmark arms.
for attempt in $(seq 1 360); do
 if grep -q '^COMPLETE qualify$' "$art/qualify.log"; then break; fi
 if grep -q '^Traceback' "$art/qualify.log"; then exit 1; fi
 sleep 5
done
grep -q '^COMPLETE qualify$' "$art/qualify.log"
mkdir "$art/src/baseline"
tar -xzf "$art/baseline-source.tar.gz" -C "$art/src/baseline"
cp -a "$art/../../targets/baseline" "$CARGO_TARGET_DIR"
cd "$art/src/baseline"
tar -xzf "$art/test-overlay.tar.gz"
if [ -d <scratch> ]; then
 mkdir -p .cargo
 printf '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="<scratch>"\n' > .cargo/config.toml
fi
cargo clean --release -p kernel -p sekejap-core > "$art/baseline-clean.log" 2>&1
python3 tools/run_foundation.py lean "$art/baseline-lean"
echo 'COMPLETE validation'
