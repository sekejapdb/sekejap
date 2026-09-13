import hashlib,json,shutil,sys
from pathlib import Path
base=Path(sys.argv[1]);audit=json.loads(Path(sys.argv[2]).read_text())
assert not (base/'cleanup.json').exists()
removed=[]
for row in audit['reports']:
    if row['label']=='mixed-400000':continue
    engine='sqlite' if row['arm']=='sqlite' else 'e4'
    rel=Path('matrix')/row['label']/row['arm']/f"{engine}-{row['rows']}-off.json"
    report=base/rel
    assert hashlib.sha256(report.read_bytes()).hexdigest()==row['sha256']
    folder=report.with_suffix('');assert folder.is_dir() and not folder.is_symlink()
    assert base.resolve() in folder.resolve().parents
    files=[]
    for p in folder.rglob('*'):
        assert not p.is_symlink()
        if p.is_file():
            st=p.stat();files.append(dict(path=str(p.relative_to(folder)),logical=st.st_size,allocated=st.st_blocks*512))
    removed.append(dict(path=str(folder.relative_to(base)),files=files,evidence_sha256=row['sha256']))
assert len(removed)==(15 if audit['platform']=='Pi' else 21)
out=dict(complete=False,removed=removed,retained='All three 400K comparison databases, reports, archived candidate, binaries and logs',
    logical_bytes=sum(f['logical'] for d in removed for f in d['files']),
    allocated_bytes=sum(f['allocated'] for d in removed for f in d['files']))
(base/'cleanup-pending.json').write_text(json.dumps(out,indent=2)+'\n')
for d in removed:shutil.rmtree(base/d['path'])
out['complete']=True
(base/'cleanup.json').write_text(json.dumps(out,indent=2)+'\n')
print(json.dumps({k:v for k,v in out.items() if k!='removed'}))
