#!/usr/bin/env python3
"""Validate both controls and publish every completed Pi comparison."""
import json, pathlib, sys

root=pathlib.Path(sys.argv[1])
out=pathlib.Path(sys.argv[2])
records=[]
limit_kind=None
postboot_path=root.parent/'postboot-verification.json'
before_reboot=set()
if postboot_path.exists():
    postboot=json.loads(postboot_path.read_text())
    assert postboot['complete']
    before_reboot={p['run'] for p in postboot['pairs']}
states=snapshots=rows=0
for path in sorted(root.glob('*/results.json')):
    r=json.loads(path.read_text())
    assert r['complete'], path
    memory=r['process_memory_limits']
    kind='cgroup' if memory['memory_max']=='67108864' else 'address-space'
    if kind=='cgroup':
        assert memory['swap_max']=='0',path
    else:
        assert memory['address_space_soft_hard']==['134217728','134217728'],path
    assert limit_kind in (None,kind),'do not combine different memory limits'
    limit_kind=kind
    a,b=r['arms']
    assert len(a['stages'])==len(b['stages'])
    for x,y in zip(a['stages'],b['stages']):
        assert x['verify']['crc32c']==y['verify']['crc32c'],path
        assert x['verify']['rows']==y['verify']['rows'],path
        for stage in (x,y):
            assert stage['verify']['exact'] and stage['peak']['sample_errors']==0,path
            states+=1; rows+=stage['verify']['rows']
            if stage.get('snapshot_verify'):
                assert stage['snapshot_verify']['exact'],path
                snapshots+=1
            if stage.get('accounting'):
                assert stage['accounting']['unaccounted_pages']==0,path
    assert a['reopened_verify']['crc32c']==b['reopened_verify']['crc32c'],path
    assert a['reads']['crc32c']==b['reads']['crc32c'],path
    mode=path.parent.name.split('-')[0]
    limits=r.get('e4_resource_limits')
    assert bool(limits)==(mode=='limited'),path
    for arm in r['arms']:
        assert arm['reopened_verify']['exact'] and arm['reopened_verify']['rows']==arm['n'],path
        assert arm['reopened_verify']['crc32c']==arm['stages'][-1]['verify']['crc32c'],path
        peaks=[s['peak'] for s in arm['stages']]+[arm['closing_peak']]
        assert all(p['sample_errors']==0 for p in peaks),path
        logical=max(p['sampled_peak_bytes'] for p in peaks)
        if limits and arm['arm']=='e4':
            assert logical <= limits['managed_logical_allowance']-limits['recovery_bytes'],path
            for s in arm['stages']:
                assert all(n<=limits['tracked_pages'] for n in s['counters']['tracked_pages']),path
        records.append({
            'run':path.parent.name,'rows':arm['n'],'case':arm['case'],
            'timing_caveat':'shared device load during final pair' if path.parent.name=='limited-load-4000000' and before_reboot else None,
            'measurement_batch':('before_sensor_reboot' if path.parent.name in before_reboot else 'after_sensor_reboot') if before_reboot else 'single_batch',
            'engine':arm['arm'],'mode':mode,'load_order':r['load_order'],'limits':limits if arm['arm']=='e4' else None,
            'load_seconds':arm['stages'][0]['seconds'],
            'mutation_seconds':sum(s['seconds'] for s in arm['stages'][1:]),
            'commit_seconds':sum(s['profile']['commit_seconds'] for s in arm['stages'][1:]),
            'operations':sum(s.get('operations',0) for s in arm['stages'][1:]),
            'initial_bytes':arm['initial_bytes'],'peak_bytes':logical,
            'peak_initial_factor':logical/arm['initial_bytes'],
            'allocated_peak_bytes':max(p['sampled_peak_allocated_bytes'] for p in peaks),
            'sampled_rss_bytes':max(p['sampled_peak_rss_bytes'] for p in peaks),
            'final_bytes':arm['final_disk']['total_bytes'],
            'reopen_seconds':arm['reopen_seconds'],
        })
assert len(records)==36, f'expected 18 complete pairs, found {len(records)//2}'
result={'complete':True,'pairs':len(records)//2,'limit_kind':limit_kind,
    'reboot_between_batches':bool(before_reboot),'pairs_reverified_after_reboot':len(before_reboot),
    'process_cap_bytes':(64 if limit_kind=='cgroup' else 128)<<20,
    'swap_cap_bytes':0 if limit_kind=='cgroup' else None,'current_state_checks':states,'repeated_row_visits':rows,
    'snapshot_checks':snapshots,'records':records}
