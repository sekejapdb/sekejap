import hashlib, json, os, pathlib, shutil, subprocess, tarfile
base=pathlib.Path('<scratch>')
source=base/'phase2-native-20260917-r1'
archive=source/'phase2-arm-candidate-r1.tar.gz'
expected='5f2554a6e1a57aafaebd97c39e3bef06a15b6b82be6503f956b6150f69f5891d'
def sha(path):
 h=hashlib.sha256()
 with path.open('rb') as f:
  for chunk in iter(lambda:f.read(1024*1024),b''):h.update(chunk)
 return h.hexdigest()
assert sha(archive)==expected
root=base/'phase2-native-20260917-restore-r1';root.mkdir(exist_ok=False)
restored=root/'preserved';restored.mkdir()
with tarfile.open(archive,'r:gz') as tar:
 members=tar.getmembers();names=[m.name for m in members]
 manifest=json.load(tar.extractfile('ARCHIVE.json'))
 assert len(names)==len(set(names)) and set(names)==set(manifest['files'])|{'ARCHIVE.json'}
 for m in members:
  p=pathlib.PurePosixPath(m.name)
  assert m.isfile() and not p.is_absolute() and '..' not in p.parts
  target=restored/m.name;target.parent.mkdir(parents=True,exist_ok=True)
  with tar.extractfile(m) as f, target.open('xb') as out:shutil.copyfileobj(f,out)
  target.chmod(m.mode)
  if m.name!='ARCHIVE.json':
   record=manifest['files'][m.name]
   assert sha(target)==record['sha256'] and target.stat().st_size==record['bytes']
   assert (m.mode & 0o777)==record['mode']
reports=[]
for family,binary,count in [('graph','phase2_format_fixture',4),('multimodel','multimodel_format_fixture',20),('lifecycle','phase2_lifecycle_fixture',80)]:
 report=restored/f'{family}-fixtures/REPORT.json'
 command=['python3','-B',str(restored/'src/tools/phase2_rollback_compat.py'),
          '--qualification-report',str(report),'--report-sha256',sha(report),
          '--corpus',str(restored/f'{family}-fixtures/corpus'),
          '--qualification-kind','cross-build','--work',str(root/f'{family}-replay')]
 for mode in ('default','retained'):
  path=str(restored/'bin'/f'{binary}-{mode}')
  command.extend(['--baseline-bin',mode,path,'--comparison-bin',mode,path])
 with (root/f'{family}-replay.log').open('w') as log:
  subprocess.run(command,check=True,stdout=log,stderr=subprocess.STDOUT,env={**os.environ,'PYTHONDONTWRITEBYTECODE':'1'})
 p=root/f'{family}-replay/REPORT.json';d=json.loads(p.read_text())
 assert d['result']=='PASS' and d['protected_unchanged'] and len(d['arms'])==count*4
 reports.append({'family':family,'cycles':count*4,'report':str(p),'sha256':sha(p)})
for name,record in manifest['files'].items():assert sha(restored/name)==record['sha256'],name
assert sha(archive)==expected
result={'result':'PASS','archive_sha256':expected,'restored_files':len(manifest['files']),
        'source_archive_unchanged':True,'all_restored_inputs_unchanged':True,
        'fixture_generation_invoked':False,'rollback_cycles':416,'families':reports,
        'scope':'Relocated archive restoration/replay, same candidate binaries; not new engine or cross-release qualification'}
(root/'RESTORE_REPORT.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
