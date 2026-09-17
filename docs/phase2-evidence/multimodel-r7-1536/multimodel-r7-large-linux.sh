#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
# Keep measured work isolated from both compilation and compatibility tests.
while [ ! -f "$R/lifecycle-r1.exit" ]; do sleep 5; done
[ "$(cat "$R/multimodel-r7.exit")" = 0 ]
[ "$(cat "$R/phase2-workspace-r1.exit")" = 0 ]
cd "$R/multimodel-r7-src"
sha256sum -c "$R/multimodel-r7-binaries/binary.sha256"
for specification in 1536:2000:1536 1000000:1000000:32; do
 IFS=: read -r label rows dimension <<< "$specification"
 python3 tools/phase2_multimodel_bench.py --bin "$R/multimodel-r7-binaries/bench" --root "$R/multimodel-r7-$label" --sizes "$rows" --reader-modes none,batch,short,held --trials 3 --dimension "$dimension" --include-resumable > "$R/logs/multimodel-r7-$label.log" 2>&1
 python3 - "$R/multimodel-r7-$label/report.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); assert d['capture_complete'] and len(d['arms'])==36
for arm in d['arms']:
 result=arm['raw']['result']; assert arm['returncode']==0
 if result.get('refused',False): assert result['refusal_oracle']['committed_state_verified'] is True
 else: assert len(result['crud'])==3
print(d['completion_counts'])
PY
 printf '0\n' > "$R/multimodel-r7-$label.exit"
done
sha256sum -c "$R/multimodel-r7-binaries/binary.sha256"
