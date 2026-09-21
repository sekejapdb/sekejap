#!/bin/bash
# Repeat the complete suite with the same test-only Direct-I/O buffer fix.
# Initial archives/logs remain immutable evidence; aligned-test.patch is the delta.
set -euo pipefail
art="$1"
case "$art" in
 <scratch>)
  task_root=<scratch>
  export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home";;
 <scratch>)
  export CARGO_HOME=<scratch>;;
 *) exit 2;;
esac
export CARGO_BUILD_JOBS=1 MALLOC_ARENA_MAX=1
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir "$art/aligned-run"
sha256sum "$art/aligned-test.patch" > "$art/aligned-run/patch-hash.txt"
for variant in baseline candidate; do
 if [ ! -d "$art/src/$variant" ]; then
  mkdir "$art/src/$variant"
  tar -xzf "$art/$variant.tar.gz" -C "$art/src/$variant"
 fi
 export CARGO_TARGET_DIR="$art/targets/$variant"
 if [ ! -d "$CARGO_TARGET_DIR" ]; then
  cp -a "$art/targets/baseline" "$CARGO_TARGET_DIR"
 fi
 cd "$art/src/$variant"
 if [ -n "${task_root:-}" ]; then
  mkdir -p .cargo
  printf '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="%s/vendor"\n' "$task_root" > .cargo/config.toml
 fi
 patch --batch -p1 < "$art/aligned-test.patch" > "$art/aligned-run/$variant-patch.log"
 sha256sum core/kernel/src/io.rs >> "$art/aligned-run/io-hashes.txt"
 export E4_LAW1_ARTIFACTS="$art/aligned-run/$variant-law1"
 cargo test --release --offline --workspace --features sqlite-balance,compact-cells -- --test-threads=1 > "$art/aligned-run/$variant-workspace.log" 2>&1
 grep -q 'Compiling kernel ' "$art/aligned-run/$variant-workspace.log"
 grep -q 'Compiling e4-prototype ' "$art/aligned-run/$variant-workspace.log"
 echo "PASS aligned full workspace $variant"
 cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin foundation_scale --bin pagewal_bench --bin pagewal_cap --bin foundation_space > "$art/aligned-run/$variant-build.log" 2>&1
 mkdir -p "$art/bin/$variant"
 for binary in foundation_scale pagewal_bench pagewal_cap foundation_space; do
  cp "$CARGO_TARGET_DIR/release/$binary" "$art/bin/$variant/$binary"
 done
 sha256sum "$art/bin/$variant/"* >> "$art/aligned-run/binary-hashes.txt"
done
test "$(sha256sum "$art"/bin/*/foundation_scale | cut -d' ' -f1 | sort -u | wc -l)" -eq 2
touch "$art/aligned-run/correctness-complete"
echo 'COMPLETE aligned native correctness'
