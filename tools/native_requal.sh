#!/bin/bash
set -euo pipefail
art="$1"
case "$art" in
 <scratch>|<scratch>)
  task_root=<scratch>
  export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home"
  cache="$task_root/artifacts/scatter-resume-20260914/pair-packing-20260914/v3/targets/baseline";;
 <scratch>|<scratch>)
  export CARGO_HOME=<scratch>
  cache=<scratch>;;
 *) exit 2;;
esac
export CARGO_BUILD_JOBS=1 MALLOC_ARENA_MAX=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir -p "$art/tmp" "$art/src" "$art/targets" "$art/bin"
uname -a > "$art/platform.txt"
rustc -Vv >> "$art/platform.txt"
cc --version >> "$art/platform.txt"
for variant in baseline candidate; do
 mkdir "$art/src/$variant" "$art/bin/$variant"
 tar -xzf "$art/$variant.tar.gz" -C "$art/src/$variant"
 sha256sum "$art/$variant.tar.gz" >> "$art/source-hashes.txt"
 export CARGO_TARGET_DIR="$art/targets/$variant"
 cp -a "$cache" "$CARGO_TARGET_DIR"
 cd "$art/src/$variant"
 if [ -n "${task_root:-}" ]; then
  mkdir -p .cargo
  printf '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="%s/vendor"\n' "$task_root" > .cargo/config.toml
 fi
 # Reuse dependency objects only, in distinct targets; never stale engine code.
 cargo clean --release -p kernel -p sekejap-core > "$art/$variant-clean.log" 2>&1
 export E4_LAW1_ARTIFACTS="$art/$variant-law1"
 cargo test --release --offline --workspace --features sqlite-balance,compact-cells -- --test-threads=1 > "$art/$variant-workspace.log" 2>&1
 grep -q 'Compiling kernel ' "$art/$variant-workspace.log"
 grep -q 'Compiling e4-prototype ' "$art/$variant-workspace.log"
 echo "PASS full workspace $variant"
 cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin foundation_scale --bin pagewal_bench --bin pagewal_cap --bin foundation_space > "$art/$variant-build.log" 2>&1
 for binary in foundation_scale pagewal_bench pagewal_cap foundation_space; do
  cp "$CARGO_TARGET_DIR/release/$binary" "$art/bin/$variant/$binary"
 done
 sha256sum "$art/bin/$variant/"* >> "$art/binary-hashes.txt"
 file "$art/bin/$variant/foundation_scale" >> "$art/platform.txt"
done
test "$(sha256sum "$art"/bin/*/foundation_scale | cut -d' ' -f1 | sort -u | wc -l)" -eq 2
touch "$art/correctness-complete"
echo 'COMPLETE native correctness'
