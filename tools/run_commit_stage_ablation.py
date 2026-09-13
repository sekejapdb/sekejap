"""Serial, reversed-order E4/SQLite comparison with full state checks."""
import hashlib, json, os, shutil, statistics, subprocess, tarfile
from pathlib import Path

root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
source=Path('/tmp/e4-stage-candidate')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
old=json.loads((root/'docs/REUSE_PROVENANCE.json').read_text())['source_members']
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')) or n=='CONTRACT.md':assert sha(root/n)==h,n
files=[source/'Cargo.toml',source/'Cargo.lock']
for folder in ('src','kernel','tests'):
    files += [p for p in (source/folder).rglob('*') if p.is_file()]
members={str(p.relative_to(source)):sha(p) for p in files}
assert {n for n,h in members.items() if old.get(n)!=h} == {'src/bin/collections.rs','kernel/src/wal.rs','kernel/src/wal_reset_tests.rs'}
archive=base/'candidate-source.tar.gz';assert not archive.exists()
with tarfile.open(archive,'w:gz') as t:
    for p in files:t.add(p,arcname=str(p.relative_to(source)),recursive=False)
with tarfile.open(archive) as t:
    assert {m.name:hashlib.sha256(t.extractfile(m).read()).hexdigest() for m in t.getmembers()}==members
shutil.copy2('/tmp/e4-commit-candidate/target/release/collections',base/'candidate')
assert sha(base/'candidate')!=sha(base/'baseline')
provenance=dict(archive=str(archive),archive_sha256=sha(archive),source_members=members,
    binaries={n:sha(base/n) for n in ('baseline','candidate','profile')},production_unchanged=True)
(base/'provenance.json').write_text(json.dumps(provenance,indent=2)+'\n')
(base/'matrix').mkdir()
reports=[];checks=0
# Separate load-only case; unchanged transaction size within every comparison.
specs=[('load',100000,0,1000,'load'),('updates',100000,4,1000,'updates'),
       ('mixed',100000,4,1000,'mixed'),('small',1000,2,1,'mixed')]
for rep in (1,2):
    for label,n,cycles,batch,case in specs:
        pair=[]
        for arm in (('baseline','candidate','sqlite') if rep==1 else ('sqlite','candidate','baseline')):
            engine='sqlite' if arm=='sqlite' else 'e4'
            binary=base/('baseline' if arm=='sqlite' else arm)
            dest=base/f'matrix/{label}-r{rep}/{arm}'
            env=dict(os.environ,TMPDIR=str(base/'tmp'),SQLITE_TMPDIR=str(base/'tmp'),COLLECTION_BATCH=str(batch))
            with (base/f'matrix/{label}-r{rep}-{arm}.log').open('w') as log:
                subprocess.run([str(binary),str(dest),engine,str(n),'off',str(cycles),case,'none'],env=env,stdout=log,stderr=log,check=True)
            p=dest/f'{engine}-{n}-off.json';d=json.loads(p.read_text())
            for k,v in dict(rows=n,cycles=cycles,case=case,reader='none',transaction_operations=batch,
                cache_bytes=8388608,timestamps=False,collections=2,vector_dim=4,change_vectors=False).items():assert d[k]==v,(p,k)
            assert len(d['phases'])==cycles+1
            for i,phase in enumerate(d['phases']):
                assert phase['cycle']==i and phase['verification']['rows']==n
                assert phase['operations']==(n if i==0 else n//5 if case=='updates' else n*2//5)
                assert phase['snapshot_verification'] is None
            assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
            assert d['reopen_verification']['rows']==n
            peaks=[v['peak'] for v in d['phases']]+[d['alter_peak']]
            assert all(v['sample_errors']==0 and v['samples']>0 for v in peaks)
            reports.append(dict(label=label,rep=rep,arm=arm,rows=n,cycles=cycles,batch=batch,
                load_seconds=d['phases'][0]['seconds'],mutation_seconds=sum(v['seconds'] for v in d['phases'][1:]),
                peak_logical=max(v['sampled_peak_logical'] for v in peaks),peak_allocated=max(v['sampled_peak_allocated'] for v in peaks),
                final_logical=d['alter_final_logical'],final_allocated=d['alter_final_allocated'],
                evidence=str(p),sha256=sha(p)))
            pair.append(d)
            print(f'completed {label}-r{rep} {arm}',flush=True)
        for i in range(cycles+1):
            assert len({v['phases'][i]['verification']['crc32c'] for v in pair})==1;checks+=1
        assert len({v['reopen_verification']['crc32c'] for v in pair})==1;checks+=1
summary=[]
for label,*_ in specs:
    row=dict(label=label)
    for arm in ('baseline','candidate','sqlite'):
        rs=[r for r in reports if r['label']==label and r['arm']==arm]
        row[arm]={k:statistics.mean(r[k] for r in rs) for k in ('load_seconds','mutation_seconds','peak_logical','peak_allocated','final_logical','final_allocated')}
    key='load_seconds' if label=='load' else 'mutation_seconds'
    row['reduction_pct']=100*(1-row['candidate'][key]/row['baseline'][key])
    summary.append(row)
result=dict(platform='Mac',arms=len(reports),state_comparisons=checks,summary=summary,reports=reports)
for p in (base/'results.json',root/'docs/COMMIT_STAGE_RESULTS.json'):p.write_text(json.dumps(result,indent=2)+'\n')
(base/'matrix.complete').write_text('All 24 arms and three-way state checks passed\n')
print(json.dumps(summary,indent=2))
