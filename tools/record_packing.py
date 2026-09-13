"""Audit complete paired reports and render reproducible packing tables."""
import json
import sys
from pathlib import Path

BASE = Path(sys.argv[1] if len(sys.argv) > 1 else '<scratch>')
PI = len(sys.argv) > 2 and sys.argv[2] == 'pi'
OUT = Path(__file__).resolve().parents[1] / 'docs'
PREFIX = 'PACKING_PI' if PI else 'PACKING'
reports = []
checks = visits = 0

def read(label, engine, n, cycles, case='mixed', reader='none', times=False, ablation=False):
    global visits
    p = BASE / (label if ablation else f'matrix/{label}') / f'{engine}-{n}-{"on" if times else "off"}.json'
    d = json.loads(p.read_text())
    for key, value in dict(engine=engine, rows=n, cycles=cycles, case=case, reader=reader, timestamps=times,
                           cache_bytes=8388608, transaction_operations=1000, collections=2).items():
        assert d[key] == value, (p, key)
    assert len(d['phases']) == cycles + 1
    for i, phase in enumerate(d['phases']):
        assert phase['cycle'] == i
        assert phase['operations'] == (n if i == 0 else n * (4 if case == 'mixed' else 2) // 10)
        assert phase['verification']['rows'] == n
        assert phase['peak']['sample_errors'] == 0 and phase['peak']['samples'] > 0
        old = phase['snapshot_verification']
        if i and reader != 'none':
            version = 0 if reader == 'held' else i - 1
            assert old['rows'] == n
            assert old['crc32c'] == d['phases'][version]['verification']['crc32c']
        else:
            assert old is None
    assert d['reopen_verification']['rows'] == n
    assert d['reopen_verification']['crc32c'] == d['phases'][-1]['verification']['crc32c']
    assert d['alter_peak']['sample_errors'] == 0
    assert (d['reader_release'] is not None) == (reader != 'none')
    if d['reader_release']:
        assert d['reader_release']['peak']['sample_errors'] == 0
    visits += n * (cycles + 3 + (cycles if reader != 'none' else 0))
    d.update(label=label, evidence=str(p))
    reports.append(d)
    return d

def pair(label, n, cycles, case='mixed', reader='none', times=False):
    global checks
    a, b = [read(label, e, n, cycles, case, reader, times) for e in ('e4', 'sqlite')]
    for x, y in zip(a['phases'], b['phases']):
        assert x['verification']['crc32c'] == y['verification']['crc32c'], label
        checks += 1
        if x['snapshot_verification']:
            assert x['snapshot_verification']['crc32c'] == y['snapshot_verification']['crc32c']
            checks += 1
    assert a['reopen_verification']['crc32c'] == b['reopen_verification']['crc32c']
    checks += 1

if PI:
    for n in (100000, 400000):
        pair(f'mixed-{n}', n, 12)
    pair('held-100000', 100000, 4, reader='held')
else:
    for rep in (1, 2):
        for n in (100000, 400000):
            pair(f'mixed-{n}-r{rep}', n, 12)
    for n in (100000, 400000):
        pair(f'load-{n}', n, 0, case='load')
        for case in ('updates', 'reinsert'):
            pair(f'{case}-{n}', n, 4, case=case)
        for reader in ('held', 'rolling'):
            pair(f'{reader}-{n}', n, 4, reader=reader)
        pair(f'timestamps-{n}', n, 4, times=True)
    for n in (100000, 400000):
        a = read(f'ablation-{n}', 'e4', n, 12, ablation=True)
        b = next(d for d in reports if d['label'] == f'mixed-{n}-r1' and d['engine'] == 'e4')
        for x, y in zip(a['phases'], b['phases']):
            assert x['verification']['crc32c'] == y['verification']['crc32c']
            checks += 1
        assert a['reopen_verification']['crc32c'] == b['reopen_verification']['crc32c']
        checks += 1

def metrics(d):
    ps = d['phases']
    peaks = [p['peak'] for p in ps] + [d['alter_peak']]
    if d['reader_release']:
        peaks.append(d['reader_release']['peak'])
    logical = max(p['sampled_peak_logical'] for p in peaks)
    allocated = max(p['sampled_peak_allocated'] for p in peaks)
    return dict(load_seconds=ps[0]['seconds'], churn_seconds=sum(p['seconds'] for p in ps[1:]),
                loaded_bytes=ps[0]['final_logical'], peak_bytes=logical, peak_allocated_bytes=allocated,
                churn_final_bytes=ps[-1]['final_logical'], logical_factor=logical/ps[0]['final_logical'],
                allocated_factor=allocated/ps[0]['final_allocated'])

for d in reports:
    d['summary'] = metrics(d)
    if PI:
        p = BASE / 'matrix' / f"{d['label']}-{d['engine']}.log"
        usage = json.loads(p.read_text().splitlines()[-1])['resource_usage']
        assert usage['exit_code'] == 0
        d['resource_usage'] = usage
summary = dict(arms=len(reports), compared_states=checks, exact_oracle_row_visits=visits,
               results=reports, peak_semantics='1ms sampled lower bounds; all phases including reader release and alteration; not caps')
(OUT / f'{PREFIX}_RESULTS.json').write_text(json.dumps(summary, indent=2)+'\n')
lines = [f'# {PREFIX.replace("_", " ")} benchmark tables — 2026-09-12', '',
         'MiB = 1,048,576 bytes; times are seconds. Every arm is shown. Peak includes',
         'all database files, WAL, reader release and alteration; 1 ms samples are lower bounds, not caps.', '',
         '| Run | Engine | Load s | Churn s | Loaded MiB | Peak MiB | Churn final MiB | Allocated peak MiB | Logical factor | Allocated factor |',
         '|---|---|---:|---:|---:|---:|---:|---:|---:|---:|']
for d in reports:
    m = d['summary']
    lines.append(f"| {d['label']} | {d['engine']} | {m['load_seconds']:.3f} | {m['churn_seconds']:.3f} | {m['loaded_bytes']/2**20:.3f} | {m['peak_bytes']/2**20:.3f} | {m['churn_final_bytes']/2**20:.3f} | {m['peak_allocated_bytes']/2**20:.3f} | {m['logical_factor']:.3f}× | {m['allocated_factor']:.3f}× |")
lines += ['', '## Reader release', '', '| Run | Engine | Release s | Logical retained MiB | Allocated retained MiB |', '|---|---|---:|---:|---:|']
for d in reports:
    r = d['reader_release']
    if r:
        lines.append(f"| {d['label']} | {d['engine']} | {r['seconds']:.3f} | {r['final_bytes'][0]/2**20:.3f} | {r['final_bytes'][1]/2**20:.3f} |")
lines += ['', '## Phase detail', '', '| Run | Engine | Cycle | Operations | Time s | Logical peak MiB | Logical final MiB | Allocated peak MiB |', '|---|---|---:|---:|---:|---:|---:|---:|']
for d in reports:
    for p in d['phases']:
        lines.append(f"| {d['label']} | {d['engine']} | {p['cycle']} | {p['operations']} | {p['seconds']:.3f} | {p['peak']['sampled_peak_logical']/2**20:.3f} | {p['final_logical']/2**20:.3f} | {p['peak']['sampled_peak_allocated']/2**20:.3f} |")
if PI:
    lines += ['', '## Process usage', '', 'Each child used `prlimit --as=134217728` (128 MiB virtual address space).', 'RSS excludes filesystem cache and other services.', '', '| Run | Engine | Max RSS MiB | User s | System s |', '|---|---|---:|---:|---:|']
    for d in reports:
        u = d['resource_usage']
        lines.append(f"| {d['label']} | {d['engine']} | {u['max_rss_kib_linux']/1024:.3f} | {u['user_seconds']:.3f} | {u['system_seconds']:.3f} |")
(OUT / f'{PREFIX}_TABLES.md').write_text('\n'.join(lines)+'\n')
print(json.dumps({k:v for k,v in summary.items() if k != 'results'}))
