"""Delete only audited generated databases; keep reports and the main comparison."""
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
assert audit['arms'] == (48 if base == mac else 12)
keep = 'mixed-400000-r1' if base == mac else 'mixed-400000'
paths = []
for d in audit['results']:
    if d['label'] == keep and (base == mac or d['arm'] != 'baseline'):
        continue
    p = base/'matrix'/d['label']/d['arm']/f"{d['engine']}-{d['rows']}-{'on' if d['timestamps'] else 'off'}.json"
    actual = json.loads(p.read_text())
    assert actual['phases'] == d['phases'] and actual['reopen_verification'] == d['reopen_verification']
    paths.append(p)
if base == mac:
    for label, n in [('profile-before',400000),('probe-before',100000),('probe-delta',100000)]:
        p = base/label/f'e4-{n}-off.json'
        d = json.loads(p.read_text())
        reference = next(r for r in audit['results'] if r['label']==f'mixed-{n}-r1' and r['arm']=='baseline')
        for phase in d['phases']:
            assert phase['verification']['rows'] == n
            assert phase['verification']['crc32c'] == reference['phases'][phase['cycle']]['verification']['crc32c']
        assert d['reopen_verification']['crc32c'] == d['phases'][-1]['verification']['crc32c']
        paths.append(p)
if base == pi:
    invalid=list((base/'matrix-invalid-identical-binaries').glob('*/*/*.json'))
    assert len(invalid)==12
    for p in invalid:
        d=json.loads(p.read_text())
        assert len(d['phases'])==d['cycles']+1
        assert all(x['verification']['rows']==d['rows'] for x in d['phases'])
        assert d['reopen_verification']['rows']==d['rows']
        assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
    paths += invalid
assert len(paths) == (48 if base == mac else 22)
manifest = dict(complete=False, invalid_performance_arms_removed=0 if base==mac else 12,
    retained=f'matrix/{keep}: main comparison; all reports, sources and fault logs retained', databases=[])
for report in paths:
    directory = report.with_suffix('')
    assert directory.is_dir() and not directory.is_symlink() and directory.resolve().is_relative_to(base)
    files = []
    for p in sorted(directory.rglob('*')):
        assert not p.is_symlink()
        if p.is_file():
            s=p.stat()
            files.append(dict(path=str(p.relative_to(base)), logical=s.st_size, allocated=s.st_blocks*512))
    manifest['databases'].append(dict(path=str(directory.relative_to(base)), files=files,
        evidence_sha256=hashlib.sha256(report.read_bytes()).hexdigest()))
for key in ('logical','allocated'):
    manifest[key+'_bytes']=sum(f[key] for d in manifest['databases'] for f in d['files'])
pending=base/'cleanup-pending.json'
assert not pending.exists() and not (base/'cleanup.json').exists()
pending.write_text(json.dumps(manifest,indent=2)+'\n')
with pending.open('rb') as f: os.fsync(f.fileno())
for report in paths:
    shutil.rmtree(report.with_suffix(''))
    assert not report.with_suffix('').exists()
manifest['complete']=True
(base/'cleanup.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps({k:v for k,v in manifest.items() if k!='databases'}))
