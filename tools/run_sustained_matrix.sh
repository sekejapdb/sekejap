#!/bin/bash
# Reproduce an explicit paired matrix with a retained executable and fresh root.
set -euo pipefail
binary=${1:?retained lifecycle executable}
root=${2:?fresh scratch output directory}
rows=${3:-100000}
cycles=${4:-12}
cases=${5:-'load updates delete_reinsert mixed_none mixed_short mixed_short_gap mixed_long'}
repeats=${6:-3}
threshold=${7:-4194304}
publications=${8:-1}
case "$root" in <scratch>) ;; *) echo 'Output must be in the E4 scratch directory' >&2; exit 1;; esac
test -x "$binary"
test ! -e "$root"
mkdir -p "$root/tmp"
export TMPDIR="$root/tmp"
export SQLITE_TMPDIR="$root/tmp"
for ((repeat=1; repeat<=repeats; repeat++)); do
  order=e4-first
  if ((repeat % 2 == 0)); then order=sqlite-first; fi
  for workload in $cases; do
    "$binary" --sustained "$root/matrix-$workload-r$repeat" "$rows" "$workload" "$threshold" "$publications" "$cycles" "$order" > "$root/matrix-$workload-r$repeat.log" 2>&1
    echo "$workload repeat $repeat finished"
  done
done
