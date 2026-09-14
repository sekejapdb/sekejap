"""Archive metadata first, then delete only verified redundant task databases."""
import hashlib, json, shutil, sys, tarfile
from pathlib import Path
base = Path(sys.argv[1]).resolve()
assert str(base) in ['<scratch>', '<scratch>']
lean = json.loads((base/'restored-lean/results.json').read_text()); assert lean['status']=='PASS'
audit = json.loads((base/'refusal-audit/results.json').read_text()); assert len(audit)==6
assert all(r['source_unchanged'] and r['verification']['exact_loaded_state_after_refusal'] for r in audit)
paths=[]
for part in ['probe','tradeoff','refusal-audit','restored-lean','attempt-shared-target','attempt-missing-dependency','attempt-lean-path-guard']:
    folder=base/part
    if folder.exists():
        paths.extend(p for p in folder.rglob('*') if p.is_file() and p.suffix in ['.json','.log','.txt'])
paths.extend(p for p in base.iterdir() if p.is_file() and p.suffix in ['.json','.log','.txt','.py','.sh','.rs'])
with tarfile.open(base/'evidence.tar.gz','w:gz') as archive:
    for p in sorted(set(paths)): archive.add(p,arcname=str(p.relative_to(base)))
cleanup=[]
for profile in ['probe','tradeoff']:
    d=json.loads((base/profile/'results.json').read_text())
    if profile=='tradeoff':assert not d['failures'] and len(d['records'])==25
    cleanup.extend(Path(r['path'])/'db' for r in d['records'])
cleanup.extend(Path(r['path']).parent/'db' for r in lean['records'])
cleanup.extend(Path(r['copy']) for r in audit)
invalid=base/'attempt-shared-target/probe/results.json'
if invalid.exists():
    cleanup.extend(invalid.parent/Path(r['path']).name/'db' for r in json.loads(invalid.read_text())['records'])
removed=[]
for p in cleanup:
    assert p.is_relative_to(base) and p!=base and p.is_dir(),p
    files=[f for f in p.rglob('*') if f.is_file()]
    item=dict(path=str(p),logical=sum(f.stat().st_size for f in files),allocated=sum(f.stat().st_blocks*512 for f in files))
    shutil.rmtree(p);removed.append(item)
result=dict(removed=removed,allocated_reclaimed=sum(r['allocated'] for r in removed),logical_removed=sum(r['logical'] for r in removed),
    retained_failed_sources=[r['source'] for r in audit],evidence_sha256=hashlib.sha256((base/'evidence.tar.gz').read_bytes()).hexdigest())
(base/'cleanup.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(dict(removed=len(removed),allocated_reclaimed=result['allocated_reclaimed'])))
