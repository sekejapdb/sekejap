"""Publish the measured pilot verdict and seven-law qualification gaps."""
import hashlib,json,re
from pathlib import Path
root=Path(__file__).resolve().parents[1];base=Path('<scratch>')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
mac=json.loads((root/'docs/PAGEWAL_RESULTS.json').read_text());pi=json.loads((root/'docs/PAGEWAL_PI_RESULTS.json').read_text())
assert mac['arms']==39 and pi['arms']==18
prov=json.loads((base/'provenance.json').read_text());assert sha(Path(prov['archive']))==prov['archive_sha256']
assert sha(base/'pilot')==prov['binary_sha256']
assert prov['archive_sha256'] in (base/'pi-evidence/hashes.txt').read_text()
old=json.loads((root/'docs/REUSE_PROVENANCE.json').read_text())['source_members']
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')) or n=='CONTRACT.md':assert sha(root/n)==h,n
for d in (mac,pi):
    for r in d['reports']:assert sha(Path(r['report']))==r['sha256']
for name in ('final-pilot-tests.log','pi-evidence/tests.log'):
    assert 'test result: ok. 11 passed; 0 failed' in (base/name).read_text()
assert 'test result: ok. 129 passed; 0 failed' in (base/'kernel-tests.log').read_text()
laws=[
 dict(id='L1-MEM',law=1,status='PENDING',passed='Fixed 8MiB cache, 16MiB WAL bound; 18 Pi arms under 128MiB address-space cap',remaining='Aggregate allocation accounting, bounded reader admission and persisted resource policy'),
 dict(id='L2-WORK',law=2,status='PENDING',passed='Stable-page path, bounded WAL lookup and change-page checkpoint; 10K/40K/100K/400K probes',remaining='Fixed-change-count N ladder with I/O counters and exponent gate'),
 dict(id='L3-ATOMIC',law=3,status='PENDING',passed='Four checkpoint process-death points, uncommitted eviction rollback and independent checkpoint read-back',remaining='Append/commit/checkpoint EIO/short-write/fsync/ENOSPC matrix and source-preserving repair'),
 dict(id='L4-COST',law=4,status='PASS',passed='Explicit source/binaries, layer, timings, peaks, native sync settings, Pi RSS and named limitations',remaining='No full-engine qualification implied'),
 dict(id='L5-DAMAGE',law=5,status='FAIL',passed='Page/frame/transaction CRC and identity validation; corrupt WAL refused without modification',remaining='Independent salvage, root/schema/free/overflow fault matrix and current-membership proof are absent'),
 dict(id='L6-READ',law=6,status='PENDING',passed='Old/new snapshots, opening during pending writes, cap refusal preserves reader, checkpoint defers for readers',remaining='Cross-process readers, reader-admission/coordination and latency/I/O regression gates; surviving reader limits writer reopen'),
 dict(id='L7-DEVICE',law=7,status='PENDING',passed='Mac/Pi raw-KV load, live writes, reopen and sustained churn',remaining='Complete typed collection integration; bulk and indexed live writes/late indexing remain pending'),
]
workloads=[]
for name in ('W-LOAD','W-UPDATE','W-RESIZE','W-MIXED','W-SUSTAIN'):
    workloads.append(dict(id=name,status='PASS',scope='Executed raw-KV comparison, not a promise of parity'))
for name,why in [('W-DELETE','Mixed delete timing exists; delete-only matrix pending'),('W-REINSERT','Mixed fresh-ID reinserts exist; standalone matrix pending'),('W-READERS','Functional tests only; timed held/rolling/process matrix pending'),('W-CAP','Focused 2x completion/refusal tests only; N/device/held-reader matrix pending'),('W-REPAIR','Independent salvage absent')]:
    workloads.append(dict(id=name,status='PENDING',remaining=why))
registry=dict(version='F1',prototype='stable-page page-image WAL',production_promotable=False,
    verdict='Retain isolated architecture experiment; do not promote',laws=laws,workloads=workloads,
    fixtures={'F-T32':'10K smoke','F-T256':'40K smoke,100K repeated,400K sustained','F-T8192':'Overflow within F-TVAR only; standalone pending','F-TVAR':'1K six-round probe','F-H4':'PENDING','F-H1536':'PENDING','F-SCHEMA':'PENDING'},
    evidence={'Mac':dict(arms=39,state_comparisons=76,tests='129 kernel + 11 pilot test entries (one subprocess helper)'),
              'Pi':dict(arms=18,state_comparisons=42,tests='11 pilot test entries; four deliberate child exits; no full kernel suite this run')},
    parity={'Mac':[{k:r[k] for k in ('case','time_target','size_target','acceptance_repetitions')} for r in mac['summary']],
            'Pi':[{k:r[k] for k in ('case','time_target','size_target','acceptance_repetitions')} for r in pi['summary']]})
(root/'docs/FOUNDATION_GATES.json').write_text(json.dumps(registry,indent=2)+'\n')
provenance=dict(prototype=prov,pi_hashes=(base/'pi-evidence/hashes.txt').read_text(),production_source_unchanged=True,
    accepted_source_archive='<scratch>',decision=registry['verdict'])
