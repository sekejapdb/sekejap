"""Verify a failure archive without extracting or opening any database."""
import hashlib, json, sys, tarfile
from pathlib import Path
archive, manifest = map(Path, sys.argv[1:3])
expected = json.loads(manifest.read_text())
def digest(stream):
    h = hashlib.sha256()
    for block in iter(lambda: stream.read(1 << 20), b''): h.update(block)
    return h.hexdigest()
with archive.open('rb') as f: archive_hash = digest(f)
assert archive_hash == expected['archive_sha256']
seen = set()
with tarfile.open(archive) as tar:
    for member in tar:
        assert member.isfile() and member.name not in seen
        entry = expected['files'][member.name]
        assert member.size == entry['logical']
        with tar.extractfile(member) as f: assert digest(f) == entry['sha256']
        seen.add(member.name)
assert seen == expected['files'].keys()
print(json.dumps(dict(archive_sha256=archive_hash, verified_files=len(seen), logical_bytes=sum(f['logical'] for f in expected['files'].values()))))
