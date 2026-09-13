"""Archive final source, independently verify members, and record provenance."""
import hashlib
import json
from pathlib import Path
import tarfile

root = Path(__file__).resolve().parents[1]
base = Path('<scratch>')
target = base / 'packing-final-source.tar.gz'
assert not target.exists()
files = [root/p for p in ('Cargo.toml','Cargo.lock','README.md','CONTRACT.md','AGENTS.md','.gitignore')]
for name in ('src', 'kernel', 'tests', 'tools', 'docs'):
    files += [p for p in (root/name).rglob('*') if p.is_file()
              and not any(x in ('target','__pycache__','.git') or x.startswith('._') for x in p.relative_to(root).parts)
              and p.name not in ('.DS_Store','PACKING_PROVENANCE.json')]
files.sort()
assert all(not p.is_symlink() for p in files)
digest = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
members = {str(p.relative_to(root)): digest(p) for p in files}
with tarfile.open(target, 'w:gz') as archive:
    for p in files:
        archive.add(p, arcname=str(p.relative_to(root)), recursive=False)
with tarfile.open(target, 'r:gz') as archive:
    actual = {m.name: hashlib.sha256(archive.extractfile(m).read()).hexdigest() for m in archive.getmembers()}
assert actual == members
pi_source = {}
for line in Path('/tmp/e4-packing-pi-source.sha256').read_text().splitlines():
    sha, p = line.split()
    assert members[p] == sha
    pi_source[p] = sha
provenance = dict(source_archive=str(target), source_archive_sha256=digest(target),
    source_members=members, pi_verified_source=pi_source,
    binaries={p.name:digest(p) for p in [base/'collections-candidate',base/'collections-baseline',base/'collection-inspect']},
    pi_binary_sha256=(base/'pi-evidence/binary.sha256').read_text().strip(),
    pi_evidence_archive_sha256=digest(base/'pi-evidence.tar.gz'),
    archive_verified=True,
    note='Archive excludes this generated provenance file. Old kernel control uses previous collection source with current benchmark harness.')
(root/'docs/PACKING_PROVENANCE.json').write_text(json.dumps(provenance,indent=2)+'\n')
(base/'provenance.json').write_text(json.dumps(provenance,indent=2)+'\n')
print(json.dumps({k:v for k,v in provenance.items() if k not in ('source_members','pi_verified_source','binaries')}))
