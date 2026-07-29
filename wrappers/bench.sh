#!/usr/bin/env bash
# Cross-wrapper micro-benchmark: the SAME point-lookup query run N times through
# each binding vs native Rust. The gap vs Rust is the binding's FFI +
# serialization overhead. Env: N=<iters> (default 50000).
#
#   ./wrappers/bench.sh
set -u

N=${N:-50000}
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$ROOT/target/release"
# JDK ≤ 22 for Kotlin (Kotlin 2.0.20 crashes on JDK 26); adjust if needed.
JDK22="${JDK22:-/Users/mala0061/Installer/homebrew/Cellar/openjdk/22.0.1/libexec/openjdk.jdk/Contents/Home}"

echo "building libsekejap (release)…"
( cd "$ROOT" && cargo build --release -p sekejap-capi -q ) || { echo "cargo build failed"; exit 1; }

RESULTS=()
run() { # $1 = label, $2 = command
  printf "  running %-7s… " "$1"
  local line
  line=$(eval "$2" 2>/dev/null | grep -E "^$1 " | tail -1)
  if [ -n "$line" ]; then echo "ok"; RESULTS+=("$line"); else echo "SKIP/FAIL"; fi
}

run rust   "N=$N cargo run --release --example bench_native -q --manifest-path '$ROOT/Cargo.toml'"

cc -O2 -std=gnu11 "$ROOT/wrappers/c/examples/bench.c" -I"$ROOT/wrappers/c/include" \
   -L"$TARGET" -lsekejap -Wl,-rpath,"$TARGET" -o /tmp/skbench_c 2>/dev/null \
   && run c "N=$N /tmp/skbench_c"

run go     "cd '$ROOT/wrappers/go'   && N=$N go run ./bench"
run node   "cd '$ROOT/wrappers/node' && N=$N node bench.cjs"

# python (PyO3) — uses the interpreter its .so was built for (3.12 via pyenv).
PYBIN="${PYBIN:-$HOME/.pyenv/versions/3.12.2/bin/python3.12}"
run python "PYTHONPATH='$ROOT/wrappers/python/python' N=$N '$PYBIN' '$ROOT/wrappers/python/bench.py'"

run swift  "cd '$ROOT/wrappers/swift' && N=$N swift run -c release bench"
run kotlin "cd '$ROOT/wrappers/kotlin' && JAVA_HOME='$JDK22' N=$N gradle run -q --console=plain"

echo
rust_ops=$(printf '%s\n' "${RESULTS[@]}" | awk '$1=="rust"{print $2}')
printf "%-8s %14s %11s %9s\n" "lang" "ops/sec" "us/op" "vs rust"
printf -- "-------------------------------------------------\n"
printf '%s\n' "${RESULTS[@]}" | sort -k2 -nr | while read -r lang ops us; do
  ratio=$(awk -v o="$ops" -v r="$rust_ops" 'BEGIN{ if(o>0) printf "%.2fx", r/o; else print "—" }')
  printf "%-8s %14s %11s %9s\n" "$lang" "$ops" "$us" "$ratio"
done
echo
echo "N=$N point-lookup queries. 'vs rust' = how many times more throughput native Rust has."
echo "C-ABI bindings (c/go/node/swift/kotlin) receive a JSON string (no parse) — overhead is"
echo "FFI + the C ABI's Rust-side JSON serialize. Python (PyO3) builds native objects directly"
echo "(no JSON), so it's a different — and here, cheaper — mechanism."
