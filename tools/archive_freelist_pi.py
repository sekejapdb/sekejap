import hashlib,json,tarfile
from pathlib import Path
root=Path('<scratch>')
art=root/'artifacts/freelist-20260912'
assert (art/'matrix.complete').exists()
for arm in ('baseline','candidate'):
    source=root/f'free-{arm}-src'
    files=[source/'Cargo.toml',source/'Cargo.lock']
    for folder in ('src','kernel','tests'):
        files += [p for p in (source/folder).rglob('*') if p.is_file() and not any(x in ('target','__pycache__') or x.startswith('._') for x in p.relative_to(source).parts)]
    hashes={str(p.relative_to(source)):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
    (art/f'{arm}-source.json').write_text(json.dumps(hashes,indent=2)+'\n')
target=art/'pi-evidence.tar.gz'
assert not target.exists()
files=[p for p in art.iterdir() if p.is_file() and p.suffix in ('.log','.json','.sha256','.status','.complete')]
files+=list((art/'matrix').glob('*.log'))+list((art/'matrix').glob('*/*/*.json'))
with tarfile.open(target,'w:gz') as t:
    for p in files:t.add(p,arcname=str(p.relative_to(art)),recursive=False)
print(hashlib.sha256(target.read_bytes()).hexdigest())
