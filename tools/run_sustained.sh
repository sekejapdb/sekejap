#!/bin/bash
set -euo pipefail
root=<scratch>
mode=${1:?baseline or candidate}
mkdir -p "$root/tmp"
export TMPDIR="$root/tmp"
export SQLITE_TMPDIR="$root/tmp"
if [[ "$mode" == baseline ]]; then
  test ! -e "$root/lifecycle-baseline"
  cp target/release/lifecycle "$root/lifecycle-baseline"
  tar --exclude=target --exclude=.git -czf "$root/baseline-source.tar.gz" Cargo.toml Cargo.lock kernel src tests tools
fi
binary="$root/lifecycle-$mode"
for repeat in 1 2 3; do
  order=e4-first
  if [[ "$repeat" == 2 ]]; then order=sqlite-first; fi
  for workload in updates mixed_none mixed_long; do
    "$binary" --sustained "$root/$mode-$workload-r$repeat" 100000 "$workload" 4194304 1 12 "$order" > "$root/$mode-$workload-r$repeat.log" 2>&1
    echo "$mode $workload repeat $repeat finished"
  done
done
