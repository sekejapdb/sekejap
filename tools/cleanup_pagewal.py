"""Delete only audited pilot DB folders; retain source, binary and all reports."""
import hashlib,json,shutil,sys
from pathlib import Path
platform=sys.argv[1];assert platform in ('mac','pi')
base=Path('<scratch>') if platform=='mac' else Path('<scratch>')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
plan=json.loads((base/f'cleanup-{platform}-input.json').read_text())
assert sha(base/'pilot-source.tar.gz')==plan['archive_sha256']
assert sha(base/'pilot')==plan['binary_sha256']
assert (base/'matrix.complete').exists()
manifest=base/'cleanup.json';assert not manifest.exists()
def entry(item):
    rel=Path(item['report']);assert not rel.is_absolute() and '..' not in rel.parts
    p=base/rel;assert p.name=='report.json' and sha(p)==item['sha256']
    folder=p.parent/'db';assert folder.is_dir() and not folder.is_symlink() and folder.resolve()==folder
    files=[]
    for f in sorted(folder.rglob('*')):
        assert not f.is_symlink()
        if f.is_file():
            s=f.stat();files.append(dict(path=str(f.relative_to(folder)),logical=s.st_size,allocated=s.st_blocks*512))
    assert files
    return dict(path=str(folder),report=str(p),report_sha256=sha(p),files=files,
        logical=sum(f['logical'] for f in files),allocated=sum(f['allocated'] for f in files))
keep=[];deleted=[]
for item in plan['reports']:(keep if item['keep'] else deleted).append(entry(item))
assert len(keep)==3 and len(deleted)==(36 if platform=='mac' else 15)
record=dict(platform=platform,status='planned',retained=keep,deleted=deleted,
    removed_logical_bytes=sum(e['logical'] for e in deleted),removed_allocated_bytes=sum(e['allocated'] for e in deleted))
manifest.write_text(json.dumps(record,indent=2)+'\n')
for e in deleted:
    assert sha(Path(e['report']))==e['report_sha256'];shutil.rmtree(e['path'])
for e in deleted:assert not Path(e['path']).exists()
for e in keep:assert Path(e['path']).is_dir()
record['status']='complete';manifest.write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({k:v for k,v in record.items() if k not in ('retained','deleted')}))
