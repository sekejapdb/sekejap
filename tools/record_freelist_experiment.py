"""Audit embedded-freelist trials, including negative acceptance evidence."""
import hashlib,json,statistics,sys
from pathlib import Path
root=Path(__file__).resolve().parents[1]
pi=len(sys.argv)>1 and sys.argv[1]=='pi'
base=Path('<scratch>')
evidence=base/'pi-evidence' if pi else base
specs=([('batch-1',1,1000,2,'none'),('batch-100',100,10000,3,'none'),
    ('mixed-400000',1000,400000,12,'none'),('held-100000',1000,100000,4,'held')]
    if pi else [(f'batch-{batch}-r{rep}',batch,n,cycles,'none') for rep in (1,2)
    for batch,n,cycles in ((1,1000,2),(100,10000,3),(1000,100000,4))]
    +[('mixed-400000',1000,400000,12,'none'),('held-100000',1000,100000,4,'held')])
assert (evidence/'matrix.complete').exists()
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
reports=[];checks=0
for label,batch,n,cycles,reader in specs:
    arms=[]
    for arm in ('baseline','candidate','sqlite'):
        engine='sqlite' if arm=='sqlite' else 'e4'
        p=evidence/f'matrix/{label}/{arm}/{engine}-{n}-off.json'
        d=json.loads(p.read_text())
        for k,v in dict(rows=n,engine=engine,cycles=cycles,reader=reader,
            transaction_operations=batch,cache_bytes=8388608,timestamps=False,
            collections=2,case='mixed',vector_dim=4,change_vectors=False).items():assert d[k]==v,(p,k)
        assert len(d['phases'])==cycles+1
        for i,phase in enumerate(d['phases']):
            assert phase['cycle']==i and phase['operations']==(n if i==0 else n*4//10)
            assert phase['verification']['rows']==n and phase['seconds']>0
            assert phase['peak']['sample_errors']==0 and phase['peak']['samples']>0
            if engine=='e4':
                b=phase['issued_bytes'];assert b['total']==sum(b[k] for k in ('data','wal','sidecars'))
            if i and reader=='held':
                old=phase['snapshot_verification'];assert old['rows']==n and old['crc32c']==d['phases'][0]['verification']['crc32c']
            else:assert phase['snapshot_verification'] is None
        assert d['reopen_verification']['rows']==n
        assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
        peaks=[phase['peak'] for phase in d['phases']]+[d['alter_peak']]
        if reader=='held':peaks.append(d['reader_release']['peak'])
        assert all(p['sample_errors']==0 for p in peaks)
        usage=None
        if pi:
            log=(evidence/f'matrix/{label}-{arm}.log').read_text().splitlines()
            usage=json.loads(log[-1])['resource_usage'];assert usage['exit_code']==0
        reports.append(dict(label=label,batch=batch,rows=n,cycles=cycles,reader=reader,arm=arm,
            load_seconds=d['phases'][0]['seconds'],churn_seconds=sum(p['seconds'] for p in d['phases'][1:]),
            peak_logical=max(p['sampled_peak_logical'] for p in peaks),peak_allocated=max(p['sampled_peak_allocated'] for p in peaks),
            final_logical=d['alter_final_logical'],final_allocated=d['alter_final_allocated'],
            churn_issued_bytes=None if engine=='sqlite' else sum(p['issued_bytes']['total'] for p in d['phases'][1:]),
            usage=usage,evidence=str(p),sha256=sha(p)))
        arms.append(d)
    for i in range(cycles+1):
        assert len({a['phases'][i]['verification']['crc32c'] for a in arms})==1;checks+=1
        if i and reader=='held':
            assert len({a['phases'][i]['snapshot_verification']['crc32c'] for a in arms})==1;checks+=1
    assert len({a['reopen_verification']['crc32c'] for a in arms})==1;checks+=1
assert len(reports)==(12 if pi else 24)
assert len(list((evidence/'matrix').glob('*/*/*.json')))==len(reports)
summary=[]
for label in dict.fromkeys(r['label'].split('-r')[0] for r in reports):
    entry={'case':label}
    for arm in ('baseline','candidate','sqlite'):
        rs=[r for r in reports if r['label'].split('-r')[0]==label and r['arm']==arm]
        entry[arm]={k:statistics.mean(r[k] for r in rs) for k in ('load_seconds','churn_seconds','peak_logical','peak_allocated','final_logical','final_allocated')}
        entry[arm]['runs']=len(rs)
    entry['load_improvement_pct']=100*(1-entry['candidate']['load_seconds']/entry['baseline']['load_seconds'])
    entry['churn_improvement_pct']=100*(1-entry['candidate']['churn_seconds']/entry['baseline']['churn_seconds'])
    summary.append(entry)
out=dict(platform='Pi' if pi else 'Mac',arms=len(reports),three_way_state_comparisons=checks,summary=summary,reports=reports)
name='FREELIST_PI_RESULTS.json' if pi else 'FREELIST_RESULTS.json'
(root/'docs'/name).write_text(json.dumps(out,indent=2)+'\n')
print(json.dumps({k:v for k,v in out.items() if k!='reports'},indent=2))
