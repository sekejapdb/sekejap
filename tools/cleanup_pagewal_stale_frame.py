"""Keep failures and final comparisons; remove verified redundant benchmark DBs."""
import hashlib
import json
from pathlib import Path
import shutil
import sys

base = Path(sys.argv[1]).resolve()
mode = sys.argv[2]
assert str(base) in (
    '<scratch>',
    '<scratch>',
)
assert mode in ('plan', 'delete') and not (base / 'cleanup.json').exists()

def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()

results = json.loads((base / 'comparison-results.json').read_text())
assert not results['failures'] and len(results['records']) == 9
remove = []
retained = []
for r in results['records']:
    report = Path(r['path'])
    assert base in report.resolve().parents and sha(report) == r['sha256']
    db = report.parent / 'db'
    assert db.is_dir() and not db.is_symlink()
    if r['repetition'] == 3:
        retained.append(str(db.relative_to(base)))
        continue
    files = []
    for f in sorted(db.rglob('*')):
        assert not f.is_symlink()
        if f.is_file():
            m = f.stat()
            files.append(dict(path=str(f.relative_to(base)), logical=m.st_size,
                allocated=m.st_blocks * 512, sha256=sha(f)))
    remove.append(dict(path=str(db.relative_to(base)), report_sha256=r['sha256'], files=files))
assert len(remove) == 6
result = dict(base=str(base), mode=mode, removed_databases=len(remove),
    removed_logical_bytes=sum(f['logical'] for r in remove for f in r['files']),
    removed_allocated_bytes=sum(f['allocated'] for r in remove for f in r['files']),
    removed=remove, retained=retained,
    also_retained='All failures, exact endpoint reproductions, source archives, binaries, reports and logs')
if mode == 'delete':
    for r in remove:
        shutil.rmtree(base / r['path'])
    assert all(not (base / r['path']).exists() for r in remove)
    assert all((base / p).is_dir() for p in retained)
    (base / 'cleanup.json').write_text(json.dumps(result, indent=2) + '\n')
else:
    (base / 'cleanup-plan.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({k: v for k, v in result.items() if k not in ('removed', 'retained')}, indent=2))
