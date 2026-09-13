#!/bin/bash
# Run after build-complete, with no concurrent compilation or tests.
set -euo pipefail
root=<scratch>
test -f "$root/artifacts/build-complete"
kind=${1:-cgroup}
case "$kind" in
  cgroup)
    grep -qw memory /sys/fs/cgroup/cgroup.controllers || { echo "memory controller unavailable; refuse uncapped run" >&2; exit 1; }
    launcher=(systemd-run --user --wait --pipe --collect -p MemoryMax=64M -p MemorySwapMax=0)
    ;;
  address-space) launcher=(prlimit --as=134217728 --) ;;
  *) echo "unknown limit kind" >&2; exit 1 ;;
esac
binary="$root/artifacts/lifecycle-pi"
matrix="$root/artifacts/matrix-$kind"
if [[ "${2:-}" = --resume ]]; then
  test -f "$root/artifacts/postboot-verification.json"
  test -d "$matrix"
else
  mkdir "$matrix"
fi
export TMPDIR="$root/tmp"
export SQLITE_TMPDIR="$root/tmp"
unset E4_LIMIT_DATA_BYTES E4_BENCH_ORDERED_LOAD
for rows in 100000 400000 4000000; do
  workloads="load updates mixed_none mixed_long"
  if [[ "$rows" = 4000000 ]]; then workloads=load; fi
  for workload in $workloads; do
    for mode in ordinary limited; do
      name="$mode-$workload-$rows"
      if [[ "${2:-}" = --resume && -f "$matrix/$name.completed" ]]; then continue; fi
      # An interrupted pair must first be preserved outside this matrix.
      test ! -e "$matrix/$name"
      order=e4-first
      selection=()
      if [[ "$mode" = limited ]]; then
        order=sqlite-first
        data_bytes=134217728
        if [[ "$rows" = 400000 ]]; then data_bytes=536870912; fi
        if [[ "$rows" = 4000000 ]]; then data_bytes=1610612736; fi
        selection=("E4_LIMIT_DATA_BYTES=$data_bytes")
      fi
      if [[ "$rows" = 4000000 ]]; then selection+=("E4_BENCH_ORDERED_LOAD=1"); fi
      date -Is > "$matrix/$name.started"
      # Each pair gets a fresh cgroup. The recorded memory.max must agree.
      "${launcher[@]}" /usr/bin/env "${selection[@]}" "$binary" --sustained \
        "$matrix/$name" "$rows" "$workload" 4194304 1 4 "$order" \
        > "$matrix/$name.log" 2>&1
      python3 - "$matrix/$name/results.json" "$kind" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]))
assert r['complete']
if sys.argv[2]=='cgroup':
    assert r['process_memory_limits']['memory_max']=='67108864'
    assert r['process_memory_limits']['swap_max']=='0'
else:
    assert r['process_memory_limits']['address_space_soft_hard']==['134217728','134217728']
PY
      date -Is > "$matrix/$name.completed"
    done
  done
done
python3 "$root/src/tools/check_sustained.py" "$matrix" > "$matrix/summary.json"
date -Is > "$matrix/complete"
