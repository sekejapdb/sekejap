"""Audit raw reports, including native SQLite, and derive explicit F1 verdicts."""
import hashlib,json,statistics,sys
from pathlib import Path
root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
pi=len(sys.argv)>1 and sys.argv[1]=='pi'
source=base/'pi-evidence' if pi else base
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
reports=[]
if pi:
    assert (source/'matrix.complete').exists()
    paths=sorted((source/'matrix').glob('*/*/report.json'))
else:
    assert (base/'smoke.complete').exists() and (base/'matrix.complete').exists()
    paths=sorted((base/'smoke').glob('*/*/report.json'))+sorted((base/'matrix').glob('*/*/report.json'))
groups={};states=0
for p in paths:
    d=json.loads(p.read_text());label=p.parent.parent.name;arm=p.parent.name
    case=label.rsplit('-r',1)[0] if '-r' in label else label
    assert arm==d['engine'] and d['reopen_verification']==d['phases'][-1]['verification']
    for phase in d['phases']:
        assert phase['verification']['rows']==d['rows'] and phase['peak']['errors']==0
    phases=d['phases'];last=phases[-1];peak=max(v['peak']['logical'] for v in phases)
    r=dict(case=case,label=label,arm=arm,rows=d['rows'],payload_bytes=d['payload_bytes'],cycles=d['cycles'],batch=d['batch'],
        load_seconds=phases[0]['seconds'],mutation_seconds=sum(v['seconds'] for v in phases[1:]),
        mutation_commit_seconds=sum(v['commit_seconds'] for v in phases[1:]),
        mutation_checkpoint_seconds=sum(v['ending_checkpoint_seconds'] for v in phases[1:]),
        create_update_delete_seconds=[sum(v['create_update_delete_seconds'][i] for v in phases[1:]) for i in range(3)],
        create_update_delete_counts=[sum(v['create_update_delete_counts'][i] for v in phases[1:]) for i in range(3)],
        loaded_logical=phases[0]['final_logical'],final_logical=last['final_logical'],final_allocated=last['final_allocated'],
        peak_logical=peak,peak_allocated=max(v['peak']['allocated'] for v in phases),expansion=peak/phases[0]['final_logical'],
        final_three_logical=[v['final_logical'] for v in phases[-3:]],reopen_seconds=d['reopen_seconds'],report=str(p),sha256=sha(p))
    if pi:
        usage=json.loads((source/'matrix'/(label+'-'+arm+'.log')).read_text().splitlines()[-1])['resource_usage']
        assert usage['exit_code']==0;r['resource_usage']=usage
    reports.append(r);groups.setdefault(label,{})[arm]=d
for label,ds in groups.items():
    assert set(ds)=={'e4','pagewal','sqlite'},label
    for i in range(len(ds['e4']['phases'])):
        assert len({d['phases'][i]['verification']['crc32c'] for d in ds.values()})==1;states+=1
    assert len({d['reopen_verification']['crc32c'] for d in ds.values()})==1;states+=1
assert len(reports)==(18 if pi else 39)
summary=[]
for case in sorted({r['case'] for r in reports}):
    row=dict(case=case)
    for arm in ('e4','pagewal','sqlite'):
        rs=[r for r in reports if r['case']==case and r['arm']==arm]
        row[arm]={k:statistics.median(r[k] for r in rs) for k in ('load_seconds','mutation_seconds','mutation_commit_seconds','mutation_checkpoint_seconds','final_logical','final_allocated','peak_logical','peak_allocated','expansion','reopen_seconds')}
        row[arm]['repetitions']=len(rs)
    for op in ('load_seconds','mutation_seconds'):
        if row['e4'][op]:row[op+'_reduction_vs_e4_pct']=100*(1-row['pagewal'][op]/row['e4'][op])
        if row['sqlite'][op]:row[op+'_ratio_to_sqlite']=row['pagewal'][op]/row['sqlite'][op]
    row['final_size_ratio_to_sqlite']=row['pagewal']['final_logical']/row['sqlite']['final_logical']
    row['size_target']='PASS' if row['final_size_ratio_to_sqlite']<=1.10 else 'FAIL'
    op='load_seconds' if case.startswith('load-') else 'mutation_seconds'
    row['time_target']='PASS' if row[op+'_ratio_to_sqlite']<=1.10 else 'FAIL'
    row['acceptance_repetitions']='PASS' if row['pagewal']['repetitions']>=3 else 'PENDING'
    summary.append(row)
out=dict(platform='Pi' if pi else 'Mac',layer='raw KV only',arms=len(reports),three_way_state_comparisons=states,
         aggregate='median; single-repetition cases are probes',summary=summary,reports=reports)
p=root/'docs'/('PAGEWAL_PI_RESULTS.json' if pi else 'PAGEWAL_RESULTS.json');p.write_text(json.dumps(out,indent=2)+'\n')
print(json.dumps({k:v for k,v in out.items() if k not in ('reports','summary')}))
for r in summary:print(r['case'],{a:round(r[a]['mutation_seconds'] if not r['case'].startswith('load-') else r[a]['load_seconds'],3) for a in ('e4','pagewal','sqlite')},r['time_target'],r['size_target'])
