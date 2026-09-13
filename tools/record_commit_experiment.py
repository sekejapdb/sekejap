"""Audit all three-arm commit experiments before deciding whether to retain."""
import hashlib
import json
from pathlib import Path
import statistics

base = Path('<scratch>')
root = Path(__file__).resolve().parents[1]
assert (base/'matrix.complete').exists()
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(base/'baseline') != sha(base/'candidate')
reports, comparisons = [], 0
for rep in (1, 2, 3):
    for batch, n, cycles in ((1,1000,2),(100,10000,3),(1000,100000,4)):
        arms = []
        for arm in ('baseline','candidate','sqlite'):
            engine = 'sqlite' if arm == 'sqlite' else 'e4'
            path = base/f'matrix/batch-{batch}-r{rep}/{arm}/{engine}-{n}-off.json'
            d = json.loads(path.read_text())
            for key, expected in dict(engine=engine, rows=n, cycles=cycles,
                transaction_operations=batch, cache_bytes=8388608, collections=2,
                case='mixed', reader='none', timestamps=False, vector_dim=4,
                change_vectors=False).items():
                assert d[key] == expected, (path,key)
            assert len(d['phases']) == cycles+1
            for i,p in enumerate(d['phases']):
                assert p['cycle']==i and p['operations']==(n if i==0 else n*4//10)
                assert p['verification']['rows']==n and p['seconds']>0
                assert p['peak']['sample_errors']==0 and p['peak']['samples']>0
                assert p['snapshot_verification'] is None
                if engine=='e4':
                    b=p['issued_bytes']
                    assert b['total']==b['data']+b['wal']+b['sidecars']
            assert d['reopen_verification']['rows']==n
            assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
            assert d['alter_peak']['sample_errors']==0
            peaks=[p['peak'] for p in d['phases']]+[d['alter_peak']]
            row=dict(arm=arm, batch=batch, rep=rep, rows=n, cycles=cycles,
                load_seconds=d['phases'][0]['seconds'],
                churn_seconds=sum(p['seconds'] for p in d['phases'][1:]),
                peak_logical=max(p['sampled_peak_logical'] for p in peaks),
                peak_allocated=max(p['sampled_peak_allocated'] for p in peaks),
                final_logical=d['alter_final_logical'], evidence=str(path),sha256=sha(path))
            reports.append(row)
            arms.append(d)
        for i in range(cycles+1):
            assert len({a['phases'][i]['verification']['crc32c'] for a in arms})==1
            comparisons+=1
        assert len({a['reopen_verification']['crc32c'] for a in arms})==1
        comparisons+=1
assert len(list((base/'matrix').glob('*/*/*.json')))==27
summary=[]
for batch in (1,100,1000):
    result={'batch':batch}
    for arm in ('baseline','candidate','sqlite'):
        rows=[r for r in reports if r['arm']==arm and r['batch']==batch]
        result[arm]={key:statistics.mean(r[key] for r in rows) for key in
            ('load_seconds','churn_seconds','peak_logical','peak_allocated','final_logical')}
    result['load_improvement_pct']=100*(1-result['candidate']['load_seconds']/result['baseline']['load_seconds'])
    result['churn_improvement_pct']=100*(1-result['candidate']['churn_seconds']/result['baseline']['churn_seconds'])
    result['paired_churn_improvement_pct']=[100*(1-next(r['churn_seconds'] for r in reports if r['arm']=='candidate' and r['batch']==batch and r['rep']==rep)/next(r['churn_seconds'] for r in reports if r['arm']=='baseline' and r['batch']==batch and r['rep']==rep)) for rep in (1,2,3)]
    summary.append(result)
out=dict(arms=len(reports),three_way_state_comparisons=comparisons,
    binaries={a:sha(base/a) for a in ('baseline','candidate')},summary=summary,reports=reports)
(root/'docs/COMMIT_EXPERIMENT_RESULTS.json').write_text(json.dumps(out,indent=2)+'\n')
print(json.dumps({k:v for k,v in out.items() if k!='reports'},indent=2))
