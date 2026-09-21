#!/bin/bash
# Run only after all benchmark processes exit. Preserves evidence before cleanup.
set -euo pipefail
roots=(<scratch> <scratch> <scratch> <scratch>)
python3 tools/record_sustained.py "${roots[@]}"
cargo fmt --package sekejap-core --check
for root in "${roots[@]}"; do
  python3 tools/cleanup_sustained.py "$root"
done
python3 - <<'PY'
from pathlib import Path
import hashlib,json
roots=[Path('<scratch>')/n for n in ('sustained-20260910','sustained-extra-20260911','sustained-scale-20260911','sustained-gap-20260911')]
cleanup=[json.loads((r/'cleanup.json').read_text()) for r in roots]
Path('docs/SUSTAINED_CLEANUP.json').write_text(json.dumps(cleanup,indent=2)+'\n')
artifacts=[]
for r in roots:
 for p in sorted(r.iterdir()):
  if p.is_file() and (p.name.startswith('lifecycle-') or p.name.endswith('.tar.gz')):
   artifacts.append({'path':str(p),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()})
Path('docs/SUSTAINED_ARTIFACTS.json').write_text(json.dumps(artifacts,indent=2)+'\n')
print('Removed logical bytes:',sum(x['logical_bytes'] for x in cleanup))
PY
COPYFILE_DISABLE=1 tar --exclude=target --exclude=.git -czf "${roots[0]}/final-source.tar.gz" Cargo.toml Cargo.lock kernel src tests tools README.md AGENTS.md CONTRACT.md docs
shasum -a 256 "${roots[0]}/final-source.tar.gz" > "${roots[0]}/final-source.sha256"
cp docs/SUSTAINED_RESULTS.json docs/SUSTAINED_TABLES.md docs/SUSTAINED_FOUNDATION.md docs/SUSTAINED_CLEANUP.json docs/SUSTAINED_ARTIFACTS.json "${roots[0]}/"
echo 'Checked evidence archived and generated arm databases removed'
