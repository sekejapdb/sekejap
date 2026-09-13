"""Audit three-arm write-path matrix and render reproducible tables."""
import json
import sys
from pathlib import Path

BASE = Path(sys.argv[1] if len(sys.argv) > 1 else '<scratch>')
PI = len(sys.argv) > 2 and sys.argv[2] == 'pi'
OUT = Path(__file__).resolve().parents[1] / 'docs'
PREFIX = 'WRITE_PATH_PI' if PI else 'WRITE_PATH'
reports = []
checks = visits = 0

TESTS = {
    False: [
        ('mixed-100000-r1', 100000, 12, 'mixed', 'none', False, 4, 0),
        ('mixed-400000-r1', 400000, 12, 'mixed', 'none', False, 4, 0),
        ('mixed-100000-r2', 100000, 12, 'mixed', 'none', False, 4, 0),
        ('mixed-400000-r2', 400000, 12, 'mixed', 'none', False, 4, 0),
        ('load-100000', 100000, 0, 'load', 'none', False, 4, 0),
        ('updates-100000', 100000, 4, 'updates', 'none', False, 4, 0),
        ('reinsert-100000', 100000, 4, 'reinsert', 'none', False, 4, 0),
        ('held-100000', 100000, 4, 'mixed', 'held', False, 4, 0),
        ('rolling-100000', 100000, 4, 'mixed', 'rolling', False, 4, 0),
        ('timestamps-100000', 100000, 4, 'mixed', 'none', True, 4, 0),
        ('vectors-stable', 10000, 4, 'updates', 'none', False, 1536, 0),
        ('vectors-changing', 10000, 4, 'updates', 'none', False, 1536, 1),
        ('vectors-held', 10000, 4, 'updates', 'held', False, 1536, 0),
        ('vectors-stable-r2', 10000, 4, 'updates', 'none', False, 1536, 0),
        ('vectors-changing-r2', 10000, 4, 'updates', 'none', False, 1536, 1),
        ('vectors-held-r2', 10000, 4, 'updates', 'held', False, 1536, 0),
    ],
    True: [
        ('mixed-400000', 400000, 12, 'mixed', 'none', False, 4, 0),
        ('vectors-stable', 10000, 4, 'updates', 'none', False, 1536, 0),
        ('vectors-changing', 10000, 4, 'updates', 'none', False, 1536, 1),
        ('vectors-held', 10000, 4, 'updates', 'held', False, 1536, 0),
    ]
}

