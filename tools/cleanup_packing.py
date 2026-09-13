"""Remove audited disposable packing databases; retain the principal 400K pair."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys

base = Path(sys.argv[1]).resolve()
mac = Path('<scratch>')
pi = Path('<scratch>')
assert base in (mac, pi)
audit = json.loads(Path(sys.argv[2]).read_text())
assert (audit['arms'], audit['compared_states']) == ((34, 164) if base == mac else (6, 38))
keep = 'mixed-400000-r1' if base == mac else 'mixed-400000'
paths = []
for d in audit['results']:
    if d['label'] == keep:
        continue
    parent = base / (d['label'] if d['label'].startswith('ablation-') else f"matrix/{d['label']}")
    p = parent / f"{d['engine']}-{d['rows']}-{'on' if d['timestamps'] else 'off'}.json"
    actual = json.loads(p.read_text())
    assert actual['phases'] == d['phases'] and actual['reopen_verification'] == d['reopen_verification']
    paths.append(p)
if base == mac:
    smoke = [base / 'smoke-held' / f'{e}-10000-on.json' for e in ('e4', 'sqlite')]
    a, b = [json.loads(p.read_text()) for p in smoke]
    assert a['reopen_verification']['rows'] == b['reopen_verification']['rows'] == 10000
    for x, y in zip(a['phases'], b['phases']):
        assert x['verification']['crc32c'] == y['verification']['crc32c']
        if x['snapshot_verification']:
            assert x['snapshot_verification']['crc32c'] == y['snapshot_verification']['crc32c']
    paths += smoke
assert len(paths) == (34 if base == mac else 4)
manifest = dict(complete=False, retained=f'matrix/{keep}: both engines; test/fault evidence retained', databases=[])
for report in paths:
    directory = report.with_suffix('')
    assert directory.is_dir() and not directory.is_symlink() and directory.resolve().is_relative_to(base)
    files = []
    for p in sorted(directory.rglob('*')):
        assert not p.is_symlink()
        if p.is_file():
            s = p.stat()
            files.append(dict(path=str(p.relative_to(base)), logical=s.st_size, allocated=s.st_blocks*512))
    manifest['databases'].append(dict(path=str(directory.relative_to(base)), files=files,
        evidence_sha256=hashlib.sha256(report.read_bytes()).hexdigest()))
manifest['logical_bytes'] = sum(f['logical'] for d in manifest['databases'] for f in d['files'])
manifest['allocated_bytes'] = sum(f['allocated'] for d in manifest['databases'] for f in d['files'])
pending = base / 'cleanup-pending.json'
assert not pending.exists() and not (base/'cleanup.json').exists()
pending.write_text(json.dumps(manifest, indent=2)+'\n')
with pending.open('rb') as f:
    os.fsync(f.fileno())
for report in paths:
    shutil.rmtree(report.with_suffix(''))
    assert not report.with_suffix('').exists()
manifest['complete'] = True
(base/'cleanup.json').write_text(json.dumps(manifest, indent=2)+'\n')
print(json.dumps({k:v for k,v in manifest.items() if k != 'databases'}))
