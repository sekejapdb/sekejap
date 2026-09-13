"""F1 primitive-only comparisons. No collection/API or seven-law promotion claim."""
import hashlib,json,os,shutil,subprocess,sys,tarfile
from pathlib import Path
root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
source=Path('/tmp/e4-pagewal-candidate')
mode=sys.argv[1]
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
if mode=='smoke':
    assert not (base/'pilot').exists()
    shutil.copy2('/tmp/e4-commit-candidate/target/release/pagewal_bench',base/'pilot')
    files=[source/'Cargo.toml',source/'Cargo.lock']
    for folder in ('src','kernel','tests'):
        files += [p for p in (source/folder).rglob('*') if p.is_file()]
    members={str(p.relative_to(source)):sha(p) for p in files}
    archive=base/'pilot-source.tar.gz'
    with tarfile.open(archive,'w:gz') as t:
        for p in files:t.add(p,arcname=str(p.relative_to(source)),recursive=False)
    with tarfile.open(archive) as t:
        assert {m.name:hashlib.sha256(t.extractfile(m).read()).hexdigest() for m in t.getmembers()}==members
    (base/'provenance.json').write_text(json.dumps(dict(source_members=members,archive=str(archive),archive_sha256=sha(archive),binary_sha256=sha(base/'pilot')),indent=2)+'\n')
    specs=[('text32-10k',10000,32,4,'mixed',1000,1),('text256-40k',40000,256,4,'mixed',1000,1),('resize-1k',1000,256,6,'resize',100,1)]
elif mode=='matrix':
    assert (base/'smoke.complete').exists()
    specs=[('load-100k',100000,256,0,'load',1000,3),('updates-100k',100000,256,4,'updates',1000,3),
           ('mixed-100k',100000,256,4,'mixed',1000,3),('sustain-400k',400000,256,12,'mixed',1000,1)]
else:raise SystemExit('smoke or matrix')
prov=json.loads((base/'provenance.json').read_text())
assert sha(base/'pilot')==prov['binary_sha256']
for n,h in prov['source_members'].items():assert sha(source/n)==h,n
results=[];comparisons=0
for label,n,size,cycles,case,batch,reps in specs:
    for rep in range(1,reps+1):
        arms=['e4','pagewal','sqlite'];arms=arms[rep-1:]+arms[:rep-1]
        pair=[]
        for arm in arms:
            dest=base/mode/f'{label}-r{rep}'/arm
            assert not dest.exists();dest.parent.mkdir(parents=True,exist_ok=True)
            env=dict(os.environ,TMPDIR=str(base/'tmp'),SQLITE_TMPDIR=str(base/'tmp'))
            with (dest.parent/(arm+'.log')).open('w') as log:
                subprocess.run([str(base/'pilot'),str(dest),arm,str(n),str(size),str(cycles),case,str(batch)],env=env,stdout=log,stderr=log,check=True)
            p=dest/'report.json';d=json.loads(p.read_text());assert d['rows']==n and d['cycles']==cycles and d['batch']==batch
            assert len(d['phases'])==cycles+1 and d['cache_bytes']==8388608
            for i,phase in enumerate(d['phases']):
                expected=[n,0,0] if i==0 else [n//10,n//5,n//10] if case=='mixed' else [0,n//5,0]
                assert phase['cycle']==i and phase['create_update_delete_counts']==expected
                assert phase['verification']['rows']==n
                assert phase['peak']['errors']==0
            assert d['reopen_verification']==d['phases'][-1]['verification']
            results.append(dict(label=label,rep=rep,arm=arm,report=str(p),sha256=sha(p)))
            pair.append(d);print('completed',label,rep,arm,flush=True)
        for i in range(cycles+1):assert len({p['phases'][i]['verification']['crc32c'] for p in pair})==1;comparisons+=1
        assert len({p['reopen_verification']['crc32c'] for p in pair})==1;comparisons+=1
(base/(mode+'-results.json')).write_text(json.dumps(dict(reports=results,three_way_states=comparisons),indent=2)+'\n')
(base/(mode+'.complete')).write_text('All row/state checks passed\n')
print(json.dumps(dict(mode=mode,arms=len(results),three_way_states=comparisons)))
