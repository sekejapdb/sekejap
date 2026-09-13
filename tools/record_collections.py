#!/usr/bin/env python3
"""Independently compare every benchmark state, then record paired tables."""
import json
import os
from pathlib import Path
from statistics import mean
BASE = Path(os.environ.get('E4_COLLECTION_ARTIFACTS', '<scratch>'))
ROOT = Path(__file__).resolve().parents[1]
reports = []
checks = 0
row_visits = 0
for repeat in (1, 2):
    for n in (100000, 400000):
        for times in ('off', 'on'):
            pair = []
            for engine in ('e4', 'sqlite'):
                p = BASE / f'run-{repeat}' / f'{engine}-{n}-{times}.json'
                d = json.loads(p.read_text())
                assert d['engine'] == engine and d['rows'] == n
                assert d['timestamps'] == (times == 'on')
                assert len(d['phases']) == 4
                assert all(x['peak']['sample_errors'] == 0 for x in d['phases'])
                assert d['alter_peak']['sample_errors'] == 0
                assert all(x['verification']['rows'] == n for x in d['phases'])
                assert d['reopen_verification']['rows'] == n
                for i, x in enumerate(d['phases']):
                    assert x['cycle'] == i
                    assert x['operations'] == (n if i == 0 else n * 4 // 10)
                    assert x['peak']['sampled_peak_logical'] >= x['final_logical']
                d['repeat'] = repeat
                d['evidence'] = str(p)
                reports.append(d)
                pair.append(d)
                row_visits += n * 6  # four phase scans, pre-alter and post-reopen
            for i in range(4):
                assert pair[0]['phases'][i]['verification']['crc32c'] == pair[1]['phases'][i]['verification']['crc32c']
                checks += 1
            assert pair[0]['reopen_verification']['crc32c'] == pair[1]['reopen_verification']['crc32c']
            checks += 1
summary = {'arms': len(reports), 'paired_state_checks': checks,
           'rows_visited_by_exact_oracles': row_visits,
           'source': str(BASE), 'results': reports}
(ROOT / 'docs/COLLECTION_RESULTS.json').write_text(json.dumps(summary, indent=2) + '\n')
lines = ['# Collection benchmark tables — 2026-09-11', '',
         'MiB = 1,048,576 bytes. Times are seconds. All repeated arms are shown.', '',
         'Final sizes include every file in the engine directory. Peaks are 1 ms',
         'sampled lower bounds, including WAL and bookkeeping; they are not caps.', '',
         '| Repeat | Rows | Times | Engine | Load s | Load MiB | 3-cycle churn s | Churn peak MiB | Churn final MiB | Peak allocated MiB |',
         '|---|---:|---|---|---:|---:|---:|---:|---:|---:|']
for d in reports:
    p = d['phases']
    lines.append(f"| {d['repeat']} | {d['rows']:,} | {'ON' if d['timestamps'] else 'OFF'} | {d['engine']} | {p[0]['seconds']:.3f} | {p[0]['final_logical']/2**20:.3f} | {sum(x['seconds'] for x in p[1:]):.3f} | {max(x['peak']['sampled_peak_logical'] for x in p[1:])/2**20:.3f} | {p[-1]['final_logical']/2**20:.3f} | {max(x['peak']['sampled_peak_allocated'] for x in p)/2**20:.3f} |")
lines += ['', '## Phase detail', '', '| Repeat | Rows | Times | Engine | Cycle | Operations | Time s | Logical peak MiB | Logical final MiB | Allocated final MiB |', '|---|---:|---|---|---:|---:|---:|---:|---:|---:|']
for d in reports:
    for p in d['phases']:
        lines.append(f"| {d['repeat']} | {d['rows']:,} | {'ON' if d['timestamps'] else 'OFF'} | {d['engine']} | {p['cycle']} | {p['operations']:,} | {p['seconds']:.3f} | {p['peak']['sampled_peak_logical']/2**20:.3f} | {p['final_logical']/2**20:.3f} | {p['final_allocated']/2**20:.3f} |")
lines += ['', '## Schema alteration and reopen', '', 'Alter adds an optional declared field to both collections; old rows are unchanged.', 'Reopen timing excludes the subsequent full row verification. This is a warm filesystem-cache reopen.', '', '| Repeat | Rows | Times | Engine | Alter ms | Logical change B | Reopen ms |', '|---|---:|---|---|---:|---:|---:|']
for d in reports:
    lines.append(f"| {d['repeat']} | {d['rows']:,} | {'ON' if d['timestamps'] else 'OFF'} | {d['engine']} | {d['alter_seconds']*1000:.3f} | {d['alter_final_logical']-d['phases'][-1]['final_logical']} | {d['reopen_seconds']*1000:.3f} |")
(ROOT / 'docs/COLLECTION_TABLES.md').write_text('\n'.join(lines) + '\n')
print(json.dumps({k:v for k,v in summary.items() if k != 'results'}))
for n in (100000,400000):
    for times in (False,True):
        for engine in ('e4','sqlite'):
            ds=[d for d in reports if (d['rows'],d['timestamps'],d['engine'])==(n,times,engine)]
            print(n,times,engine,'load_s',round(mean(d['phases'][0]['seconds'] for d in ds),3), 'load_MiB',round(mean(d['phases'][0]['final_logical'] for d in ds)/2**20,3), 'churn_s',round(mean(sum(p['seconds'] for p in d['phases'][1:]) for d in ds),3),'peak_MiB',round(max(p['peak']['sampled_peak_logical'] for d in ds for p in d['phases'][1:])/2**20,3),'final_MiB',round(mean(d['phases'][-1]['final_logical'] for d in ds)/2**20,3))
# The longer diagnostic is kept separate from the two repeated three-cycle arms.
long = []
for engine in ('e4','sqlite'):
    d = json.loads((BASE / 'sustained' / f'{engine}-400000-off.json').read_text())
    assert len(d['phases']) == 13 and d['rows'] == 400000 and not d['timestamps']
    assert all(p['verification']['rows'] == 400000 and p['peak']['sample_errors'] == 0 for p in d['phases'])
    assert d['reopen_verification']['rows'] == 400000
    long.append(d)
for i in range(13):
    assert long[0]['phases'][i]['verification']['crc32c'] == long[1]['phases'][i]['verification']['crc32c']
assert long[0]['reopen_verification']['crc32c'] == long[1]['reopen_verification']['crc32c']
long_summary = {'arms':2,'paired_state_checks':14,'rows_visited_by_exact_oracles':12000000,'results':long}
(ROOT / 'docs/COLLECTION_SUSTAINED_RESULTS.json').write_text(json.dumps(long_summary,indent=2)+'\n')
lines=['# Twelve-cycle collection diagnostic — 400K, timestamps OFF', '',
       'Single additional matched pair, E4 first. This is separate from the repeated three-cycle matrix.', '',
       '| Cycle | E4 time s | SQLite time s | E4 logical peak MiB | SQLite logical peak MiB | E4 final MiB | SQLite final MiB |',
       '|---:|---:|---:|---:|---:|---:|---:|']
for a,b in zip(long[0]['phases'],long[1]['phases']):
    lines.append(f"| {a['cycle']} | {a['seconds']:.3f} | {b['seconds']:.3f} | {a['peak']['sampled_peak_logical']/2**20:.3f} | {b['peak']['sampled_peak_logical']/2**20:.3f} | {a['final_logical']/2**20:.3f} | {b['final_logical']/2**20:.3f} |")
(ROOT / 'docs/COLLECTION_SUSTAINED_TABLES.md').write_text('\n'.join(lines)+'\n')
for d in long:
    p=d['phases'];peak=max(x['peak']['sampled_peak_logical'] for x in p);ap=max(x['peak']['sampled_peak_allocated'] for x in p)
    print('sustained',d['engine'],'churn_s',sum(x['seconds'] for x in p[1:]),'peak_MiB',peak/2**20,'final_MiB',p[-1]['final_logical']/2**20,'peak_factor',peak/p[0]['final_logical'],'allocated_peak_MiB',ap/2**20,'allocated_factor',ap/p[0]['final_allocated'])
