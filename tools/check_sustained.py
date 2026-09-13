#!/usr/bin/env python3
"""Validate retained paired outputs; summarize all repetitions without selecting wins."""
import json, pathlib, statistics, sys
root=pathlib.Path(sys.argv[1]); groups={}; states=rows=snapshots=0
for p in sorted(root.glob('*/results.json')):
    d=json.loads(p.read_text())
    if 'sqlite_native_autocheckpoint_pages' not in d: continue
    if not d.get('complete'): raise ValueError(f'incomplete {p}')
    arms={a['arm']:a for a in d['arms']}
    assert set(arms)=={'e4','sqlite'},p
    a,b=arms['e4'],arms['sqlite']
    assert len(a['stages'])==len(b['stages'])
    for x,y in zip(a['stages'],b['stages']):
        assert (x['cycle'],x['stage'])==(y['cycle'],y['stage'])
        assert x['verify']['crc32c']==y['verify']['crc32c'],p
        assert x['verify']['rows']==y['verify']['rows'],p
        assert x.get('operations')==y.get('operations'),p
        assert x.get('pinned')==y.get('pinned'),p
        assert bool(x.get('snapshot_verify'))==bool(y.get('snapshot_verify')),p
        if x.get('snapshot_verify'):
            assert x['snapshot_verify']['crc32c']==y['snapshot_verify']['crc32c'],p
            assert x['snapshot_verify']['rows']==y['snapshot_verify']['rows'],p
        for st in (x,y):
            assert st['verify']['exact'] and st['peak']['sample_errors']==0,p
            states+=1;rows+=st['verify']['rows']
            if st.get('accounting'): assert st['accounting']['unaccounted_pages']==0,p
            if st.get('snapshot_verify'):
                assert st['snapshot_verify']['exact'];snapshots+=1
    assert a['reopened_verify']['crc32c']==b['reopened_verify']['crc32c'],p
    for arm in arms.values():
        assert arm['reopened_verify']['exact'],p
        assert arm['reopened_verify']['rows']==arm['n'],p
        assert arm['reopened_verify']['crc32c']==arm['stages'][-1]['verify']['crc32c'],p
        assert arm['closing_peak']['sample_errors']==0,p
    if 'reads' in a:
        assert a['reads']['crc32c']==b['reads']['crc32c']
    name=p.parent.name
    mode=name.split('-')[0]
    group=groups.setdefault(f"{mode}/{a['case']}/{a['n']}",[])
    record={'run':name}
    for engine,arm in arms.items():
        stages=arm['stages']; churn=stages[1:]
        peaks=[st['peak'] for st in stages]+[arm['closing_peak']]
        end_cycles={st['cycle']:st['disk']['total_bytes'] for st in stages}
        last_cycles=sorted(end_cycles)[-4:]
        even_cycles=sorted(c for c in end_cycles if c%2==0)[-2:]
        record[engine]={
            'load_seconds':stages[0]['seconds'],
            'initial_bytes':arm['initial_bytes'],
            'sampled_growth_factor':max(p['sampled_peak_bytes'] for p in peaks)/arm['initial_bytes'],
            'mutation_seconds':sum(st['seconds'] for st in churn),
            'operations':sum(st.get('operations',0) for st in churn),
            'peak_bytes':max(p['sampled_peak_bytes'] for p in peaks),
            'allocated_peak_bytes':max(p['sampled_peak_allocated_bytes'] for p in peaks),
            'final_bytes':arm['final_disk']['total_bytes'],
            'reopen_seconds':arm['reopen_seconds'],
            'reads':arm.get('reads'),
            'scan':arm.get('scan'),
            'last_four_cycle_end_bytes':{str(c):end_cycles[c] for c in last_cycles},
            'last_four_cycle_growth_bytes':end_cycles[last_cycles[-1]]-end_cycles[last_cycles[0]],
            'last_two_even_cycle_growth_bytes':end_cycles[even_cycles[-1]]-end_cycles[even_cycles[0]] if len(even_cycles)>1 else None,
            'profile': [st.get('profile') for st in stages],
            'last_cycle_bytes':[st['disk']['total_bytes'] for st in stages if st['cycle']==d['cycles']],
        }
    group.append(record)
out={'complete':True,'current_state_checks':states,'current_rows_compared':rows,'snapshot_checks':snapshots,'groups':{}}
for group,repeats in groups.items():
    g={'repetitions':len(repeats),'runs':repeats,'summary':{}}
    for engine in ('e4','sqlite'):
        summary={}
        for metric in ('load_seconds','mutation_seconds','peak_bytes','allocated_peak_bytes','final_bytes','reopen_seconds'):
            values=[r[engine][metric] for r in repeats]
            summary[metric]={'median':statistics.median(values),'min':min(values),'max':max(values)}
        g['summary'][engine]=summary
    g['e4_sqlite_ratios']={m:g['summary']['e4'][m]['median']/g['summary']['sqlite'][m]['median'] if g['summary']['sqlite'][m]['median'] else None for m in ('load_seconds','mutation_seconds','peak_bytes','final_bytes')}
    out['groups'][group]=g
print(json.dumps(out,indent=2))
