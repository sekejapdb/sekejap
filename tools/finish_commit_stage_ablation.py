"""Record a failed significance gate, restore the isolated source, then clean up."""
import hashlib, json, re, shutil, tarfile
from pathlib import Path
root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
source=Path('/tmp/e4-stage-candidate')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
result=json.loads((base/'results.json').read_text())
assert result['arms']==24 and result['state_comparisons']==36
assert (base/'matrix.complete').exists() and not (base/'decision.json').exists()
assert all(r['reduction_pct']<10 for r in result['summary']), 'Significant mean: review before rejecting'
provenance=json.loads((base/'provenance.json').read_text())
assert sha(Path(provenance['archive']))==provenance['archive_sha256']
for n,h in provenance['source_members'].items():assert sha(source/n)==h,n
old=json.loads((root/'docs/REUSE_PROVENANCE.json').read_text())['source_members']
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')) or n=='CONTRACT.md':assert sha(root/n)==h,n
tests=[]
for n in ('kernel-green.log','safety-green.log'):
    tests+=re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed', (base/n).read_text())
assert sum(int(t[1]) for t in tests)==161 and all(t[0]=='ok' and t[2]=='0' for t in tests)

lines=['\n## Verdict: rejected and reverted\n',
       'No workload reaches the 10% mean improvement gate. The empty-WAL reset',
       'shortcut is rejected; the accepted persistent-freelist engine remains',
       'unchanged. This is a negative performance result, not an observed',
       'correctness failure. No Pi timing or larger acceptance matrix followed',
       'the failed Mac gate. No claim of equivalent Pi gains is made.\n',
       '| Workload | Current E4 load / changes s | Candidate load / changes s | SQLite load / changes s | Candidate change-time reduction |',
       '|---|---:|---:|---:|---:|']
for r in result['summary']:
    values=['{:.3f} / {:.3f}'.format(r[a]['load_seconds'],r[a]['mutation_seconds']) for a in ('baseline','candidate','sqlite')]
    lines.append('| {} | {} | {:.2f}%{} |'.format(r['label'],' | '.join(values),r['reduction_pct'],' (load)' if r['label']=='load' else ''))
lines+=['\nMean of two repetitions with engine order reversed. Negative reductions',
        'mean slower. All timings are whole-phase seconds, including commits and',
        'ending checkpoints, excluding full-row/reopen verification. Load-only:',
        '100K initial people, zero mutations. Updates: 100K people, 80K total',
        'replacements. Mixed: 100K people, 80K updates + 40K deletes + 40K inserts.',
        'Small: 1K people, 400 updates + 200 deletes + 200 inserts, one operation',
        'per commit. All other cases use 1000 operations per commit.\n',
        '| Workload | Current E4 peak / final MiB | Candidate peak / final MiB | SQLite peak / final MiB |',
        '|---|---:|---:|---:|']
for r in result['summary']:
    values=['{:.6f} / {:.6f}'.format(r[a]['peak_logical']/1048576,r[a]['final_logical']/1048576) for a in ('baseline','candidate','sqlite')]
    lines.append('| {} | {} |'.format(r['label'],' | '.join(values)))
lines+=['\nThese are sampled logical peaks and final file sizes. Allocated peaks and',
        'each repetition remain in the [raw results](COMMIT_STAGE_RESULTS.json).',
        'Sampling variation is not an enforced maximum expansion guarantee.\n',
        '**161 selected tests pass**: 133 kernel tests (including four new reset',
        'tests), 5 durability, 2 persistent-freelist, 8 resource-limit, 6 snapshot',
        'and 7 collection tests. All 24 benchmark arms pass full row/reopen',
        'oracles and all 36 three-way state comparisons agree. This rejected',
        'candidate did not run the full shipping suite or physical power cuts.\n',
        'The significant finding is where time goes: data/root and freelist',
        'barriers dominate; avoiding an already-empty WAL reset cannot remove',
        'much elapsed time. Any future reduction in barrier count needs an',
        'independently validated publication design, not a weaker sync setting.\n']
with (root/'docs/COMMIT_STAGE_ABLATION.md').open('a') as f:f.write('\n'.join(lines))
with tarfile.open('<scratch>') as t:
    for n in ('kernel/src/wal.rs','src/bin/collections.rs'):
        (source/n).write_bytes(t.extractfile(n).read())
