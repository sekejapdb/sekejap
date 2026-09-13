"""Promote only the measured, byte-verified engine changes; retain all evidence."""
import hashlib, json, re, shutil, tarfile
from pathlib import Path

root = Path(__file__).resolve().parents[1]
source = Path('/tmp/e4-reuse-candidate')
base = Path('<scratch>')
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
old = json.loads((root/'docs/WRITE_PATH_PROVENANCE.json').read_text())['source_members']
staged = json.loads((base/'staged.json').read_text())
mac = json.loads((root/'docs/REUSE_RESULTS.json').read_text())
pi = json.loads((root/'docs/REUSE_PI_RESULTS.json').read_text())
assert not (base/'decision.json').exists()
assert mac['arms'] == 24 and pi['arms'] == 18
assert (base/'pi-evidence/tests.status').read_text().strip() == '0'
assert sha(Path(staged['archive'])) == staged['archive_sha256']
for name, h in staged['source_members'].items(): assert sha(source/name) == h, name
assert json.loads((base/'pi-evidence/candidate-source.json').read_text()) == staged['source_members']
for name, h in json.loads((base/'pi-evidence/baseline-source.json').read_text()).items():
    assert h == (staged['source_members'][name] if name == 'src/bin/collections.rs' else old[name]), name
for name, h in old.items():
    if name.startswith(('src/', 'kernel/', 'tests/')) or name == 'CONTRACT.md':
        assert sha(root/name) == h, name

def results(name):
    s = (base/name).read_text()
    return s, re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored', s)
s, workspace = results('workspace.log')
assert sum(int(x[1]) for x in workspace) == 346
assert all(x[0] == 'ok' and x[2:] == ('0', '0') for x in workspace)
assert '129 passed; 0 failed' in (base/'kernel-final.log').read_text()
assert '1 passed; 0 failed' in (base/'missing-wal-green.log').read_text()
assert '0 passed; 1 failed' in (base/'missing-wal-red.log').read_text()
assert '1 passed; 1 failed' in (base/'red.log').read_text()
assert '5 passed; 0 failed' in (base/'path-final.log').read_text()
assert '14 passed; 0 failed' in (base/'path-final.log').read_text()
_, p1 = results('pi-evidence/tests.log')
_, p2 = results('pi-evidence/tests-path-final.log')
assert sum(int(x[1]) for x in p1+p2) == 182
assert sum(int(x[2]) for x in p1) == 19  # Original Mac-only path guards.
assert all(x[0] == 'ok' for x in p2)

gains = []
for result in (mac, pi):
    for rep in (1, 2):
        rows = {x['arm']: x for x in result['reports'] if x['label'] == f'batch-1-r{rep}'}
        gain = 100*(1-rows['candidate']['churn_seconds']/rows['baseline']['churn_seconds'])
        assert gain >= 10
        gains.append(dict(platform=result['platform'], repetition=rep, gain_pct=gain))
    for row in result['summary']:
        assert row['baseline']['final_logical'] == row['candidate']['final_logical']
        assert row['candidate']['peak_logical'] <= row['baseline']['peak_logical']

changed = [n for n, h in staged['source_members'].items()
           if h != old.get(n) and n != 'src/bin/collections.rs']
expected = {'kernel/src/store.rs', 'kernel/src/verify.rs', 'kernel/src/wal.rs',
            'kernel/tests/persistent_free.rs', 'tests/overflow_lifecycle.rs', 'tests/recovery_faults.rs'}
assert set(changed) == expected, changed
for name in changed: shutil.copyfile(source/name, root/name)
for name, h in old.items():
    if name.startswith(('src/', 'kernel/', 'tests/')) or name == 'CONTRACT.md':
        assert sha(root/name) == (staged['source_members'][name] if name in expected else h), name

lines = ['\n## Verdict: retained in E4\n',
    'The change is retained for its repeatable small-commit benefit: **30.3% on Mac\nand 28.9% on Pi against accepted E4 before this loop**. The two individual\nsmall-commit gains exceed 10% on each machine. It does not establish SQLite\nwrite-speed parity. Pi 400K mutation time improves only 4.4%.\n',
    'All paired E4 final logical sizes are identical. Candidate sampled logical\npeaks are slightly lower because ordinary checkpoints no longer hold `free`\nand `free.tmp` together. No extra data pages or retirement entries are added.\n']
