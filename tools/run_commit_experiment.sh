#!/bin/bash
set -euo pipefail
base=<scratch>
export TMPDIR="$base/tmp" SQLITE_TMPDIR="$base/tmp"
mkdir "$base/matrix"
cp /tmp/e4-commit-baseline/target/release/collections "$base/baseline"
cp /tmp/e4-commit-candidate/target/release/collections "$base/candidate"
if cmp -s "$base/baseline" "$base/candidate"; then exit 1; fi
shasum -a 256 "$base/baseline" "$base/candidate" > "$base/binaries.sha256"
for rep in 1 2 3; do
  order='baseline candidate sqlite'
  if [[ $rep = 2 ]]; then order='sqlite candidate baseline'; fi
  if [[ $rep = 3 ]]; then order='candidate baseline sqlite'; fi
  for spec in '1 1000 2 mixed' '100 10000 3 mixed' '1000 100000 4 mixed'; do
    read -r batch rows cycles mode <<< "$spec"
    label="batch-$batch-r$rep"
    for arm in $order; do
      engine=e4
      binary="$base/$arm"
      if [[ $arm = sqlite ]]; then engine=sqlite; binary="$base/baseline"; fi
      COLLECTION_BATCH=$batch "$binary" "$base/matrix/$label/$arm" "$engine" "$rows" off "$cycles" "$mode" none > "$base/matrix/$label-$arm.log" 2>&1
      echo "completed $label $arm"
    done
  done
done
date -u > "$base/matrix.complete"
