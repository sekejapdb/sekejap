"""Verify and remove redundant fixed-work DBs, preserving reports and diagnostic cases.

Usage: cleanup_foundation_scale.py BASE plan|delete
Only accepts this loop's two authorized roots; never touches pre-existing loops.
Keeps all third repetitions at 10K/100K/1M, including parity failures. The 10M
confirmation is disposable after every oracle and paired report has passed.
"""
import hashlib
import json
from pathlib import Path
import sys


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


base = Path(sys.argv[1]).resolve()
assert str(base) in ['<scratch>',
    '<scratch>']
action = sys.argv[2]; assert action in ['plan', 'delete']
assert not (base/'cleanup.json').exists(), 'cleanup already recorded'
victims = []; kept = []
for profile, expected in [('scale',36),('large',4)]:
    path = base/profile/'results.json'; d = json.loads(path.read_text())
    assert d['status']=='PASS' and not d['failures'] and len(d['records'])==expected
    pairs = {}
    for record in d['records']:
        report = record['report']; key=(record['repetition'], report['rows'], report['locality'])
        pairs.setdefault(key,[]).append(report['verification'])
        rp = Path(record['path']).resolve()
        assert rp.is_relative_to(base/profile) and rp.name=='report.json'
        assert sha(rp)==record['report_sha256'] and json.loads(rp.read_text())==report
        assert report['verification']['rows']==report['rows']
        db = rp.parent/'db'
        if profile=='scale' and record['repetition']==3:
            kept.append(str(db)); continue
        files = []
        for p in sorted(db.iterdir()):
            assert p.is_file() and not p.is_symlink()
            m=p.stat(); files.append(dict(path=str(p),logical=m.st_size,allocated=m.st_blocks*512,sha256=sha(p),
                identity=[m.st_dev,m.st_ino,m.st_size,m.st_mtime_ns]))
        victims.append(dict(path=str(db),report_sha256=record['report_sha256'],files=files))
    assert all(len(pair)==2 and pair[0]==pair[1] for pair in pairs.values())
manifest=dict(action=action, kept=kept, deleted=victims,
    logical_bytes=sum(f['logical'] for v in victims for f in v['files']),
    allocated_bytes=sum(f['allocated'] for v in victims for f in v['files']))
(base/'cleanup-plan.json').write_text(json.dumps(manifest,indent=2)+'\n')
if action=='delete':
    for v in victims:
        for f in v['files']:
            path=Path(f['path']); m=path.stat()
            assert [m.st_dev,m.st_ino,m.st_size,m.st_mtime_ns]==f['identity'], 'file changed after hashing'
            path.unlink()
        Path(v['path']).rmdir()
    (base/'cleanup.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps(dict(databases=len(victims),retained=len(kept),logical_bytes=manifest['logical_bytes'],
    allocated_bytes=manifest['allocated_bytes'],action=action)))