for result in (mac, pi):
    lines += [f"### {result['platform']}\n",
        '| Workload | E4 before load / mutations s | E4 after load / mutations s | SQLite load / mutations s | Mutation reduction vs E4 before |',
        '|---|---:|---:|---:|---:|']
    for row in result['summary']:
        cells = [f"{row[a]['load_seconds']:.3f} / {row[a]['churn_seconds']:.3f}" for a in ('baseline','candidate','sqlite')]
        lines.append(f"| {row['case']} | {' | '.join(cells)} | {row['churn_improvement_pct']:.1f}% |")
    lines += ['\n| Workload | E4 before peak / final MiB | E4 after peak / final MiB | SQLite peak / final MiB |',
              '|---|---:|---:|---:|']
    for row in result['summary']:
        cells = [f"{row[a]['peak_logical']/1048576:.3f} / {row[a]['final_logical']/1048576:.3f}" for a in ('baseline','candidate','sqlite')]
        lines.append(f"| {row['case']} | {' | '.join(cells)} |")
    lines.append('')
lines += [
    'Batch-1 = 1,000 initial people, two rounds of 400 mutations, one operation per\ncommit. Batch-100 = 10,000 people, three rounds of 4,000 mutations. Batch-1000\n= 100,000 people, four rounds of 40,000 mutations. Each round updates 20%,\ndeletes 10% and inserts 10%; comparisons are within a workload. Both platforms\nuse two reversed-order repetitions for batch-1/batch-100; Mac also repeats\nbatch-1000. 400K and held-reader results are single runs.\n',
    'At 400K, E4 after takes **2.27× SQLite time on Mac / 2.04× on Pi**. Pi held-case\ninitial load was 7.143 s after versus 6.751 s before, a 5.8% regression in that\nsingle run; its cause is not established. The shared Pi is not an idle dedicated\nbenchmark device. Do not generalize the small-commit gain to all workloads.\n',
    'Pi 400K maximum RSS is 10.70 MiB before / 10.75 MiB after / 12.11 MiB SQLite\nunder the common 128 MiB address-space limit. These are measured process peaks,\nnot filesystem-cache measurements or whole-process allocation-ledger proofs.\nMac allocated size can exceed logical size and fluctuate between runs; exact\nallocated peaks and issued bytes remain in the result JSON.\n',
    '**347 distinct Mac checks pass**: the complete 346-test workspace run plus\nthe subsequently added missing-WAL regression, with all 129 kernel tests rerun\nand the 19 path-adjusted lifecycle/recovery checks rerun. **182 selected Pi\nchecks pass**, including capacity, overflow plateaus, corruption recovery,\nsnapshots, process-killed merge writers and the constrained embedding case.\nThe initial Pi runner targeted one test in the wrong package; the next run\nfound 19 Mac-only artifact-path guards. Both diagnostics are retained. Only\nthe allowed artifact paths changed; no data-size, bookkeeping, plateau or\ncorrectness assertion was relaxed.\n',
    'The new missing-WAL crash-model test was also mutation-checked: removing\nits directory-publication barrier loses the acknowledged row; restoring the\nbarrier passes. Lost/torn freelist checks model persisted crash images, not\nphysical power-cut testing. A lost hint still sacrifices reuse knowledge.\nWriter reopen now pays one directory barrier, including when no WAL recreation\nwas needed; repeated-open latency was not separately benchmarked.\n',
    f"All {mac['arms']+pi['arms']} benchmark arms pass complete row/reopen oracles, with {mac['three_way_state_comparisons']+pi['three_way_state_comparisons']} three-way state comparisons.\n",
    'Raw reports: [Mac](REUSE_RESULTS.json), [Pi](REUSE_PI_RESULTS.json).\nSource/binary fingerprints and exact promoted files: [provenance](REUSE_PROVENANCE.json).\nThe benchmark batch-size override remains in the archived experimental harness;\nproduction benchmark source is unchanged. No service, SQL interface, deployment\nor seven-law contract was changed.\n']
with (root/'docs/PERSISTENT_FREELIST.md').open('a') as f: f.write('\n'.join(lines))

bins = {n:sha(base/n) for n in ('baseline','candidate')}
assert len(set(bins.values())) == 2
record = dict(decision='retained in E4 for repeatable frequent-small-write gains',
    promoted_files=changed, mac_distinct_tests=347, pi_selected_tests=182,
    benchmark_arms=42, three_way_state_comparisons=96, repeated_small_commit_gains=gains,
    candidate_archive=staged['archive'], candidate_archive_sha256=staged['archive_sha256'],
    candidate_source_members=staged['source_members'], mac_binaries=bins,
    pi_binaries=(base/'pi-evidence/binary.sha256').read_text().splitlines(),
    pi_evidence_sha256=sha(base/'pi-evidence.tar.gz'),
    baseline_archive='<scratch>',
    laws_unchanged=sha(root/'CONTRACT.md') == old['CONTRACT.md'])
(root/'docs/REUSE_PROVENANCE.json').write_text(json.dumps(record,indent=2)+'\n')
(base/'decision.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({k:v for k,v in record.items() if k not in ('candidate_source_members',)}))
