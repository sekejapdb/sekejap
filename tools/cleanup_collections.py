#!/usr/bin/env python3
"""Remove only verified disposable collection-matrix databases; retain the 12-cycle pair."""
import hashlib
import json
import os
import shutil
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
BASE = Path('<scratch>').resolve()
assert str(BASE) == '<scratch>'
summary = json.loads((ROOT / 'docs/COLLECTION_RESULTS.json').read_text())
assert summary['arms'] == 16 and summary['paired_state_checks'] == 40
long = json.loads((ROOT / 'docs/COLLECTION_SUSTAINED_RESULTS.json').read_text())
assert long['paired_state_checks'] == 14
paths = []
for d in summary['results']:
    report = Path(d['evidence'])
    actual = json.loads(report.read_text())
    assert actual['phases'] == d['phases'] and actual['reopen_verification'] == d['reopen_verification']
    paths.append((report.with_suffix(''), report))
for engine in ('e4', 'sqlite'):
    report = BASE / 'smoke' / f'{engine}-10000-off.json'
    d = json.loads(report.read_text())
    assert d['reopen_verification']['rows'] == 10000
    paths.append((report.with_suffix(''), report))
assert len(paths) == 18
manifest = {'scope':'16 repeated-matrix arms and 2 verified smoke arms only',
            'retained':'sustained/e4-400000-off and sustained/sqlite-400000-off for occupancy follow-up',
            'databases':[], 'logical_bytes':0,'allocated_bytes':0}
for directory, report in paths:
    assert not directory.is_symlink() and directory.resolve().is_relative_to(BASE)
    assert directory.is_dir()
    files = []
    for p in sorted(directory.rglob('*')):
        assert not p.is_symlink()
        if p.is_file():
            s = p.stat()
            files.append({'path':str(p.relative_to(BASE)), 'logical':s.st_size,'allocated':s.st_blocks*512})
    logical = sum(x['logical'] for x in files)
    allocated = sum(x['allocated'] for x in files)
    manifest['databases'].append({'path':str(directory.relative_to(BASE)),
        'evidence_sha256':hashlib.sha256(report.read_bytes()).hexdigest(),
        'files':files,'logical_bytes':logical,'allocated_bytes':allocated})
    manifest['logical_bytes'] += logical
    manifest['allocated_bytes'] += allocated
pending = BASE / 'cleanup-pending.json'
assert not pending.exists() and not (BASE / 'cleanup.json').exists()
pending.write_text(json.dumps(manifest,indent=2)+'\n')
with pending.open('rb') as f: os.fsync(f.fileno())
for directory, _ in paths:
    shutil.rmtree(directory)
    assert not directory.exists()
manifest['complete'] = True
(BASE / 'cleanup.json').write_text(json.dumps(manifest,indent=2)+'\n')
(ROOT / 'docs/COLLECTION_CLEANUP.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps({k:v for k,v in manifest.items() if k != 'databases'}))
