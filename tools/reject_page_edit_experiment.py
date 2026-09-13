"""Record the failed significance gate and restore only isolated prototype files."""
import hashlib,json,re,shutil,tarfile
from pathlib import Path
root=Path(__file__).resolve().parents[1]
source=Path('/tmp/e4-batch-candidate')
base=Path('<scratch>')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
staged=json.loads((base/'staged.json').read_text())
result=json.loads((root/'docs/PAGE_EDIT_RESULTS.json').read_text())
old=json.loads((root/'docs/REUSE_PROVENANCE.json').read_text())['source_members']
assert result['arms']==20 and result['five_way_state_comparisons']==24
assert not (base/'decision.json').exists()
assert sha(Path(staged['archive']))==staged['archive_sha256']
for n,h in staged['source_members'].items():assert sha(source/n)==h,n
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')) or n=='CONTRACT.md':assert sha(root/n)==h,n
for row in result['summary']:
    for arm in ('scratch','cell','combined'):
        assert row[arm]['mutation_reduction_pct']<10
        assert row[arm]['final_logical']==row['baseline']['final_logical']
        assert row[arm]['peak_logical']==row['baseline']['peak_logical']
tests=[]
for name in ('combined-tests.log','combined-safety.log'):
    tests+=re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed', (base/name).read_text())
assert sum(int(t[1]) for t in tests)==176 and all(t[0]=='ok' and t[2]=='0' for t in tests)

lines=['\n## Verdict: rejected and reverted\n',
    'The primitives passed their selected safety checks but did not meet the\n10% end-to-end significance gate. All three prototypes are rejected for now.\nThe accepted E4 engine remains the persistent-freelist version from the prior\nloop; its 347-test Mac / 182-test Pi validation is unchanged. No query or\nmultimodel-index code was changed.\n',
    '| Engine / prototype | Initial load, mixed case s | Mixed mutations s | Initial load, update case s | Update-only mutations s |',
    '|---|---:|---:|---:|---:|']
mixed,updates=result['summary']
for arm,title in [('baseline','Accepted E4'),('scratch','Scratch-only — rejected'),('cell','Cell-only — rejected'),('combined','Both — rejected'),('sqlite','SQLite')]:
    lines.append(f"| {title} | {mixed[arm]['load_seconds']:.3f} | {mixed[arm]['mutation_seconds']:.3f} | {updates[arm]['load_seconds']:.3f} | {updates[arm]['mutation_seconds']:.3f} |")
lines += ['\nEach timing is the mean of two reversed-order repetitions, in seconds. The\nmixed case performs 160,000 mutations total against 100,000 initial people;\nthe update-only case performs 80,000 replacements. Initial load is excluded\nfrom mutation time. SQLite remains faster in both comparisons.\n',
    '| Prototype | Mixed reduction vs E4 | Update-only reduction vs E4 |',
    '|---|---:|---:|']
for arm in ('scratch','cell','combined'):
    lines.append(f"| {arm} | {mixed[arm]['mutation_reduction_pct']:.2f}% | {updates[arm]['mutation_reduction_pct']:.2f}% |")
lines += ['\nThe largest mean improvement is 5.52% for scratch-only updates; it varies\nfrom about 2.5% to 8.5% across the two repetitions. Combining both changes does\nnot produce a larger end-to-end gain. This is a negative performance gate,\nnot a claim of observed data loss.\n',
    '| Case | All E4 variants peak / final MiB | SQLite peak / final MiB |',
    '|---|---:|---:|']
for row in result['summary']:
    lines.append(f"| {row['case']} | {row['baseline']['peak_logical']/1048576:.3f} / {row['baseline']['final_logical']/1048576:.3f} | {row['sqlite']['peak_logical']/1048576:.3f} / {row['sqlite']['final_logical']/1048576:.3f} |")
lines += ['\nLogical peaks and final sizes are identical between all E4 variants. There\nis no disk-density benefit. Allocated-size variation remains in the raw report.\nAll 20 timing arms passed full row/reopen checks; 24 five-way state comparisons\nagree. The combined prototype passed 176 selected kernel, resource, overflow,\ncorruption and packing checks; no full shipping-suite pass is claimed for it.\n',
    'No Pi timing matrix or larger 400K acceptance run was launched after the\nMac gate failed. The Pi was queried only for profiling support; `perf` was not\ninstalled and nothing was installed or reconfigured. This result does not prove\nthe same percentages on Pi.\n',
    'The three-second Mac profile remains the useful direction signal: commit\nwork dominated that sample, so fewer temporary allocations cannot remove most\nof the observed time. A future commit-protocol experiment needs its own proof\nof durability, snapshot visibility, bounded metadata and peak-space behavior.\nIncreasing transaction size or weakening durability is not this result.\n',
    'See [audited results](PAGE_EDIT_RESULTS.json) and [source/binary provenance](PAGE_EDIT_PROVENANCE.json).\nThe combined source archive plus `scratch-page.rs`, `cell-page.rs` and\n`combined-btree.rs` reproduce the individual ablations. Their retained binaries\nwere built with identical benchmark harnesses and are fingerprinted separately.\nThe isolated source is restored to the accepted engine after archiving.\n']
with (root/'docs/PAGE_EDIT_EXPERIMENT.md').open('a') as f:f.write('\n'.join(lines))

changed=[]
accepted=Path('<scratch>')
with tarfile.open(accepted) as t:
    for n,h in staged['source_members'].items():
        if n not in old:
            (source/n).unlink();changed.append(n)
        elif h!=old[n]:
            (source/n).write_bytes(t.extractfile(n).read());changed.append(n)
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')):assert sha(source/n)==h,n
record=dict(decision='rejected: all end-to-end gains below 10%; isolated source restored',
            accepted_engine='persistent freelist',production_code_unchanged=True,laws_unchanged=True,
            selected_tests=176,benchmark_arms=20,five_way_state_comparisons=24,
            isolated_files_reverted=changed,prototype=staged,accepted_source_archive=str(accepted),
            accepted_source_sha256=sha(accepted),pi_timing_run=False)
(base/'decision.json').write_text(json.dumps(record,indent=2)+'\n')
(root/'docs/PAGE_EDIT_PROVENANCE.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({k:v for k,v in record.items() if k!='prototype'}))
