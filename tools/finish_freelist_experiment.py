"""Record a measured but unpromoted prototype without changing acceptance gates."""
import hashlib,json,tarfile
from pathlib import Path
root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
staged=json.loads((base/'staged.json').read_text())
mac=json.loads((root/'docs/FREELIST_RESULTS.json').read_text())
pi=json.loads((root/'docs/FREELIST_PI_RESULTS.json').read_text())
assert mac['arms']==24 and pi['arms']==12
assert (base/'pi-evidence/tests.status').read_text().strip()=='101'
assert 'retired-page bookkeeping full' in (base/'limits-final.log').read_text()
assert 'overflow overwrite file must plateau' in (base/'collection-safety.log').read_text()
assert not (base/'decision.json').exists()
old=json.loads((root/'docs/WRITE_PATH_PROVENANCE.json').read_text())['source_members']
for name,h in old.items():
    if name.startswith(('src/','kernel/','tests/')) or name=='CONTRACT.md':assert sha(root/name)==h,name
for name,h in staged['source_members'].items():assert sha(Path('/tmp/e4-free-candidate')/name)==h,name
candidate_pi=json.loads((base/'pi-evidence/candidate-source.json').read_text())
baseline_pi=json.loads((base/'pi-evidence/baseline-source.json').read_text())
assert candidate_pi==staged['source_members']
for name,h in baseline_pi.items():
    assert h==(staged['source_members'][name] if name=='src/bin/collections.rs' else old[name]),name
pi_bins=(base/'pi-evidence/binary.sha256').read_text().splitlines()
assert len(pi_bins)==2 and len({s.split()[0] for s in pi_bins})==2
assert {a:sha(base/a) for a in ('baseline','candidate')}==staged['binaries']

lines=['\n## Result: speed gain proven; prototype not promoted\n',
    'The active engine remains unchanged. The candidate is retained as an archived\nexperiment, but the isolated working source is reverted because the existing\ncapacity and plateau gates did not pass. Neither threshold is relaxed.\n']
for result in (mac,pi):
    lines += [f"### {result['platform']}\n",'| Case | Baseline load / churn s | Candidate load / churn s | SQLite load / churn s | Churn improvement |',
        '|---|---:|---:|---:|---:|']
    for r in result['summary']:
        cells=[f"{r[a]['load_seconds']:.3f} / {r[a]['churn_seconds']:.3f}" for a in ('baseline','candidate','sqlite')]
        lines.append(f"| {r['case']} | {' | '.join(cells)} | {r['churn_improvement_pct']:.1f}% |")
    lines += ['\n| Case | Baseline peak MiB | Candidate peak MiB | SQLite peak MiB |', '|---|---:|---:|---:|']
    for r in result['summary']:
        cells=[f"{r[a]['peak_logical']/1048576:.3f}" for a in ('baseline','candidate','sqlite')]
        lines.append(f"| {r['case']} | {' | '.join(cells)} |")
    lines.append('')
lines += ['Mac batch rows are two-run means; 400K/held cases and all Pi cases are single\nruns. Held-reader SQLite times include checkpoint busy waits.\n',
    'The final selected Mac checks have **171 passes and 5 failures**: one unchanged\nbookkeeping-cap gate and four unchanged overflow-plateau gates. The Pi subset\nhas **147 passes and the same bookkeeping-cap failure**. Recovery, stale-hint\nrejection, freelist-body corruption, fallback reads, held snapshots, packing\nfault tests and the configured 1024×1536 embedding workload pass their selected\nchecks. No complete shipping-suite pass is claimed. Earlier diagnostic failures\nand their fixture/format fixes remain in the logs.\n',
    f"All {mac['arms']+pi['arms']} timing arms passed their full row oracles and reopens; {mac['three_way_state_comparisons']+pi['three_way_state_comparisons']} three-way state comparisons passed.\n",
    'Audited per-run times, allocated sizes, final sizes, issued bytes and Pi RSS:\n[Mac results](FREELIST_RESULTS.json), [Pi results](FREELIST_PI_RESULTS.json).\n',
    'The useful conclusion is that separate allocator-file publication is a\nsubstantial commit cost. Adoption needs a metadata-reservation design that\npreserves constrained CRUD capacity and the existing stabilization gate; the\nspeed result alone does not earn a production change.\n']
with (root/'docs/EMBEDDED_FREELIST.md').open('a') as f:f.write('\n'.join(lines))

# Source remains fully reproducible from the verified archive before reversal.
source=Path('/tmp/e4-free-candidate')
accepted=Path('<scratch>')
changed=[]
with tarfile.open(accepted) as t:
    for name,h in staged['source_members'].items():
        if name not in old:
            (source/name).unlink();changed.append(name)
        elif h!=old[name]:
            (source/name).write_bytes(t.extractfile(name).read());changed.append(name)
for name,h in old.items():
    if name.startswith(('src/','kernel/','tests/')) and (source/name).exists():assert sha(source/name)==h,name
record=dict(decision='not promoted; isolated source reverted',performance='significant measured gain',
    remaining_gates=['tracked_pages=4 capacity regression','four overflow plateau regressions'],
    candidate_archive=staged['archive'],candidate_archive_sha256=sha(Path(staged['archive'])),
    candidate_source=staged['source_members'],mac_binaries=staged['binaries'],pi_binaries=pi_bins,
    pi_evidence_sha256=sha(base/'pi-evidence.tar.gz'),production_code_unchanged=True,
    isolated_files_reverted=changed,accepted_source_sha256=sha(accepted))
(root/'docs/FREELIST_PROVENANCE.json').write_text(json.dumps(record,indent=2)+'\n')
(base/'decision.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({k:v for k,v in record.items() if k not in ('candidate_source','isolated_files_reverted')}))
