"""Preserve all ablations and verify the accepted engine was not changed."""
import hashlib,json,tarfile
from pathlib import Path
root=Path(__file__).resolve().parents[1]
source=Path('/tmp/e4-batch-candidate')
base=Path('<scratch>')
old=json.loads((root/'docs/REUSE_PROVENANCE.json').read_text())['source_members']
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')) or n=='CONTRACT.md': assert sha(root/n)==h,n
assert sha(source/'kernel/src/page.rs')==sha(base/'combined-page.rs')
assert sha(source/'kernel/src/btree.rs')==sha(base/'combined-btree.rs')
files=[source/'Cargo.toml',source/'Cargo.lock']
for folder in ('src','kernel','tests'):
    files += [p for p in (source/folder).rglob('*') if p.is_file() and not any(
        x in ('target','__pycache__') or x.startswith('._') for x in p.relative_to(source).parts)]
members={str(p.relative_to(source)):sha(p) for p in files}
changed={n for n,h in members.items() if h!=old.get(n)}
assert changed=={'src/bin/collections.rs','kernel/src/page.rs','kernel/src/btree.rs',
                 'kernel/tests/page_repack.rs','kernel/tests/cell_replace.rs'},changed
archive=base/'combined-source.tar.gz';assert not archive.exists()
with tarfile.open(archive,'w:gz') as t:
    for p in files:t.add(p,arcname=str(p.relative_to(source)),recursive=False)
with tarfile.open(archive) as t:
    assert {m.name:hashlib.sha256(t.extractfile(m).read()).hexdigest() for m in t.getmembers()}==members
bins={a:sha(base/a) for a in ('baseline','scratch','cell','combined')}
assert len(set(bins.values()))==4
variants={a:{'kernel/src/page.rs':sha(base/f'{a}-page.rs'),
             'kernel/src/btree.rs':old['kernel/src/btree.rs'] if a=='scratch' else sha(base/'combined-btree.rs')}
          for a in ('scratch','cell','combined')}
record=dict(archive=str(archive),archive_sha256=sha(archive),source_members=members,
            variant_engine_hashes=variants,binaries=bins,production_code_unchanged=True)
(base/'staged.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({k:v for k,v in record.items() if k!='source_members'}))
