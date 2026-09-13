"""Archive final source and verify source/binary provenance for the write loop."""
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile

root=Path(__file__).resolve().parents[1]
base=Path('<scratch>')
target=base/'write-final-source.tar.gz'
assert not target.exists()
digest=lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
files=[root/p for p in ('Cargo.toml','Cargo.lock','README.md','CONTRACT.md','AGENTS.md','.gitignore')]
for name in ('src','kernel','tests','tools','docs'):
    files += [p for p in (root/name).rglob('*') if p.is_file()
              and not any(x in ('target','__pycache__','.git') or x.startswith('._') for x in p.relative_to(root).parts)
              and p.name not in ('.DS_Store','WRITE_PATH_PROVENANCE.json')]
files.sort()
assert all(not p.is_symlink() for p in files)
members={str(p.relative_to(root)):digest(p) for p in files}
with tarfile.open(target,'w:gz') as archive:
    for p in files: archive.add(p,arcname=str(p.relative_to(root)),recursive=False)
with tarfile.open(target,'r:gz') as archive:
    actual={m.name:hashlib.sha256(archive.extractfile(m).read()).hexdigest() for m in archive.getmembers()}
assert actual==members
old=Path('<scratch>')
with tarfile.open(old) as archive:
    unchanged=['CONTRACT.md']+[p for p in members if p.startswith('kernel/')]
    for p in unchanged:
        assert hashlib.sha256(archive.extractfile(p).read()).hexdigest()==members[p],p
pi_source={}
for line in (base/'pi-source.sha256').read_text().splitlines():
    sha,p=line.split()
    assert members[p]==sha,p
    pi_source[p]=sha
mac_bins={p.name:digest(p) for p in [base/'collections-current',base/'collections-baseline']}
assert len(set(mac_bins.values()))==2
pi_bins=(base/'pi-evidence/binary.sha256').read_text().splitlines()
assert len(pi_bins)==2 and len({s.split()[0] for s in pi_bins})==2
references={}
for name in ('sqlite','postgres'):
    repo=Path('<scratch>')/f'{name}-repo'
    references[name]=dict(path=str(repo),commit=subprocess.check_output(['git','-C',str(repo),'rev-parse','HEAD'],text=True).strip())
report=dict(source_archive=str(target),source_archive_sha256=digest(target),source_members=members,
    source_archive_verified=True,contract_and_kernel_unchanged_from_packing=True,
    baseline_source_archive_sha256=digest(old),pi_verified_source=pi_source,
    mac_binary_sha256=mac_bins,pi_binary_sha256=pi_bins,reference_sources=references,
    pi_evidence_archive_sha256=digest(base/'pi-evidence.tar.gz'),
    note='Initial Pi timing matrix excluded: shared build target reused baseline executable. Corrected matrix uses isolated target directories and distinct hashes. Archive excludes this generated provenance file.')
(root/'docs/WRITE_PATH_PROVENANCE.json').write_text(json.dumps(report,indent=2)+'\n')
(base/'provenance.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:v for k,v in report.items() if k not in ('source_members','pi_verified_source')}))