def read(label, arm, engine, n, cycles, case, reader, times, dim, changes):
    global visits
    times_str = 'on' if times else 'off'
    p = BASE / 'matrix' / label / arm / f'{engine}-{n}-{times_str}.json'
    d = json.loads(p.read_text())

    assert d['engine'] == engine, f'{p}: engine'
    assert d['rows'] == n, f'{p}: rows'
    assert d['cycles'] == cycles, f'{p}: cycles'
    assert d['case'] == case, f'{p}: case'
    assert d['reader'] == reader, f'{p}: reader'
    assert d['timestamps'] == times, f'{p}: timestamps'
    assert d['cache_bytes'] == 8388608, f'{p}: cache_bytes'
    assert d['transaction_operations'] == 1000, f'{p}: transaction_operations'
    assert d['collections'] == 2, f'{p}: collections'
    assert d['vector_dim'] == dim, f'{p}: vector_dim'
    assert d['change_vectors'] == (changes == 1), f'{p}: change_vectors'
    assert len(d['phases']) == cycles + 1, f'{p}: phases length'

    for i, phase in enumerate(d['phases']):
        assert phase['cycle'] == i, f'{p}: phase {i} cycle'
        assert phase['operations'] == (n if i == 0 else n * (4 if case == 'mixed' else 2) // 10), f'{p}: operations'
        assert phase['verification']['rows'] == n, f'{p}: row count'
        if engine == 'e4':
            b = phase['issued_bytes']
            assert all(isinstance(b[k], int) and b[k] >= 0 for k in ('data','wal','sidecars','total'))
            assert b['total'] == b['data'] + b['wal'] + b['sidecars']
        else:
            assert phase['issued_bytes'] is None
        assert phase['peak']['sample_errors'] == 0, f'{p}: phase {i} sample_errors'
        assert phase['peak']['samples'] > 0, f'{p}: phase {i} samples'

        old = phase['snapshot_verification']
        if i and reader != 'none':
            version = 0 if reader == 'held' else i - 1
            assert old is not None, f'{p}: phase {i} snapshot_verification'
            assert old['rows'] == n, f'{p}: snapshot row count'
            assert old['crc32c'] == d['phases'][version]['verification']['crc32c'], f'{p}: phase {i} snapshot crc'
        else:
            assert old is None, f'{p}: phase {i} snapshot'

    assert d['reopen_verification']['rows'] == n, f'{p}: reopened row count'
    assert d['reopen_verification']['crc32c'] == d['phases'][-1]['verification']['crc32c'], f'{p}: reopen crc'
    assert d['alter_peak']['sample_errors'] == 0, f'{p}: alter_peak sample_errors'
    assert (d['reader_release'] is not None) == (reader != 'none'), f'{p}: reader_release'
    if d['reader_release']:
        assert d['reader_release']['peak']['sample_errors'] == 0, f'{p}: reader_release sample_errors'

    visits += n * (cycles + 3 + (cycles if reader != 'none' else 0))
    d.update(label=label, arm=arm, evidence=str(p))
    reports.append(d)
    return d

def check_arms(label, n, cycles, case, reader, times, dim, changes):
    global checks
    baseline = read(label, 'baseline', 'e4', n, cycles, case, reader, times, dim, changes)
    current = read(label, 'current', 'e4', n, cycles, case, reader, times, dim, changes)
    sqlite = read(label, 'sqlite', 'sqlite', n, cycles, case, reader, times, dim, changes)

    for i, (bp, cp, sp) in enumerate(zip(baseline['phases'], current['phases'], sqlite['phases'])):
        bv, cv, sv = bp['verification']['crc32c'], cp['verification']['crc32c'], sp['verification']['crc32c']
        assert bv == cv == sv, f'{label}: phase {i} current crc mismatch'
        checks += 1
        if bp['snapshot_verification']:
            bsv, csv, ssv = (
                bp['snapshot_verification']['crc32c'],
                cp['snapshot_verification']['crc32c'],
                sp['snapshot_verification']['crc32c']
            )
            assert bsv == csv == ssv, f'{label}: phase {i} snapshot crc mismatch'
            checks += 1

    assert baseline['reopen_verification']['crc32c'] == current['reopen_verification']['crc32c'] == sqlite['reopen_verification']['crc32c'], f'{label}: reopen crc'
    checks += 1

for test in TESTS[PI]:
    check_arms(*test)
assert len(reports) == (12 if PI else 48)
assert len(list((BASE/'matrix').glob('*/*/*.json'))) == len(reports)

def metrics(d):
    ps = d['phases']
    peaks = [p['peak'] for p in ps] + [d['alter_peak']]
    if d['reader_release']:
        peaks.append(d['reader_release']['peak'])
    logical = max(p['sampled_peak_logical'] for p in peaks)
    allocated = max(p['sampled_peak_allocated'] for p in peaks)
    churn_issued = {'data': 0, 'wal': 0, 'sidecars': 0}
    for p in ps[1:]:
        if p['issued_bytes'] is not None:
            churn_issued['data'] += p['issued_bytes']['data']
            churn_issued['wal'] += p['issued_bytes']['wal']
            churn_issued['sidecars'] += p['issued_bytes']['sidecars']
    return dict(
        load_seconds=ps[0]['seconds'],
        churn_seconds=sum(p['seconds'] for p in ps[1:]),
        loaded_bytes=ps[0]['final_logical'],
        peak_bytes=logical,
        peak_allocated_bytes=allocated,
        churn_final_bytes=ps[-1]['final_logical'],
        logical_factor=logical/ps[0]['final_logical'],
        allocated_factor=allocated/ps[0]['final_allocated'],
        churn_issued_bytes=churn_issued if d['engine'] == 'e4' else None,
        churn_issued_total=sum(churn_issued.values()) if d['engine'] == 'e4' else None
    )

for d in reports:
    d['summary'] = metrics(d)
    if PI:
        p = BASE / 'matrix' / f"{d['label']}-{d['arm']}.log"
        last_line = p.read_text().strip().splitlines()[-1]
        usage = json.loads(last_line).get('resource_usage', {})
        assert usage.get('exit_code') == 0, f'{p}: exit_code'
        assert all(k in usage for k in ('max_rss_kib_linux','user_seconds','system_seconds'))
        d['resource_usage'] = usage

summary = dict(
    arms=len(reports),
    compared_states=checks,
    exact_oracle_row_visits=visits,
    results=reports,
    peak_semantics='1ms sampled lower bounds; all phases including reader release and alteration; not caps'
)
fingerprints = (BASE/'binary.sha256').read_text().splitlines()
assert len(fingerprints) == 2 and len({line.split()[0] for line in fingerprints}) == 2, 'baseline/current binaries must differ'
summary['binary_fingerprints'] = fingerprints
(OUT / f'{PREFIX}_RESULTS.json').write_text(json.dumps(summary, indent=2)+'\n')

date_str = '2026-09-12'
lines = [
    f'# {PREFIX.replace("_", " ")} benchmark tables — {date_str}',
    '',
    'MiB = 1,048,576 bytes; times are seconds. All three arms shown: baseline (original E4),',
    'current (new E4), and sqlite (SQLite). Peak includes all database files, WAL, reader release',
    'and schema alteration; 1 ms samples are lower bounds, not caps.',
    '',
    '| Run | Arm | Load s | Churn s | Loaded MiB | Peak MiB | Churn final MiB | Allocated peak MiB | Logical factor | Allocated factor |',
    '|---|---|---:|---:|---:|---:|---:|---:|---:|---:|'
]

for d in reports:
    m = d['summary']
    lines.append(
        f"| {d['label']} | {d['arm']} | {m['load_seconds']:.3f} | {m['churn_seconds']:.3f} | "
        f"{m['loaded_bytes']/2**20:.3f} | {m['peak_bytes']/2**20:.3f} | {m['churn_final_bytes']/2**20:.3f} | "
        f"{m['peak_allocated_bytes']/2**20:.3f} | {m['logical_factor']:.3f}× | {m['allocated_factor']:.3f}× |"
    )

lines += ['', '## E4 issued writes during churn', '',
          'Bytes handed to the storage writers, including rewritten/reused pages; these are not file sizes or device-level NAND writes. SQLite issued bytes were not instrumented.', '',
          '| Run | Arm | Data MiB | WAL MiB | Sidecars MiB | Total MiB |', '|---|---|---:|---:|---:|---:|']
for d in reports:
    b = d['summary']['churn_issued_bytes']
    if b is not None:
        lines.append(f"| {d['label']} | {d['arm']} | {b['data']/2**20:.3f} | {b['wal']/2**20:.3f} | {b['sidecars']/2**20:.3f} | {sum(b.values())/2**20:.3f} |")

lines += ['', '## Reader release', '', '| Run | Arm | Release s | Logical retained MiB | Allocated retained MiB |', '|---|---|---:|---:|---:|']
for d in reports:
    r = d['reader_release']
    if r:
        lines.append(
            f"| {d['label']} | {d['arm']} | {r['seconds']:.3f} | {r['final_bytes'][0]/2**20:.3f} | "
            f"{r['final_bytes'][1]/2**20:.3f} |"
        )

lines += ['', '## Phase detail', '', '| Run | Arm | Cycle | Operations | Time s | Logical peak MiB | Logical final MiB | Allocated peak MiB |', '|---|---|---:|---:|---:|---:|---:|---:|']
for d in reports:
    for p in d['phases']:
        lines.append(
            f"| {d['label']} | {d['arm']} | {p['cycle']} | {p['operations']} | {p['seconds']:.3f} | "
            f"{p['peak']['sampled_peak_logical']/2**20:.3f} | {p['final_logical']/2**20:.3f} | "
            f"{p['peak']['sampled_peak_allocated']/2**20:.3f} |"
        )

if PI:
    lines += ['', '## Process usage', '', 'Each process used `prlimit --as=134217728` (128 MiB virtual address space).', 'RSS excludes filesystem cache and other services.', '', '| Run | Arm | Max RSS MiB | User s | System s |', '|---|---|---:|---:|---:|']
    for d in reports:
        u = d['resource_usage']
        lines.append(
            f"| {d['label']} | {d['arm']} | {u.get('max_rss_kib_linux', 0)/1024:.3f} | "
            f"{u.get('user_seconds', 0):.3f} | {u.get('system_seconds', 0):.3f} |"
        )

(OUT / f'{PREFIX}_TABLES.md').write_text('\n'.join(lines)+'\n')
print(json.dumps({k:v for k,v in summary.items() if k != 'results'}))
