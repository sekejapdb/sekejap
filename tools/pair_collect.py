"""Archive this loop's metadata, then remove only verified redundant DBs."""
import hashlib, json, os, shutil, sys, tarfile
from pathlib import Path
base = Path(sys.argv[1]).resolve()
assert str(base) in ['<scratch>', '<scratch>']
final = base/'v3'
assert (final/'instrumented-complete').is_file()
lean = json.loads((final/'baseline-lean/results.json').read_text())
assert lean['status'] == 'PASS'
qualified = json.loads((final/'qualify/results.json').read_text())
assert len(qualified['records']) == 81 and not qualified['failures']
paths = []
for root, dirs, files in os.walk(base):
    dirs[:] = [d for d in dirs if d not in ['src','targets','bin','tmp','db'] and not d.startswith('rows-')]
    for name in files:
        p = Path(root)/name
        if p.suffix in ['.json','.log','.txt','.py','.sh','.rs']: paths.append(p)
archives = {str(p.relative_to(base)): hashlib.sha256(p.read_bytes()).hexdigest() for p in base.rglob('*.tar.gz') if 'targets' not in p.parts and 'src' not in p.parts}
(base/'source-archive-hashes.json').write_text(json.dumps(archives,indent=2)+'\n')
paths.append(base/'source-archive-hashes.json')
with tarfile.open(base/'evidence.tar.gz','w:gz') as t:
    for p in sorted(set(paths)): t.add(p,arcname=str(p.relative_to(base)))
remove=[]
for rel in ['probe','v2/probe','v3/probe','v3/qualify']:
    p=base/rel/'results.json'
    if p.exists():
        r=json.loads(p.read_text());assert not r['failures']
        remove.extend(Path(x['path'])/'db' for x in r['records'])
remove.extend(Path(r['path']).parent/'db' for r in lean['records'])
for name in ['baseline-law1-lean','baseline-instrumented']:
    remove.extend((final/name).glob('rows-*'))
removed=[]
for p in remove:
    assert p.is_relative_to(base) and p.is_dir(),p
    fs=[f for f in p.rglob('*') if f.is_file()]
    item=dict(path=str(p),logical=sum(f.stat().st_size for f in fs),allocated=sum(f.stat().st_blocks*512 for f in fs))
    shutil.rmtree(p);removed.append(item)
r=dict(removed=removed,allocated_reclaimed=sum(x['allocated']for x in removed),
    evidence_sha256=hashlib.sha256((base/'evidence.tar.gz').read_bytes()).hexdigest(),
    preservation='All sources, binaries and metadata retained. No Mac failure fixtures or previous-loop databases touched.')
(base/'cleanup.json').write_text(json.dumps(r,indent=2)+'\n')
print(json.dumps(dict(removed=len(removed),allocated_reclaimed=r['allocated_reclaimed'])))
