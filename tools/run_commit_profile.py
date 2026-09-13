"""Run and aggregate the diagnostic separately from uninstrumented ablations."""
import json, os, shutil, subprocess, tarfile
from pathlib import Path
base=Path('<scratch>')
src=Path('/tmp/e4-stage-candidate')
shutil.copy2('/tmp/e4-commit-candidate/target/release/collections',base/'profile')
with tarfile.open(base/'profile-source.tar.gz','w:gz') as t:
    for folder in ('src','kernel'):
        for p in (src/folder).rglob('*'):
            if p.is_file():t.add(p,arcname=str(p.relative_to(src)),recursive=False)
    for n in ('Cargo.toml','Cargo.lock'):t.add(src/n,arcname=n)
summary={}
for batch,n,cycles in ((1,1000,2),(1000,100000,4)):
    label=f'profile-{batch}'
    env=dict(os.environ,TMPDIR=str(base/'tmp'),SQLITE_TMPDIR=str(base/'tmp'),COLLECTION_BATCH=str(batch))
    with (base/(label+'.log')).open('w') as out, (base/(label+'.stages')).open('w') as err:
        subprocess.run([str(base/'profile'),str(base/label),'e4',str(n),'off',str(cycles),'mixed','none'],env=env,stdout=out,stderr=err,check=True)
    d=json.loads((base/label/f'e4-{n}-off.json').read_text())
    assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
    totals={}; counts={}
    for line in (base/(label+'.stages')).read_text().splitlines():
        if not line.startswith('STAGE '):continue
        _,name,ns=line.split();totals[name]=totals.get(name,0)+int(ns);counts[name]=counts.get(name,0)+1
    summary[label]=dict(seconds={k:v/1e9 for k,v in totals.items()},counts=counts,diagnostic_only=True)
for n,s in json.loads((base/'profile-originals.json').read_text()).items():
    (src/'kernel/src'/n).write_text(s)
(base/'profile-summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary,indent=2))
