#!/bin/bash
set -euo pipefail
root=<scratch>
art="$root/artifacts/packing-20260911"
export PATH="$root/toolchain/bin:$PATH"
export CARGO_HOME="$root/cargo-home" CARGO_TARGET_DIR="$root/packing-target" CARGO_BUILD_JOBS=2
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
case "${1:-validate}" in
validate)
  mkdir -p "$art/tmp"
  mkdir "$root/packing-src"
  tar -xzf "$root/packing-source.tar.gz" -C "$root/packing-src"
  mkdir -p "$root/packing-src/.cargo"
  cat > "$root/packing-src/.cargo/config.toml" <<CFG
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "$root/vendor"
CFG
  cd "$root/packing-src"
  rustc --version > "$art/versions.txt"
  cargo --version >> "$art/versions.txt"
  uname -a >> "$art/versions.txt"
  uptime > "$art/validation-start-load.txt"
  cargo test --release --offline -p kernel --features sqlite-balance,compact-cells --lib --test resource_limits -- --test-threads=1 > "$art/kernel-tests.log" 2>&1
  cargo test --release --offline -p e4-prototype --features sqlite-balance,compact-cells --lib --test delete_packing --test collections -- --test-threads=1 > "$art/collection-tests.log" 2>&1
  cargo build --release --offline --features sqlite-balance,compact-cells --bin collections --bin collection_inspect > "$art/build.log" 2>&1
  cp "$root/packing-target/release/collections" "$art/collections"
  cp "$root/packing-target/release/collection_inspect" "$art/collection-inspect"
  sha256sum "$art/collections" > "$art/binary.sha256"
  date -Is > "$art/validation.complete"
  ;;
bench)
  test -f "$art/validation.complete"
  mkdir -p "$art/matrix"
  uptime > "$art/benchmark-start-load.txt"
  for rows in 100000 400000; do
    for engine in e4 sqlite; do
      python3 "$root/packing_time.py" prlimit --as=134217728 -- "$art/collections" "$art/matrix/mixed-$rows" "$engine" "$rows" off 12 mixed none > "$art/matrix/mixed-$rows-$engine.log" 2>&1
    done
  done
  for engine in sqlite e4; do
    python3 "$root/packing_time.py" prlimit --as=134217728 -- "$art/collections" "$art/matrix/held-100000" "$engine" 100000 off 4 mixed held > "$art/matrix/held-100000-$engine.log" 2>&1
  done
  uptime > "$art/benchmark-end-load.txt"
  date -Is > "$art/benchmark.complete"
  ;;
*) exit 1;;
esac
