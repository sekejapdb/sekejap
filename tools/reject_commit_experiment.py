"""Archive, revert and clean a completed experiment that missed its threshold."""
import difflib
import hashlib
import json
from pathlib import Path
import shutil
import tarfile

root = Path(__file__).resolve().parents[1]
base = Path('<scratch>')
result = json.loads((root/'docs/COMMIT_EXPERIMENT_RESULTS.json').read_text())
assert max(max(r['load_improvement_pct'],r['churn_improvement_pct']) for r in result['summary']) < 10
assert not (base/'rejected.json').exists()
archive = Path('<scratch>')
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
provenance = json.loads((root/'docs/WRITE_PATH_PROVENANCE.json').read_text())
checked = {}
for name, expected in provenance['source_members'].items():
    if name.startswith(('src/','kernel/','tests/')) or name=='CONTRACT.md':
        assert sha(root/name)==expected, name
        checked[name]=expected

# Preserve patches before restoring only the files this experiment changed.
files=('src/bin/collections.rs','kernel/src/pool.rs','kernel/src/store.rs')
patches={}
with tarfile.open(archive) as saved:
    for arm in ('baseline','candidate'):
        source=Path('/tmp/e4-commit-'+arm)
        patch=''
        for name in files:
            before=saved.extractfile(name).read()
            after=(source/name).read_bytes()
            patch+=''.join(difflib.unified_diff(before.decode().splitlines(True),
                after.decode().splitlines(True),fromfile='a/'+name,tofile='b/'+name))
        path=base/(arm+'.patch'); path.write_text(patch)
        patches[arm]=sha(path)
        for name in files:
            (source/name).write_bytes(saved.extractfile(name).read())
            assert sha(source/name)==provenance['source_members'][name]

removed=[]
for r in result['reports']:
    evidence=Path(r['evidence'])
    assert sha(evidence)==r['sha256']
    if r['batch']==1000 and r['rep']==1:
        continue
    d=json.loads(evidence.read_text())
    folder=evidence.parent/f"{d['engine']}-{d['rows']}-off"
    assert folder.resolve().is_relative_to((base/'matrix').resolve())
    assert folder.is_dir() and not folder.is_symlink()
    members=[]
    for p in folder.rglob('*'):
        assert not p.is_symlink()
        if p.is_file():
            st=p.stat(); members.append(dict(path=str(p.relative_to(folder)),logical=st.st_size,allocated=st.st_blocks*512))
    removed.append(dict(path=str(folder),files=members,evidence_sha256=r['sha256']))
assert len(removed)==24
cleanup=dict(complete=False,databases=removed,
    logical_bytes=sum(f['logical'] for d in removed for f in d['files']),
    allocated_bytes=sum(f['allocated'] for d in removed for f in d['files']))
(base/'cleanup-pending.json').write_text(json.dumps(cleanup,indent=2)+'\n')
for d in removed: shutil.rmtree(d['path'])
cleanup['complete']=True
(root/'docs/COMMIT_EXPERIMENT_CLEANUP.json').write_text(json.dumps(cleanup,indent=2)+'\n')
(base/'cleanup.json').write_text(json.dumps(cleanup,indent=2)+'\n')
decision=dict(decision='reject and revert',threshold_pct=10,production_unchanged=checked,
    baseline_archive=str(archive),baseline_archive_sha256=sha(archive),patch_sha256=patches,
    binaries=result['binaries'],results_sha256=sha(root/'docs/COMMIT_EXPERIMENT_RESULTS.json'),
    isolated_source_changes_reverted=True,
    retained='All reports, patches, binaries, logs; three 100K r1 comparison databases. No Pi/full-suite expansion after negative performance gate.')
(root/'docs/COMMIT_EXPERIMENT_PROVENANCE.json').write_text(json.dumps(decision,indent=2)+'\n')
(base/'rejected.json').write_text(json.dumps(decision,indent=2)+'\n')
lines=['\n## Decision: rejected and reverted\n',
    'All mean improvements fall below the predeclared 10% threshold. The candidate\nwas reverted in both isolated checkouts; production source never changed.\nNo Pi run or broader shipping suite was justified after this negative performance gate.\n',
    '| Batch / rows | Baseline load / churn s | Candidate load / churn s | SQLite load / churn s | Candidate churn improvement |',
    '|---|---:|---:|---:|---:|']
for r in result['summary']:
    cells=[f"{r[a]['load_seconds']:.3f} / {r[a]['churn_seconds']:.3f}" for a in ('baseline','candidate','sqlite')]
    n={1:1000,100:10000,1000:100000}[r['batch']]
    lines.append(f"| {r['batch']} / {n:,} | {' | '.join(cells)} | {r['churn_improvement_pct']:+.2f}% |")
lines+=['\n| Batch | Baseline peak MiB | Candidate peak MiB | SQLite peak MiB |',
    '|---|---:|---:|---:|']
for r in result['summary']:
    cells=[f"{r[a]['peak_logical']/1048576:.3f}" for a in ('baseline','candidate','sqlite')]
    lines.append(f"| {r['batch']} | {' | '.join(cells)} |")
lines+=['\nMeans of three repetitions; positive improvement means faster. Per-run times,\nlogical/allocated peaks and file sizes are in [audited results](COMMIT_EXPERIMENT_RESULTS.json).\n',
    f"All {result['arms']} arms and {result['three_way_state_comparisons']} three-way state comparisons passed.\n",
    'The one-write microtest is a real syscall improvement, but unchanged barriers\nstill dominate the complete operation. Avoiding unnecessary work must be judged\nat the collection boundary, not only inside the page writer.\n',
    f"Removed 24 audited disposable database directories: {cleanup['logical_bytes']:,} logical bytes / {cleanup['allocated_bytes']:,} allocated bytes. Main 100K comparisons and evidence remain.\n",
    'See [cleanup](COMMIT_EXPERIMENT_CLEANUP.json) and [source/revert provenance](COMMIT_EXPERIMENT_PROVENANCE.json).\n']
with (root/'docs/COMMIT_EXPERIMENT.md').open('a') as out: out.write('\n'.join(lines))
print(json.dumps(dict(decision=decision['decision'],removed=len(removed),logical=cleanup['logical_bytes'],allocated=cleanup['allocated_bytes'])))
