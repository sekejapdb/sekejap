"""Archive the isolated candidate and verify the unchanged accepted source."""
import hashlib, json, tarfile, sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
source = Path('/tmp/e4-reuse-candidate')
base = Path('<scratch>')
archive = base / (sys.argv[1] if len(sys.argv) > 1 else 'candidate-source.tar.gz')
assert not archive.exists()
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
old = json.loads((root/'docs/WRITE_PATH_PROVENANCE.json').read_text())['source_members']
for name, h in old.items():
    if name.startswith(('src/', 'kernel/', 'tests/')) or name == 'CONTRACT.md':
        assert sha(root/name) == h, name
files = [source/'Cargo.toml', source/'Cargo.lock']
for folder in ('src', 'kernel', 'tests'):
    files += [p for p in (source/folder).rglob('*') if p.is_file()
              and not any(x in ('target', '__pycache__') or x.startswith('._')
                          for x in p.relative_to(source).parts)]
hashes = {str(p.relative_to(source)): sha(p) for p in files}
with tarfile.open(archive, 'w:gz') as t:
    for p in files: t.add(p, arcname=str(p.relative_to(source)), recursive=False)
with tarfile.open(archive) as t:
    assert {m.name: hashlib.sha256(t.extractfile(m).read()).hexdigest()
            for m in t.getmembers()} == hashes
record = dict(archive=str(archive), archive_sha256=sha(archive),
              source_members=hashes, production_code_unchanged=True)
(base/'staged.json').write_text(json.dumps(record, indent=2)+'\n')
print(record['archive_sha256'])