(root/'docs/PAGEWAL_PROVENANCE.json').write_text(json.dumps(provenance,indent=2)+'\n')
lines=['\n## Measured verdict\n',
    '**Retain the isolated architecture experiment; do not promote it.** It',
    'earns substantial gains over current E4 and near-SQLite ordinary-text density.',
    'Mac mutation time and both platforms\' resize costs miss F1 parity. The Pi',
    'ordinary-text cases meet the observed <=1.10x time/size targets; single-run',
    '400K and load/resize probes still lack acceptance repetitions. Independent',
    'salvage and the remaining reader/resource gates prevent promotion.\n',
    'The seven-law [machine-readable gate registry](FOUNDATION_GATES.json)',
    'marks incomplete gates PENDING and absent independent recovery FAIL.',
    'Ten substantive pilot scenarios plus one child helper pass on each platform,',
    'including four deliberately terminated checkpoint children. The 129 existing',
    'kernel unit tests pass on Mac. This is not a full shipping suite.\n']
for d in (mac,pi):
    lines += [f"### {d['platform']} raw KV results\n",'Seconds for the whole phase; changes exclude initial loading and include',
        'commits and ending checkpoints. Three-run cases use medians; all other',
        'rows are single-run probes. Rows contain text-like payload bytes, not',
        'the earlier hybrid people/collection representation.\n',
        '| Case | Reps | Current E4 load / changes s | Page-WAL load / changes s | SQLite load / changes s |',
        '|---|---:|---:|---:|---:|']
    for r in d['summary']:
        vals=['{:.3f} / {:.3f}'.format(r[a]['load_seconds'],r[a]['mutation_seconds']) for a in ('e4','pagewal','sqlite')]
        lines.append('| {} | {} | {} |'.format(r['case'],r['pagewal']['repetitions'],' | '.join(vals)))
    lines += ['\n| Case | Current E4 final / peak MB | Page-WAL final / peak MB | SQLite final / peak MB | Page-WAL time / size target |',
        '|---|---:|---:|---:|---|']
    for r in d['summary']:
        vals=['{:.3f} / {:.3f}'.format(r[a]['final_logical']/1e6,r[a]['peak_logical']/1e6) for a in ('e4','pagewal','sqlite')]
        lines.append('| {} | {} | {} / {} |'.format(r['case'],' | '.join(vals),r['time_target'],r['size_target']))
    lines += ['\nMB is decimal; logical size includes side files. All raw allocated-size',
        'and expansion-factor samples remain in the result JSON. Peak samples',
        'are lower bounds. An observed target pass does not establish all seven laws.\n']
lines += ['### 400K sustained work, separated by operation\n',
    'Each arm performs 480K creates, 960K updates and 480K deletes after loading.',
    'Operation timers exclude commit and final-checkpoint work; total also',
    'includes value generation and harness overhead. Initial loading is excluded.\n',
    '| Platform / engine | Creates s | Updates s | Deletes s | Commits s | Ending checkpoints s | Total changes s |',
    '|---|---:|---:|---:|---:|---:|---:|']
for d in (mac,pi):
    for r in d['reports']:
        if r['case']=='sustain-400k':
            x=r['create_update_delete_seconds']
            lines.append('| {} / {} | {:.3f} | {:.3f} | {:.3f} | {:.3f} | {:.3f} | {:.3f} |'.format(d['platform'],r['arm'],*x,r['mutation_commit_seconds'],r['mutation_checkpoint_seconds'],r['mutation_seconds']))
lines += ['\nAt 400K, all three engines plateau in cycles 10–12. Page-WAL stays at',
    '129,888,256 logical bytes; SQLite stays at 129,445,888 bytes. This is a',
    'single sustained probe per platform, not the repeated acceptance gate.\n',
    'Pi benchmarks use `prlimit --as=134217728` for every engine. Its shared',
    'services remain running. At 400K, maximum RSS is '+
    ', '.join('{} {:.2f} MiB'.format(r['arm'],r['resource_usage']['max_rss_kib_linux']/1024) for r in pi['reports'] if r['case']=='sustain-400k')+'.',
    'This is process RSS, not filesystem cache or complete allocation accounting.\n',
    'The ordinary 256-byte rows achieve similar final density; overflow resize',
    'does not. The existing whole-value overflow representation and page reuse',
    'need separate investigation; no measured causal ablation isolates that gap yet.',
    'Raw delete work also remains more expensive than SQLite. These findings',
    'do not justify dropping checksums, read-back verification or reader protection.\n',
    'Raw results: [Mac](PAGEWAL_RESULTS.json), [Pi](PAGEWAL_PI_RESULTS.json).',
    'Source/binary hashes: [provenance](PAGEWAL_PROVENANCE.json).\n',
    'Reproduce from the archived `pilot-source.tar.gz`, building',
    '`pagewal_bench` with `--release --offline --features sqlite-balance,compact-cells`.',
    'Arguments are `OUTPUT ENGINE N SIZE CYCLES CASE BATCH`; ENGINE is `e4`,',
    '`pagewal` or `sqlite`. `run_pagewal_pilot.py` and `pagewal_pi.sh` retain the',
    'exact matrix; choose a fresh authorized artifact root before rerunning.\n']
with (root/'docs/PAGEWAL_PILOT.md').open('a') as f:f.write('\n'.join(lines))
print(json.dumps(dict(verdict=registry['verdict'],mac_arms=39,pi_arms=18,production_promotable=False)))
