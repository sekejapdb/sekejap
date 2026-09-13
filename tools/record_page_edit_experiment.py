"""Audit five-engine ablations against complete deterministic row oracles."""
import hashlib, json, statistics, sys
from pathlib import Path

root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
pi=len(sys.argv)>1 and sys.argv[1]=='pi'
evidence=base/'pi-evidence' if pi else base
assert (evidence/'matrix.complete').exists()
reports=[]; checks=0
for rep in (1,2):
    for case in ('mixed','updates'):
        label=f'{case}-100000-r{rep}'; arms=[]
        for arm in ('baseline','scratch','cell','combined','sqlite'):
            engine='sqlite' if arm=='sqlite' else 'e4'
            p=evidence/f'matrix/{label}/{arm}/{engine}-100000-off.json'
            d=json.loads(p.read_text())
            for key,value in dict(rows=100000,engine=engine,cycles=4,case=case,reader='none',
                transaction_operations=1000,cache_bytes=8388608,timestamps=False,
                collections=2,vector_dim=4,change_vectors=False).items():assert d[key]==value,(p,key)
            assert len(d['phases'])==5
            for i,phase in enumerate(d['phases']):
                assert phase['cycle']==i
                assert phase['operations']==(100000 if i==0 else (40000 if case=='mixed' else 20000))
                assert phase['verification']['rows']==100000 and phase['seconds']>0
                assert phase['snapshot_verification'] is None
            assert d['reopen_verification']['rows']==100000
            assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
            peaks=[v['peak'] for v in d['phases']]+[d['alter_peak']]
            assert all(v['sample_errors']==0 and v['samples']>0 for v in peaks)
            usage=None
            if pi:
                usage=json.loads((evidence/f'matrix/{label}-{arm}.log').read_text().splitlines()[-1])['resource_usage']
                assert usage['exit_code']==0
            reports.append(dict(label=label,case=case,arm=arm,rows=100000,
                load_seconds=d['phases'][0]['seconds'],mutation_seconds=sum(v['seconds'] for v in d['phases'][1:]),
                peak_logical=max(v['sampled_peak_logical'] for v in peaks),
                peak_allocated=max(v['sampled_peak_allocated'] for v in peaks),
                final_logical=d['alter_final_logical'],final_allocated=d['alter_final_allocated'],
                mutation_issued_bytes=None if engine=='sqlite' else sum(v['issued_bytes']['total'] for v in d['phases'][1:]),
                usage=usage,evidence=str(p),sha256=hashlib.sha256(p.read_bytes()).hexdigest()))
            arms.append(d)
        for i in range(5):
            assert len({a['phases'][i]['verification']['crc32c'] for a in arms})==1;checks+=1
        assert len({a['reopen_verification']['crc32c'] for a in arms})==1;checks+=1
assert len(reports)==20
summary=[]
for case in ('mixed','updates'):
    item={'case':case}
    for arm in ('baseline','scratch','cell','combined','sqlite'):
        rs=[r for r in reports if r['case']==case and r['arm']==arm]
        item[arm]={k:statistics.mean(r[k] for r in rs) for k in ('load_seconds','mutation_seconds','peak_logical','peak_allocated','final_logical','final_allocated')}
    for arm in ('scratch','cell','combined'):
        item[arm]['mutation_reduction_pct']=100*(1-item[arm]['mutation_seconds']/item['baseline']['mutation_seconds'])
    summary.append(item)
out=dict(platform='Pi' if pi else 'Mac',arms=20,five_way_state_comparisons=checks,summary=summary,reports=reports)
(root/'docs'/('PAGE_EDIT_PI_RESULTS.json' if pi else 'PAGE_EDIT_RESULTS.json')).write_text(json.dumps(out,indent=2)+'\n')
print(json.dumps({k:v for k,v in out.items() if k!='reports'},indent=2))
