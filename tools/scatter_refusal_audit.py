"""Preserve refused source bytes; reopen independent copies and verify all rows."""
import hashlib, json, shutil, struct, subprocess, sys
from pathlib import Path
base = Path(sys.argv[1]).resolve()
assert str(base) in ['<scratch>', '<scratch>']
out = base / 'refusal-audit'; out.mkdir()
def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda: f.read(1048576), b''): h.update(b)
    return h.hexdigest()
reports = []
for failure in json.loads((base / 'probe/results.json').read_text())['failures']:
    source = Path(failure['path']) / 'db'
    dest = out / source.parent.name
    before = {p.name: digest(p) for p in source.iterdir() if p.is_file()}
    sizes = {p.name: dict(logical=p.stat().st_size, allocated=p.stat().st_blocks*512) for p in source.iterdir() if p.is_file()}
    frames = 0; commits = 0; pages = set()
    with (source / 'wal').open('rb') as f:
        while b := f.read(4128):
            assert len(b) == 4128
            kind, page = struct.unpack_from('<II', b, 8)
            frames += 1
            if kind == 1: pages.add(page)
            if kind == 2: commits += 1
    shutil.copytree(source, dest)
    command = [str(base / 'targets/combined/release/scatter_refusal_check'), str(dest), failure['case'].split('-')[1]]
    result = subprocess.run(command, text=True, capture_output=True)
    (out / (source.parent.name + '.log')).write_text(result.stdout + result.stderr)
    assert result.returncode == 0, result.stderr
    assert before == {p.name: digest(p) for p in source.iterdir() if p.is_file()}
    reports.append(dict(case=failure['case'], arm=failure['arm'], source=str(source), copy=str(dest), source_sha256=before, sizes=sizes,
        wal_frames=frames, wal_unique_page_numbers=len(pages), wal_commit_frames=commits,
        note='Header inventory only; native reopen independently checks frame/page checksums', verification=json.loads(result.stdout), source_unchanged=True))
    (out / 'results.json').write_text(json.dumps(reports, indent=2)+'\n')
print('PASS: all refused transactions reopen to exact loaded state on copies')
