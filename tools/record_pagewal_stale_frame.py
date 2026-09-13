"""Audit both platforms' raw reports and record the stale-frame loop."""
import hashlib
import json
from pathlib import Path
import re
import statistics
import tarfile

repo = Path(__file__).resolve().parents[1]
base = Path('<scratch>')
pi = base / 'pi-evidence'
pi.mkdir(exist_ok=True)
with tarfile.open('/tmp/e4-stale-pi-evidence.tar.gz') as archive:
    for member in archive:
        path = Path(member.name)
        assert member.isfile() and not path.is_absolute() and '..' not in path.parts
        destination = pi / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(archive.extractfile(member).read())

def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()

def tests(path):
    context = ''
    entries = {}
    text = path.read_text()
    for line in text.splitlines():
        if 'Running ' in line:
            context = line.strip()
        match = re.match(r'test (\S+) \.\.\.(.*)', line)
        if match:
            entries[(context, match[1])] = match[2]
    summaries = re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
    assert summaries and all(r[0] == 'ok' and r[2] == '0' for r in summaries)
    ignored = sum('ignored' in value for value in entries.values())
    return dict(distinct_passed=len(entries) - ignored, ignored=ignored,
        pass_events_including_subprocess_helpers=sum(int(r[1]) for r in summaries), sha256=sha(path))

records = []
platforms = {}
for name, directory in [('Mac', base), ('Pi', pi)]:
    result = json.loads((directory / 'comparison-results.json').read_text())
    assert not result['failures'] and len(result['records']) == 9
    summaries = {}
    for r in result['records']:
        suffix = Path(r['path']).parts
        relative = Path(*suffix[suffix.index('comparison'):])
        report_path = directory / relative
        assert sha(report_path) == r['sha256']
        assert json.loads(report_path.read_text()) == r['report']
        records.append(dict(platform=name, **r))
    for arm in ['pagewal-v2', 'pagewal-v3', 'sqlite']:
        rows = [r['report'] for r in result['records'] if r['arm'] == arm]
        assert len(rows) == 3
        times = [sum(p['seconds'] for p in r['phases'][1:]) for r in rows]
        summaries[arm] = dict(mutation_seconds=statistics.median(times), repetitions_seconds=times,
            load_seconds=statistics.median(r['phases'][0]['seconds'] for r in rows),
            final_logical_bytes=statistics.median(r['phases'][-1]['final_logical'] for r in rows),
            final_allocated_bytes=statistics.median(r['phases'][-1]['final_allocated'] for r in rows),
            peak_logical_bytes=max(p['peak']['logical'] for r in rows for p in r['phases']),
            peak_over_loaded=max(max(p['peak']['logical'] for p in r['phases']) / r['phases'][0]['final_logical'] for r in rows))
    new, old, sqlite = (summaries[n] for n in ['pagewal-v3', 'pagewal-v2', 'sqlite'])
    ratio = new['mutation_seconds'] / sqlite['mutation_seconds']
    size_ratio = new['final_logical_bytes'] / sqlite['final_logical_bytes']
    assert ratio < 1.5 and size_ratio <= 1.10
    assert new['final_logical_bytes'] == old['final_logical_bytes']
    platforms[name] = dict(arms=summaries, v3_over_sqlite_time=ratio, v3_over_sqlite_size=size_ratio,
        v3_over_v2_time=new['mutation_seconds'] / old['mutation_seconds'],
        tests=tests(directory / ('workspace-tests.log' if name == 'Mac' else 'tests.log')))

for phase in range(13):
    assert len({r['report']['phases'][phase]['verification']['crc32c'] for r in records}) == 1
assert sha(base / 'source.tar.gz') == sha(pi / 'source.tar.gz')
core = ['src/pagewal.rs', 'src/pagewal/repair.rs', 'src/pagewal_fault_tests.rs',
    'kernel/src/btree.rs', 'kernel/src/io.rs', 'kernel/src/pool.rs', 'kernel/src/store.rs']
with tarfile.open(base / 'source.tar.gz') as archive:
    for path in core:
        assert archive.extractfile(path).read() == (repo / path).read_bytes(), path

