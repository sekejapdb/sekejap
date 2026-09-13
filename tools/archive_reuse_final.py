"""Seal accepted source and retain the final reports after audited cleanup."""
import hashlib, json, shutil, tarfile
from pathlib import Path

root = Path(__file__).resolve().parents[1]
base = Path('<scratch>')
record = json.loads((root/'docs/REUSE_PROVENANCE.json').read_text())
old = json.loads((root/'docs/WRITE_PATH_PROVENANCE.json').read_text())['source_members']
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
for name, h in record['candidate_source_members'].items():
    expected = old[name] if name == 'src/bin/collections.rs' else h
    assert sha(root/name) == expected, name
assert sha(root/'CONTRACT.md') == old['CONTRACT.md']
for platform, name in [('Mac', 'REUSE_CLEANUP.json'), ('Pi', 'REUSE_PI_CLEANUP.json')]:
    clean = json.loads((root/'docs'/name).read_text())
    assert clean['complete']
    if platform == 'Mac':
        for row in clean['removed']: assert not (base/row['path']).exists()
        for arm in ('baseline','candidate','sqlite'):
            engine = 'sqlite' if arm == 'sqlite' else 'e4'
            assert (base/f'matrix/mixed-400000/{arm}/{engine}-400000-off').is_dir()

archive = base/'reuse-final-source.tar.gz'
assert not archive.exists()
files = [root/n for n in ('Cargo.toml','Cargo.lock','README.md','CONTRACT.md')]
for folder in ('src','kernel','tests'):
    files += [p for p in (root/folder).rglob('*') if p.is_file()
              and not any(x in ('target','__pycache__') or x.startswith('._')
                          for x in p.relative_to(root).parts)]
hashes = {str(p.relative_to(root)):sha(p) for p in files}
with tarfile.open(archive,'w:gz') as t:
    for p in files: t.add(p,arcname=str(p.relative_to(root)),recursive=False)
with tarfile.open(archive) as t:
    assert {m.name:hashlib.sha256(t.extractfile(m).read()).hexdigest()
            for m in t.getmembers()} == hashes
record.update(accepted_archive=str(archive), accepted_archive_sha256=sha(archive), source_members=hashes)
(root/'docs/REUSE_PROVENANCE.json').write_text(json.dumps(record,indent=2)+'\n')
(base/'decision.json').write_text(json.dumps(record,indent=2)+'\n')
for p in list((root/'docs').glob('REUSE_*.json')) + [root/'docs/PERSISTENT_FREELIST.md'] + list((root/'tools').glob('*reuse*')):
    if p.is_file(): shutil.copyfile(p,base/p.name)
print(json.dumps(dict(accepted_archive=str(archive),sha256=sha(archive),members=len(hashes))))
