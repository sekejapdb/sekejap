import hashlib,json,tarfile
from pathlib import Path
root=Path('<home>/')
source=Path('/tmp/e4-free-candidate')
base=Path('<scratch>')
target=base/'candidate-source.tar.gz'
assert not target.exists()
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
files=[source/'Cargo.toml',source/'Cargo.lock']
for folder in ('src','kernel','tests'):
    files += [p for p in (source/folder).rglob('*') if p.is_file() and
        not any(x in ('target','__pycache__') or x.startswith('._') for x in p.relative_to(source).parts)]
hashes={str(p.relative_to(source)):sha(p) for p in files}
with tarfile.open(target,'w:gz') as t:
    for p in files:t.add(p,arcname=str(p.relative_to(source)),recursive=False)
with tarfile.open(target) as t:
    assert {m.name:hashlib.sha256(t.extractfile(m).read()).hexdigest() for m in t.getmembers()}==hashes
production=json.loads((root/'docs/WRITE_PATH_PROVENANCE.json').read_text())['source_members']
for name,h in production.items():
    if name.startswith(('src/','kernel/','tests/')) or name=='CONTRACT.md':assert sha(root/name)==h,name
record=dict(archive=str(target),archive_sha256=sha(target),source_members=hashes,
    production_code_unchanged=True,binaries={n:sha(base/n) for n in ('baseline','candidate')})
(base/'staged.json').write_text(json.dumps(record,indent=2)+'\n')
print(record['archive_sha256'])