report = dict(verdict='Keep stale-frame guard; ordinary raw-KV parity passes. No release promotion.',
    code_commit='d0b96ee', baseline_commit='ee98182', imported_v2_commit='2c56936',
    rows=400000, rounds=12, updates=960000, deletes=480000, inserts=480000,
    layer='Raw KV: 8-byte key and 256-byte value; no collection schema or multimodel indexes',
    timing='Mutation time excludes initial load and includes commits and final checkpoint; medians of three rotations',
    peak='1ms samples of all database/supporting files; lower bounds, not enforced-cap proof',
    contributor@example.invalid; existing .43 trusted host key verified; 128MiB address-space limit per benchmark arm',
    platforms=platforms, records=records, source_archive_sha256=sha(base / 'source.tar.gz'),
    core_source_sha256={p: sha(repo / p) for p in core},
    control_page_comparisons=[json.loads((base / ('compare-' + str(n) + '.json')).read_text()) for n in [6, 11]],
    native_control_cause='Unresolved; each failed endpoint differs from a successful endpoint by one stale checksum-valid data page. Parent pointers match.',
    fault_tests=dict(stale_wal_red='stale-wal-red.log', green='pagewal-fault-green.log', io_cases=356, reopen_checks=712),
    remaining=['Native old-Store stale-page cause', 'Corrupt-WAL-region salvage and rootless current-membership proof',
        'Repair failure/resource matrix', 'Cross-process readers and zero-coordination gate', 'Resize/large-value parity', 'Typed collection integration'])
(repo / 'docs/PAGEWAL_STALE_FRAME_RESULTS.json').write_text(json.dumps(report, indent=2) + '\n')
(base / 'RESULTS.json').write_text(json.dumps(report, indent=2) + '\n')
gates_path = repo / 'docs/FOUNDATION_GATES.json'
gates = json.loads(gates_path.read_text())
gates['production_promotable'] = False
gates['verdict'] = 'Keep committed stale-frame guard; Mac/Pi ordinary raw-KV parity passes. Native old-Store stale-page cause and broader F1 recovery/reader/resize/integration gates remain open.'
gates['qualification_v3'] = dict(code_commit='d0b96ee', source_archive_sha256=report['source_archive_sha256'],
    platforms=platforms, raw_reports=18, ordinary_time_pass=True, ordinary_size_pass=True,
    report='docs/PAGEWAL_STALE_FRAME.md', results='docs/PAGEWAL_STALE_FRAME_RESULTS.json')
for law in gates['laws']:
    extra = {'L3-ATOMIC': ' Successful-but-dropped data-write test catches stale images before WAL deletion and recovers acknowledged rows.',
        'L5-DAMAGE': ' Expected frame CRC rejects checksum-valid stale WAL versions; red/green snapshot and checkpoint regression.'}.get(law['id'], '')
    if extra and extra not in law['passed']:
        law['passed'] += extra
for blocker in gates['release_blockers']:
    if blocker['id'] == 'CONTROL-SCAN':
        blocker['remaining'] = report['native_control_cause'] + ' Original failed files and exact successful endpoints preserved. The related page-WAL stale-version reader defect is fixed; the native old-Store failure is not declared fixed.'
gates_path.write_text(json.dumps(gates, indent=2) + '\n')
if Path('/tmp/e4-stale-pi-cleanup.json').exists():
    cleanup = dict(Mac=json.loads((base / 'cleanup.json').read_text()),
        Pi=json.loads(Path('/tmp/e4-stale-pi-cleanup.json').read_text()))
    (repo / 'docs/PAGEWAL_STALE_FRAME_CLEANUP.json').write_text(json.dumps(cleanup, indent=2) + '\n')
    (pi / 'cleanup.json').write_text(json.dumps(cleanup['Pi'], indent=2) + '\n')
    old_path = repo / 'docs/PAGEWAL_QUALIFICATION_CLEANUP.json'
    old = json.loads(old_path.read_text())
    old['Pi'] = json.loads(Path('/tmp/e4-qualification-pi-cleanup.json').read_text())
    old_path.write_text(json.dumps(old, indent=2) + '\n')
print(json.dumps(platforms, indent=2))
