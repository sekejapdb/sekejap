#!/bin/bash
set -euo pipefail
base=${REUSE_ARTIFACT_ROOT:-<scratch>
export TMPDIR="$base/tmp" SQLITE_TMPDIR="$base/tmp"
mkdir "$base/matrix"
if cmp -s "$base/baseline" "$base/candidate"; then exit 1; fi
# Alternate order; all engines get identical operations and durability settings.
for rep in 1 2; do
  order='baseline candidate sqlite'
  if [[ $rep = 2 ]]; then order='sqlite candidate baseline'; fi
  for spec in '1 1000 2' '100 10000 3' '1000 100000 4'; do
    read -r batch rows cycles <<< "$spec"
    label="batch-$batch-r$rep"
    for arm in $order; do
      engine=e4; binary="$base/$arm"
      if [[ $arm = sqlite ]]; then engine=sqlite; binary="$base/baseline"; fi
      COLLECTION_BATCH=$batch "$binary" "$base/matrix/$label/$arm" "$engine" "$rows" off "$cycles" mixed none > "$base/matrix/$label-$arm.log" 2>&1
      echo "completed $label $arm"
    done
  done
done
for spec in 'mixed-400000 400000 12 none' 'held-100000 100000 4 held'; do
  read -r label rows cycles reader <<< "$spec"
  for arm in baseline candidate sqlite; do
    engine=e4; binary="$base/$arm"
    if [[ $arm = sqlite ]]; then engine=sqlite; binary="$base/baseline"; fi
    COLLECTION_BATCH=1000 "$binary" "$base/matrix/$label/$arm" "$engine" "$rows" off "$cycles" mixed "$reader" > "$base/matrix/$label-$arm.log" 2>&1
    echo "completed $label $arm"
  done
done
date -u > "$base/matrix.complete"
