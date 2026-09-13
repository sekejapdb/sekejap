"""One-shot cleanup of verified, disposable DB directories from this loop only."""
import hashlib,json,shutil,sys
from pathlib import Path
base=Path(sys.argv[1]).resolve();mode=sys.argv[2]
allowed=['<scratch>',
         '<scratch>']
assert str(base) in allowed and mode in ('plan','delete')
manifest=base/'cleanup.json';assert not manifest.exists()
summary=json.loads((base/'PAGEWAL_QUALIFICATION_RESULTS.json').read_text())
def sha(p):
    h=hashlib.sha256()
    with p.open('rb') as f:
        for block in iter(lambda:f.read(1<<20),b''):h.update(block)
    return h.hexdigest()
assert sha(base/'source.tar.gz')==summary['frozen_provenance']['artifacts']['source.tar.gz']
prefix='<scratch>/'
if str(base).startswith('/home/'):prefix+='pi-evidence/'
known={x['path'][len(prefix):]:x['sha256'] for x in summary['raw_reports'] if x['path'].startswith(prefix)}
remove=[];keep=[]
for folder in ['matrix','matrix-contended','cap','cap-short']:
    for db in sorted((base/folder).glob('*/*/db')):
        assert db.is_dir() and not db.is_symlink() and base in db.resolve().parents
        rel=str(db.relative_to(base));report=db.parent/'report.json';rp=str(report.relative_to(base))
        if not report.exists():keep.append(dict(path=rel,reason='Failed arm: preserve evidence'));continue
        assert rp in known and sha(report)==known[rp],rp
        retain=(rel.startswith('matrix/sustain-400k-r3/') or rel=='matrix/mixed-100k-r3/pagewal-v2/db'
                or rel.startswith('cap/100000-held/'))
        if retain:keep.append(dict(path=rel,reason='Final 400K control/candidate, repair source, or cap refusal evidence'));continue
        files=[]
        for p in sorted(db.rglob('*')):
            assert not p.is_symlink()
            if p.is_file():
                m=p.stat();files.append(dict(path=str(p.relative_to(base)),bytes=m.st_size,allocated=m.st_blocks*512,sha256=sha(p)))
        remove.append(dict(path=rel,report_sha256=known[rp],files=files))
result={'mode':mode,'base':str(base),'removed_databases':len(remove),'removed_logical_bytes':sum(f['bytes'] for x in remove for f in x['files']),
    'removed_allocated_bytes':sum(f['allocated'] for x in remove for f in x['files']),'removed':remove,'retained':keep,
    'also_retained':'repair-100k destination, all raw reports/logs, source archives, binaries and failure evidence'}
if mode=='delete':
    for x in remove:shutil.rmtree(base/x['path'])
    assert all(not (base/x['path']).exists() for x in remove)
    assert all((base/x['path']).is_dir() for x in keep)
    manifest.write_text(json.dumps(result,indent=2)+'\n')
else:(base/'cleanup-plan.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({k:v for k,v in result.items() if k not in ('removed','retained')},indent=2))
