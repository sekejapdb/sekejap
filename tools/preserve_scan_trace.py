"""Preserve this loop's failures and remove only explicitly verified successes."""
import hashlib, json, shutil, tarfile
from pathlib import Path

root = Path('<scratch>')
old = root.parent / 'scatter-resume-20260914/pair-packing-20260914'

def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''):
            h.update(b)
    return h.hexdigest()

failures = [(old / 'v3-law1-full-features', 'original-v3')]
failures += [(root / f'concurrent-0-{worker}', f'concurrent-0-{worker}') for worker in [0, 2, 3]]
manifest = {}
archive = root / 'failed-fixtures.tar.gz'
assert not archive.exists()
with tarfile.open(archive, 'w:gz', compresslevel=1) as tar:
    for source, label in failures:
        for path in sorted(source.rglob('*')):
            if path.is_file():
                name = label + '/' + str(path.relative_to(source))
                manifest[name] = dict(sha256=digest(path), logical=path.stat().st_size)
                tar.add(path, arcname=name, recursive=False)
    for run in ['c-buffered', 'c-buffered-r2']:
        for worker in range(4):
            expected = root / run / f'worker-{worker}-expected.bin'
            if expected.exists():
                for path in [expected, root / run / f'worker-{worker}-observed.bin', root / run / f'worker-{worker}.data']:
                    name = run + '/' + path.name
                    manifest[name] = dict(sha256=digest(path), logical=path.stat().st_size)
                    tar.add(path, arcname=name, recursive=False)
# Independently read every archived member before deleting anything.
with tarfile.open(archive) as tar:
    for member in tar:
        h = hashlib.sha256()
        with tar.extractfile(member) as f:
            for b in iter(lambda: f.read(1 << 20), b''):
                h.update(b)
        assert h.hexdigest() == manifest[member.name]['sha256']
(root / 'failed-fixtures-manifest.json').write_text(json.dumps(dict(files=manifest, archive_sha256=digest(archive)), indent=2) + '\n')

removed = []
for name in [f'audited-v3-r{i}' for i in range(2, 7)] + ['concurrent-0-1']:
    assert 'test result: ok.' in (root / (name + '.log')).read_text()
    path = root / name
    files = {str(p.relative_to(path)): dict(sha256=digest(p), allocated=p.stat().st_blocks*512, logical=p.stat().st_size) for p in path.rglob('*') if p.is_file()}
    removed.append(dict(path=str(path), files=files, allocated=sum(f['allocated'] for f in files.values())))
    shutil.rmtree(path)
for run in ['c-buffered', 'c-buffered-r2']:
    log = (root / (run + '.log')).read_text()
    for worker in range(4):
        if f'PASS worker={worker} ' in log:
            path = root / run / f'worker-{worker}.data'
            removed.append(dict(path=str(path), sha256=digest(path), allocated=path.stat().st_blocks*512, logical=path.stat().st_size))
            path.unlink()
(root / 'cleanup.json').write_text(json.dumps(dict(removed=removed, allocated_reclaimed=sum(r['allocated'] for r in removed), preservation='Original and new failures retained; first instrumented and uninstrumented healthy controls retained; archived failures verified before cleanup'), indent=2) + '\n')
print('Preserved failure archive', archive, 'and reclaimed', sum(r['allocated'] for r in removed), flush=True)
