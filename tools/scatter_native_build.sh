#!/bin/bash
set -euo pipefail
art="$1"
case "$art" in
  <scratch>)
    task_root=<scratch>
    export PATH="$task_root/toolchain/bin:$PATH" CARGO_HOME="$task_root/cargo-home"
    ;;
  <scratch>)
    export CARGO_HOME=<scratch>
    ;;
  *) exit 2;;
esac
export CARGO_BUILD_JOBS=1 TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir -p "$art/tmp" "$art/src" "$art/bin"
uname -a > "$art/platform.txt"
for variant in baseline compact packing combined; do
  export CARGO_TARGET_DIR="$art/targets/$variant"
  mkdir -p "$art/src/$variant"
  mkdir "$art/bin/$variant"
  tar -xzf "$art/$variant.tar.gz" -C "$art/src/$variant"
  if [ -n "${task_root:-}" ]; then
    mkdir -p "$art/src/$variant/.cargo"
    printf '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="%s/vendor"\n' "$task_root" > "$art/src/$variant/.cargo/config.toml"
  fi
  cd "$art/src/$variant"
  cargo build -p sekejap-bench --release --offline --features sqlite-balance,compact-cells --bin foundation_scale --bin pagewal_bench --bin pagewal_cap --bin foundation_space > "$art/build-$variant.log" 2>&1
  for binary in foundation_scale pagewal_bench pagewal_cap foundation_space; do
    cp "$CARGO_TARGET_DIR/release/$binary" "$art/bin/$variant/$binary"
  done
  file "$art/bin/$variant/foundation_scale" >> "$art/platform.txt"
  sha256sum "$art/bin/$variant/"* >> "$art/binary-hashes.txt"
  printf 'built %s\n' "$variant"
done
test "$(sha256sum "$art"/bin/*/foundation_scale | cut -d' ' -f1 | sort -u | wc -l)" -eq 4
python3 "$art/run_scatter_loop.py" "$art" probe