(source/'kernel/src/wal_reset_tests.rs').unlink()
for n,h in old.items():
    if n.startswith(('src/','kernel/','tests/')):assert sha(source/n)==h,n
decision=dict(verdict='rejected and reverted',reason='No workload mean meets 10% gate',
    production_unchanged=True,isolated_source_restored=True,selected_tests=161,
    benchmark_arms=24,state_comparisons=36,pi_timing_run=False,prototype=provenance)
(base/'decision.json').write_text(json.dumps(decision,indent=2)+'\n')
(root/'docs/COMMIT_STAGE_PROVENANCE.json').write_text(json.dumps(decision,indent=2)+'\n')

# Reports and archived source are verified before deleting any database.
def entry(p, expected=None):
    if expected:assert sha(p)==expected
    folder=p.with_suffix('')
    assert folder.is_dir() and not folder.is_symlink()
    assert folder.resolve()==folder and base in folder.parents
    files=[]
    for f in sorted(folder.rglob('*')):
        assert not f.is_symlink()
        if f.is_file():
            s=f.stat();files.append(dict(path=str(f.relative_to(folder)),logical=s.st_size,allocated=s.st_blocks*512))
    assert files
    return dict(path=str(folder),report=str(p),report_sha256=sha(p),files=files,
        logical=sum(f['logical'] for f in files),allocated=sum(f['allocated'] for f in files))
keep=[];remove=[]
for r in result['reports']:
    e=entry(Path(r['evidence']),r['sha256'])
    (keep if r['label']=='mixed' and r['rep']==1 else remove).append(e)
for batch,n in ((1,1000),(1000,100000)):
    p=base/f'profile-{batch}/e4-{n}-off.json';d=json.loads(p.read_text())
    assert d['reopen_verification']['rows']==n
    assert d['reopen_verification']['crc32c']==d['phases'][-1]['verification']['crc32c']
    remove.append(entry(p))
assert len(keep)==3 and len(remove)==23
cleanup=dict(status='planned',retained=keep,deleted=remove,
    removed_allocated_bytes=sum(e['allocated'] for e in remove),removed_logical_bytes=sum(e['logical'] for e in remove))
manifest=base/'cleanup.json';assert not manifest.exists()
manifest.write_text(json.dumps(cleanup,indent=2)+'\n')
for e in remove:
    assert entry(Path(e['report']),e['report_sha256'])==e
    shutil.rmtree(e['path'])
for e in remove:assert not Path(e['path']).exists() and sha(Path(e['report']))==e['report_sha256']
for e in keep:assert entry(Path(e['report']),e['report_sha256'])==e
cleanup['status']='complete'
manifest.write_text(json.dumps(cleanup,indent=2)+'\n')
(root/'docs/COMMIT_STAGE_CLEANUP.json').write_text(manifest.read_text())
with (root/'docs/COMMIT_STAGE_ABLATION.md').open('a') as f:
    f.write('\n## Retention and cleanup\n\n'
        'All binaries, source archives, profile stages, logs and reports remain in\n'
        'the scratch artifact directory. The first mixed run retains current E4,\n'
        'candidate and SQLite databases. Removed 21 other matrix databases and\n'
        f"two diagnostic databases: **{cleanup['removed_allocated_bytes']:,} allocated bytes**\n"
        '(file block accounting; actual filesystem free-space changes may differ).\n'
        'See [cleanup](COMMIT_STAGE_CLEANUP.json) and\n'
        '[source/binary provenance](COMMIT_STAGE_PROVENANCE.json).\n')
for n in ('COMMIT_STAGE_ABLATION.md','COMMIT_STAGE_RESULTS.json','COMMIT_STAGE_PROVENANCE.json','COMMIT_STAGE_CLEANUP.json'):
    shutil.copy2(root/'docs'/n,base/n)
for n in ('stage_commit_profile.py','run_commit_profile.py','run_commit_stage_ablation.py','finish_commit_stage_ablation.py'):
    shutil.copy2(root/'tools'/n,base/n)
print(json.dumps(dict(verdict=decision['verdict'],removed_allocated_bytes=cleanup['removed_allocated_bytes'],retained=3)))
