#!/usr/bin/env python3
"""Record checked matrices as local, portable evidence and Markdown tables."""
import json
from pathlib import Path
import statistics as stats
import subprocess
import sys

roots = [Path(p) for p in sys.argv[1:]]
if not roots:
    raise SystemExit('Supply completed sustained matrix roots')
records = []
for root in roots:
    summary = json.loads(subprocess.check_output([sys.executable, str(Path(__file__).with_name('check_sustained.py')), str(root)]))
    records.append({'root': str(root), **summary})
docs = Path(__file__).resolve().parent.parent / 'docs'
(docs / 'SUSTAINED_RESULTS.json').write_text(json.dumps(records, indent=2) + '\n')
lines = ['# Sustained benchmark tables', '',
         'MB means decimal megabytes. Times are medians across the stated repetitions; peaks are the largest sampled value across those repetitions. Peaks include all arm files and close/reopen. Requested sampling interval: 1 ms. These are observations, not enforced limits.', '',
         'Load uses 1,000-operation transactions and automatic checkpoints. Every later churn run starts with the same fresh load. Rows contain typed scalars, a point, binary JSON and schemaless extras; no timestamps, vectors, graph edges or indexes.', '',
         '## Churn', '',
         '| Variant / case / rows | Pairs | E4 time s (range) | SQLite time s (range) | E4 / SQLite logical peak MB | E4 / SQLite allocated peak MB | E4 / SQLite final MB |',
         '|---|---:|---:|---:|---:|---:|---:|']
for record in records:
    for key,g in record['groups'].items():
        if int(key.rsplit('/', 1)[1]) < 100000: continue
        e,s = g['summary']['e4'],g['summary']['sqlite']
        def timing(v):
            t=v['mutation_seconds'];return f"{t['median']:.2f} ({t['min']:.2f}–{t['max']:.2f})"
        lines.append(f"| {key} | {g['repetitions']} | {timing(e)} | {timing(s)} | {e['peak_bytes']['max']/1e6:.2f} / {s['peak_bytes']['max']/1e6:.2f} | {e['allocated_peak_bytes']['max']/1e6:.2f} / {s['allocated_peak_bytes']['max']/1e6:.2f} | {e['final_bytes']['median']/1e6:.2f} / {s['final_bytes']['median']/1e6:.2f} |")
lines += ['', '## Fresh load, before any update or delete', '',
          'Separate from the earlier bulk/phase-end ingest reports: the 4 MiB WAL-or-page checkpoint experiment runs during this load, retaining reusable CoW pages.', '',
          '| Variant / rows | Paired loads | E4 / SQLite load s | E4 / SQLite logical peak MB | E4 / SQLite retained MB at load end |',
          '|---|---:|---:|---:|---:|']
loads={}
for root in roots:
    for p in sorted(root.glob('*/results.json')):
        d=json.loads(p.read_text())
        if not d.get('complete') or 'sqlite_native_autocheckpoint_pages' not in d:continue
        for a in d['arms']:
            if a['n']<100000:continue
            k=(p.parent.name.split('-')[0],a['n'])
            loads.setdefault(k,{'e4':[],'sqlite':[]})[a['arm']].append(a['stages'][0])
for (mode,n),engines in sorted(loads.items()):
    def metric(engine,key):return [s[key] for s in engines[engine]]
    e,s=engines['e4'],engines['sqlite'];assert len(e)==len(s)
    lines.append(f"| {mode} / {n} | {len(e)} | {stats.median(x['seconds'] for x in e):.2f} / {stats.median(x['seconds'] for x in s):.2f} | {max(x['peak']['sampled_peak_bytes'] for x in e)/1e6:.2f} / {max(x['peak']['sampled_peak_bytes'] for x in s)/1e6:.2f} | {stats.median(x['disk']['total_bytes'] for x in e)/1e6:.2f} / {stats.median(x['disk']['total_bytes'] for x in s)/1e6:.2f} |")
lines += ['', '## Observed expansion from each engine’s post-load footprint', '',
          'The denominator is that engine’s retained size after the same automatic-checkpoint load, already including reusable pages. It is not compact live payload size. Factors are measured maxima, not safety guarantees; allocated filesystem blocks can exceed logical lengths.', '',
          '| Variant / case / rows | E4 / SQLite peak factor | E4 / SQLite growth between final two even cycle ends, bytes |',
          '|---|---:|---:|']
for record in records:
    for key,g in record['groups'].items():
        if not (key.startswith('direct/') or key.startswith('matrix/')) or key.endswith('/1000'):continue
        runs=g['runs']
        e=max(r['e4']['sampled_growth_factor'] for r in runs)
        s=max(r['sqlite']['sampled_growth_factor'] for r in runs)
        eg=max(r['e4']['last_two_even_cycle_growth_bytes'] for r in runs)
        sg=max(r['sqlite']['last_two_even_cycle_growth_bytes'] for r in runs)
        lines.append(f"| {key} | {e:.2f}× / {s:.2f}× | {eg:,} / {sg:,} |")
lines += ['', '## Full typed reads and reopen', '',
          'Warm OS cache after verification, fixed engine cache. Point reads materialize 3,000 complete rows. Scan materializes all ordered rows including ID, without oracle generation or checksum serialization. SQLite converts JSONB through json() and the Rust consumer parses it into the same Value type. These are consumer-visible typed reads, not raw-page or cold-volume throughput.', '',
          '| Variant / case / rows | E4 / SQLite point ms | E4 / SQLite scan ms | E4 / SQLite reopen ms |',
          '|---|---:|---:|---:|']
for record in records:
    for key,g in record['groups'].items():
        if not (key.startswith('direct/') or key.startswith('matrix/')) or key.endswith('/1000'):continue
        runs=g['runs']
        if not all(r['e4'].get('scan') for r in runs):continue
        def median(a,k):return stats.median(r[a][k]['seconds'] for r in runs)*1000
        lines.append(f"| {key} | {median('e4','reads'):.2f} / {median('sqlite','reads'):.2f} | {median('e4','scan'):.2f} / {median('sqlite','scan'):.2f} | {g['summary']['e4']['reopen_seconds']['median']*1000:.2f} / {g['summary']['sqlite']['reopen_seconds']['median']*1000:.2f} |")
lines += ['', '## Evidence roots', ''] + [f'- `{root}`' for root in roots]
lines += ['', 'See [interpretation, policy differences, validation and limits](SUSTAINED_FOUNDATION.md). The machine restarted between candidate and direct runs; before/after wall-time differences are not wholly attributable to the codec. Each table retains a contemporaneous SQLite counterpart.', '']
(docs / 'SUSTAINED_TABLES.md').write_text('\n'.join(lines))
print('Recorded', len(records), 'checked matrices')
