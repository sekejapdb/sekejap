#!/bin/bash
set -euo pipefail
root=<scratch>
art="$root/artifacts/write-path-20260912"
export PATH="$root/toolchain/bin:$PATH"
export CARGO_HOME="$root/cargo-home" CARGO_BUILD_JOBS=2
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
mkdir -p "$art/tmp"
case "${1:-validate}" in
validate)
  mkdir "$root/write-src" "$root/write-baseline-src"
  tar -xzf "$root/write-source.tar.gz" -C "$root/write-src"
  tar -xzf "$root/packing-source.tar.gz" -C "$root/write-baseline-src"
  cp "$root/write-src/dist/src/cli/collections.rs" "$root/write-baseline-src/dist/src/cli/collections.rs"
  for dir in "$root/write-src" "$root/write-baseline-src"; do
    mkdir -p "$dir/.cargo"
    printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s/vendor"\n' "$root" > "$dir/.cargo/config.toml"
  done
  cd "$root/write-baseline-src"
  export CARGO_TARGET_DIR="$root/write-baseline-target"
  cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin collections > "$art/baseline-build.log" 2>&1
  cp "$CARGO_TARGET_DIR/release/collections" "$art/collections-baseline"
  cd "$root/write-src"
  export CARGO_TARGET_DIR="$root/write-current-target"
  cargo test --release --offline -p sekejap-kernel --features sqlite-balance,compact-cells --lib --test resource_limits -- --test-threads=1 > "$art/kernel-tests.log" 2>&1
  cargo test --release --offline -p sekejap-core --features sqlite-balance,compact-cells --lib --test collections --test delete_packing --test write_path -- --test-threads=1 > "$art/collection-tests.log" 2>&1
  cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin collections > "$art/current-build.log" 2>&1
  cp "$CARGO_TARGET_DIR/release/collections" "$art/collections-current"
  sha256sum "$art/collections-baseline" "$art/collections-current" > "$art/binary.sha256"
  date -Is > "$art/validation.complete"
  ;;
rebuild-current)
  cd "$root/write-src"
  export CARGO_TARGET_DIR="$root/write-current-target"
  cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin collections > "$art/current-rebuild.log" 2>&1
  cp "$CARGO_TARGET_DIR/release/collections" "$art/collections-current"
  sha256sum "$art/collections-baseline" "$art/collections-current" > "$art/binary.sha256"
  ;;
bench)
  test -f "$art/validation.complete"
  if cmp -s "$art/collections-baseline" "$art/collections-current"; then
    echo 'Refusing identical baseline/current binaries' >&2; exit 1
  fi
  mkdir "$art/matrix"
  uptime > "$art/start-load.txt"
  run() {
    local label=$1 n=$2 cycles=$3 workload=$4 reader=$5 dim=$6 changes=$7 order=$8
    for arm in $order; do
      local binary="$art/collections-current" engine=e4
      if [[ $arm = baseline ]]; then binary="$art/collections-baseline"; fi
      if [[ $arm = sqlite ]]; then engine=sqlite; fi
      COLLECTION_VECTOR_DIM=$dim COLLECTION_CHANGE_VECTORS=$changes python3 "$root/packing_time.py" \
        prlimit --as=134217728 -- "$binary" "$art/matrix/$label/$arm" "$engine" "$n" off "$cycles" "$workload" "$reader" \
        > "$art/matrix/$label-$arm.log" 2>&1
    done
  }
  run mixed-400000 400000 12 mixed none 4 0 'baseline current sqlite'
  run vectors-stable 10000 4 updates none 1536 0 'sqlite current baseline'
  run vectors-changing 10000 4 updates none 1536 1 'baseline current sqlite'
  run vectors-held 10000 4 updates held 1536 0 'sqlite current baseline'
  uptime > "$art/end-load.txt"
  date -Is > "$art/benchmark.complete"
  ;;
*) exit 1;;
esac