out.mkdir(exist_ok=True)
(out/'RESOURCE_RESULTS.json').write_text(json.dumps(result,indent=2)+'\n')
lines=['# Raspberry Pi resource comparison — 2026-09-11','',
    'One pair per mode, case and size; 4 mutation cycles. All processes ran under a',
    ('64 MiB cgroup with swap disabled.' if limit_kind=='cgroup' else
     '128 MiB address-space limit (RLIMIT_AS). Filesystem cache is outside this limit; swap is not disabled.'),
    'The 4M rung is load-only with ascending IDs;',
    '100K/400K use a fixed shuffled order. Both engines receive the same order.',
    'The 4M rung tests resource scale; it is not an order-matched timing exponent. Durability is FULL',
    'using Linux fsync, with 1000-operation commits, 8 MiB configured writer budgets,',
    '64 KiB snapshot caches, no mmap, and native SQLite auto-checkpoint=1000 pages.',
    'Both engines also checkpoint at phase ends. E4 constrained commits publish',
    'metadata every transaction; ordinary E4 uses the 4 MiB WAL-or-page trigger.','',
    '`mixed_long` pins the initial snapshot through cycles 1–2, releases it after',
    'cycle 2, then runs cycles 3–4 without that reader. `mixed_none` has no pinned',
    'snapshot. Each mixed cycle updates/deletes 30% of IDs, then reinserts 10%.',
    'The fixed `updates` case changes 30% without deletes or payload growth.','',
    'The two SQLite rows are the separate paired controls, retained individually.',
    'Ordinary pairs run E4 first; constrained pairs run SQLite first. These are',
    'single device measurements, not a repetition-based parity claim. Peaks are',
    'sampled. Allocated file bytes exclude directory/filesystem metadata; RSS',
    'excludes filesystem cache. Limits are not free-space',
    'reservations. RSS is the sampled process RSS during each arm; the two arms',
    'share a process, so allocator retention from the first can affect the second.',
    'MiB = 1,048,576 bytes. Initial size is the post-load footprint; peak/initial',
    'is observed growth, not a promised safety factor. The matrix uses generous',
    'explicit E4 limits, not an automatic 2×-initial policy.','',
    ('The user rebooted the Pi to attach sensors after the eight 100K pairs and two 400K load pairs. '
     'All 20 completed databases were reverified after reboot. The interrupted 400K update pair was '
     'preserved and rerun in full; no accepted pair combines timings from opposite sides of the reboot.'
     if before_reboot else 'The accepted matrix was collected in one device batch.'),'',
    ('Timing caveat (†): the final constrained 4M pair ran from 14:09:29 to 14:15:34. '
     'Other Pi Python jobs started at 14:10:08 and 14:12:45; the final hardware record shows '
     'load averages 5.96/3.98/2.57, with no throttling. LLM jobs were also observed later. '
     'The affected pair proves correctness/resource behavior, but its timing cannot isolate '
     'the cost of constrained commits. No other user jobs were stopped.' if before_reboot else ''),'',
    '| Rows | Case | Engine / paired mode | Load s | Mutation s | Initial MiB | Peak MiB | Peak / initial | Allocated peak MiB | Final MiB | RSS MiB |',
    '|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|']
for r in sorted(records,key=lambda r:(r['rows'],r['case'],r['mode'],r['engine'])):
    label=('E4 constrained' if r['mode']=='limited' else 'E4 ordinary') if r['engine']=='e4' else f"SQLite ({r['mode']} pair)"
    if r['timing_caveat']: label+=' †'
    values=[r[k]/(1<<20) for k in ['peak_bytes','allocated_peak_bytes','final_bytes','sampled_rss_bytes']]
    lines.append(f"| {r['rows']:,} | {r['case']} | {label} | {r['load_seconds']:.2f} | {r['mutation_seconds']:.2f} | {r['initial_bytes']/(1<<20):.2f} | {values[0]:.2f} | {r['peak_initial_factor']:.2f}× | "+' | '.join(f'{v:.2f}' for v in values[1:])+' |')
lines+=['',f'Validation: {states} current-state checks, {rows:,} repeated row visits,',
    f'{snapshots} snapshot checks, exact point-read and reopen agreement in every pair.',
    'Timestamps, vectors, graph edges and secondary indexes are off in both engines.',
    'The corpus contains Unicode names, scalar values, a geo point, nested binary',
    'JSON and undeclared extras. Mixed cycles alternate large and small values.','']
(out/'RESOURCE_TABLES.md').write_text('\n'.join(lines))
print(json.dumps({'pairs':len(records)//2,'states':states,'snapshot_checks':snapshots}))
