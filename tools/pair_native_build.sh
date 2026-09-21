#!/bin/bash
set -euo pipefail
art="$1"
case "$art" in
 <scratch>|<scratch>|<scratch>)
  task_root=<scratch>
  export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home";;
 <scratch>|<scratch>|<scratch>)
  export CARGO_HOME=<scratch>;;
 *) exit 2;;
esac
export CARGO_BUILD_JOBS=1 TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir -p "$art/tmp" "$art/bin" "$art/src"
uname -a > "$art/platform.txt"
# The frozen native control is exactly the previous loop's baseline binary.
control="$art/../bin/baseline"
if [ "${art##*/}" = v2 ] || [ "${art##*/}" = v3 ]; then control="$art/../../bin/baseline"; fi
cp -a "$control" "$art/bin/baseline"
sha256sum "$art/bin/baseline/"* > "$art/binary-hashes.txt"
for variant in pair compact-pair; do
 export CARGO_TARGET_DIR="$art/targets/$variant"
 mkdir "$art/src/$variant" "$art/bin/$variant"
 tar -xzf "$art/$variant.tar.gz" -C "$art/src/$variant"
 if [ -n "${task_root:-}" ]; then
  mkdir -p "$art/src/$variant/.cargo"
  printf '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="%s/vendor"\n' "$task_root" > "$art/src/$variant/.cargo/config.toml"
 fi
 cd "$art/src/$variant"
 if [ "${art##*/}" = v3 ]; then
  mkdir -p "$art/targets"
  cp -a "$art/../v2/targets/$variant" "$CARGO_TARGET_DIR"
  # Reuse only dependency compilation. Remove BOTH workspace packages in this
  # new isolated copy; zero-mtime source archives must never reuse engine code.
  cargo clean --release -p sekejap-kernel -p sekejap-core > "$art/clean-$variant.log" 2>&1
 fi
 cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin foundation_scale --bin pagewal_bench --bin pagewal_cap --bin foundation_space > "$art/build-$variant.log" 2>&1
 grep -q 'Compiling kernel ' "$art/build-$variant.log"
 grep -q 'Compiling e4-prototype ' "$art/build-$variant.log"
 for binary in foundation_scale pagewal_bench pagewal_cap foundation_space; do
  cp "$CARGO_TARGET_DIR/release/$binary" "$art/bin/$variant/$binary"
 done
 file "$art/bin/$variant/foundation_scale" >> "$art/platform.txt"
 sha256sum "$art/bin/$variant/"* >> "$art/binary-hashes.txt"
 printf 'built %s\n' "$variant"
done
test "$(sha256sum "$art"/bin/*/foundation_scale | cut -d' ' -f1 | sort -u | wc -l)" -eq 3
python3 "$art/run_scatter_loop.py" "$art" probe compact-pair
