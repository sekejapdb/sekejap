"""Audit completed reports; retain failures and exclude contended timings."""
import hashlib,json,statistics,re
from pathlib import Path
repo=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
result={'layer':'raw KV, not full hybrid collections','platforms':{},'raw_reports':[],
        'promotion':'Not promoted; remaining F1 law gates and failed accepted control remain explicit',
        'mac_contended_run':'matrix-contended excluded from timing acceptance'}
for platform,b in [('Mac',base),('Pi',base/'pi-evidence')]:
    summary={'timing':{},'cap':[]}
    for label in ['mixed-100k','sustain-400k']:
        arms={}
        for arm in ['e4','pagewal-v1','pagewal-v2','sqlite']:
            runs=[]
            for rep in range(1,4):
                p=b/'matrix'/(label+'-r'+str(rep))/arm/'report.json'
                if not p.exists():
                    assert platform=='Mac' and label=='sustain-400k' and arm=='e4' and rep==1
                    runs.append({'rep':rep,'correctness':'FAIL','evidence':str(p.parent.parent/'e4.log')});continue
                d=json.loads(p.read_text());n=100000 if label=='mixed-100k' else 400000;cycles=4 if n==100000 else 12
                assert (d['rows'],d['cycles'],d['batch'],d['cache_bytes'])==(n,cycles,1000,8388608)
                assert len(d['phases'])==cycles+1 and d['reopen_verification']==d['phases'][-1]['verification']
                for i,x in enumerate(d['phases']):
                    assert x['cycle']==i and x['verification']['rows']==n and x['peak']['errors']==0
                    assert x['create_update_delete_counts']==([n,0,0] if i==0 else [n//10,n//5,n//10])
                phases=d['phases'][1:];last=d['phases'][-1]
                runs.append(dict(rep=rep,correctness='PASS',load_seconds=d['phases'][0]['seconds'],
                    mutation_seconds=sum(x['seconds'] for x in phases),
                    commit_seconds=sum(x['commit_seconds'] for x in phases),
                    ending_checkpoint_seconds=sum(x['ending_checkpoint_seconds'] for x in phases),
                    create_update_delete_seconds=[sum(x['create_update_delete_seconds'][i] for x in phases) for i in range(3)],
                    final_logical=last['final_logical'],final_allocated=last['final_allocated'],
                    observed_peak_logical=max(max(x['peak']['logical'],x['final_logical']) for x in d['phases']),
                    observed_peak_allocated=max(max(x['peak']['allocated'],x['final_allocated']) for x in d['phases']),
                    last_three_sizes=[x['final_logical'] for x in d['phases'][-3:]],report=str(p),sha256=sha(p)))
                result['raw_reports'].append(dict(path=str(p),sha256=sha(p)))
            good=[x for x in runs if x['correctness']=='PASS']
            arms[arm]={'runs':runs,'correctness':'PASS' if len(good)==3 else 'FAIL',
                'complete_repetitions':len(good),'failed_repetitions':3-len(good)}
            for field in ['load_seconds','mutation_seconds','final_logical','final_allocated','observed_peak_logical','observed_peak_allocated','commit_seconds','ending_checkpoint_seconds']:
                arms[arm]['median_'+field]=statistics.median(x[field] for x in good)
            arms[arm]['mutation_range_seconds']=[min(x['mutation_seconds'] for x in good),max(x['mutation_seconds'] for x in good)]
        for rep in range(1,4):
            docs=[]
            for arm in arms:
                p=b/'matrix'/(label+'-r'+str(rep))/arm/'report.json'
                if p.exists():docs.append(json.loads(p.read_text()))
            for i in range(len(docs[0]['phases'])):assert len({d['phases'][i]['verification']['crc32c'] for d in docs})==1
        candidate=arms['pagewal-v2'];sql=arms['sqlite'];v1=arms['pagewal-v1']
        summary['timing'][label]={'arms':arms,'v2_over_sqlite_time':candidate['median_mutation_seconds']/sql['median_mutation_seconds'],
            'v2_over_v1_time':candidate['median_mutation_seconds']/v1['median_mutation_seconds'],
            'v2_over_sqlite_size':candidate['median_final_logical']/sql['median_final_logical']}
    for folder in ['cap','cap-short']:
        reports=sorted((b/folder).rglob('report.json'));assert len(reports)==(18 if folder=='cap' else 6)
        for p in reports:
            d=json.loads(p.read_text());assert d['verified'] and d['cap_logical']==2*d['loaded_logical']
            if d['engine']=='pagewal':assert d['observed_peak_logical']<=d['cap_logical']
            summary['cap'].append(d);result['raw_reports'].append(dict(path=str(p),sha256=sha(p)))
    repair=b/'repair-100k'/'COMPLETE.json';assert repair.exists();d=json.loads(repair.read_text())
    assert d['source_unchanged'] and d['current_rows']==d['verified_current_rows']==100000 and d['candidate_rows']==0
    summary['repair']=d
    result['platforms'][platform]=summary
for p in (base/'matrix-contended').rglob('report.json'):result['raw_reports'].append(dict(path=str(p),sha256=sha(p),timing_excluded=True))
accepted=json.loads((repo/'docs/REUSE_PROVENANCE.json').read_text())['source_members'];checked=0
for name,h in accepted.items():
    if name.startswith(('src/','kernel/','tests/')) or name=='CONTRACT.md':assert sha(repo/name)==h,name;checked+=1
result['accepted_production_files_unchanged']=checked
frozen=json.loads((base/'provenance.json').read_text());source=Path('/tmp/e4-pagewal-qualify')
for name,h in frozen['source_members'].items():
    if name!='src/bin/pagewal_cap.rs':assert sha(source/name)==h,name
for name,h in frozen['artifacts'].items():assert sha(base/name)==h,name
result['frozen_provenance']=frozen
result['short_reader_harness']={'source':str(source/'src/bin/pagewal_cap.rs'),'sha256':sha(source/'src/bin/pagewal_cap.rs'),
    'mac_binary_sha256':sha(base/'pagewal_cap_short'),'engine_changes':False}
content=(base/'workspace-tests.log').read_text();seen=set();binary=''
for line in content.splitlines():
    if 'Running ' in line and '/deps/' in line:binary=line.split('/deps/')[1].split(')')[0]
    m=re.search(r'test ([^ ]+) \.\.\.',line)
    if m:seen.add((binary,m.group(1)))
assert 'test result: FAILED' not in content and 'Doc-tests kernel' in content
result['tests']={'mac_distinct_entries':len(seen),'pi_selected_entries':27,'includes_subprocess_helpers':True,
    'io_fault_cases':356,'ordinary_and_simulated_power_loss_reopens':712,'repair_fixture_cases':8,
    'mac_log_sha256':sha(base/'workspace-tests.log'),'pi_log_sha256':sha(base/'pi-evidence/tests.log')}
out=repo/'docs/PAGEWAL_QUALIFICATION_RESULTS.json';out.write_text(json.dumps(result,indent=2)+'\n')
(base/out.name).write_text(out.read_text())
lines=['\n## Final measured results\n','Times below are seconds. Medians use three repetitions unless the control failed.\n',
    '| Platform / case | Current E4 | Page-WAL v1 | Page-WAL v2 | SQLite |', '|---|---:|---:|---:|---:|']
for platform,d in result['platforms'].items():
    for label,x in d['timing'].items():
        cells=[]
        for arm in ['e4','pagewal-v1','pagewal-v2','sqlite']:
            a=x['arms'][arm];cells.append(('**FAIL 1/3**; completed range '+ '–'.join('%.3f'%v for v in a['mutation_range_seconds'])) if a['correctness']=='FAIL' else '%.3f'%a['median_mutation_seconds'])
        lines.append('| '+platform+' / '+label+' | '+' | '.join(cells)+' |')
lines+=['\nInitial load is separate:\n','| Platform / case | Current E4 | Page-WAL v1 | Page-WAL v2 | SQLite |','|---|---:|---:|---:|---:|']
for platform,d in result['platforms'].items():
    for label,x in d['timing'].items():lines.append('| '+platform+' / '+label+' | '+' | '.join('%.3f'%x['arms'][a]['median_load_seconds'] for a in ['e4','pagewal-v1','pagewal-v2','sqlite'])+' |')
lines+=['\nFinal/observed-peak logical sizes below are decimal MB, medians across completed runs. A sampled peak is a lower bound.\n',
    '| Platform / 400K arm | Final MB | Observed peak MB |','|---|---:|---:|']
for platform,d in result['platforms'].items():
    for arm,a in d['timing']['sustain-400k']['arms'].items():lines.append('| '+platform+' / '+arm+' | %.3f | %.3f |'%(a['median_final_logical']/1e6,a['median_observed_peak_logical']/1e6))
lines+=['\nCap diagnostics: completed updates / requested updates; observed peak divided by loaded logical size.\n',
    '| Platform | Rows / reader | E4 completed updates; peak | SQLite completed updates; peak |','|---|---|---:|---:|']
for platform,d in result['platforms'].items():
    for n in [10000,40000,100000]:
        for reader in ['none','held','rolling','short']:
            cells=[]
            for arm in ['pagewal','sqlite']:
                a=next(x for x in d['cap'] if (x['rows'],x['reader'],x['engine'])==(n,reader,arm))
                cells.append('%s / %s; %.3f×'%(format(a['committed_updates'],','),format(a['requested_updates'],','),a['observed_peak_logical']/a['loaded_logical']))
            lines.append('| '+platform+' | '+str(n)+' / '+reader+' | '+' | '.join(cells)+' |')
report=repo/'docs/PAGEWAL_QUALIFICATION.md';text=report.read_text().split('\n## Final measured results\n')[0]
report.write_text(text+'\n'.join(lines)+'\n')
print(json.dumps({'reports':len(result['raw_reports']),'accepted_source_files':checked,'tests':result['tests'],'summary':{p:{k:{f:v for f,v in x.items() if f!='arms'} for k,x in d['timing'].items()} for p,d in result['platforms'].items()}},indent=2))
