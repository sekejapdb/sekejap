"""Archive hashes and clean verified disposable native diagnostic fixtures."""
import hashlib, json, shutil, subprocess, sys
from pathlib import Path
root = Path(sys.argv[1]).resolve()
assert str(root) in ['<scratch>', '<scratch>']
assert (root / 'evidence.tar.gz').exists()
assert (root / 'c-buffered.log').read_text().count('PASS worker=') == 4
def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda:f.read(1 << 20), b''): h.update(b)
    return h.hexdigest()
paths = [root / 'c-buffered']
if str(root).startswith('/home/'):
    for run in [2, 3]:
        assert 'test result: ok.' in (root / f'one-arena-{run}.log').read_text()
        paths.append(root / f'one-arena-{run}')
removed = []
for path in paths:
    files = {str(p.relative_to(path)):dict(sha256=digest(p),logical=p.stat().st_size,allocated=p.stat().st_blocks*512) for p in path.rglob('*') if p.is_file()}
    assert files
    removed.append(dict(path=str(path),files=files,allocated=sum(f['allocated'] for f in files.values())))
result = dict(removed=removed, allocated_reclaimed=sum(r['allocated'] for r in removed), evidence_sha256=digest(root/'evidence.tar.gz'), c_source_sha256=digest(root/'buffered_rewrite_probe.c'), c_binary_sha256=digest(root/'buffered-rewrite-probe'), compiler=subprocess.check_output(['cc','--version'],text=True))
# Write the manifest before deletion, then add the completion marker.
(root/'cleanup.json').write_text(json.dumps(result,indent=2)+'\n')
for path in paths: shutil.rmtree(path)
(root/'cleanup-complete').touch()
print(json.dumps(result))
