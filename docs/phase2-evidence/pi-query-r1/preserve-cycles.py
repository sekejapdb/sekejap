#!/usr/bin/env python3
"""Preserve completed ARM candidate fixtures; never open a database."""
import hashlib
import json
import pathlib
import stat
import sys
import tarfile

root = pathlib.Path('<scratch>')
assert (root / 'run.exit').read_text().strip() == '0', 'native qualification is not complete'
expected_stages = ['old-five-build', 'helper-build-default', 'helper-build-retained',
                   'graph-fixtures', 'graph-cycles', 'multimodel-fixtures', 'multimodel-cycles',
                   'lifecycle-fixtures', 'lifecycle-cycles', 'query-default', 'query-retained']
assert (root / 'stages.tsv').read_text().splitlines() == [x + '\tPASS' for x in expected_stages]
sys.path.insert(0, str(root / 'src/tools'))
from phase2_preserved_compat import validate_report, validate_corpus

def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()

selected = set()
def add(path):
    if path.is_symlink():
        raise ValueError('symlink in preservation input: ' + str(path))
    if path.is_dir():
        for child in path.iterdir():
            if child.name != '__pycache__':
                add(child)
    else:
        assert path.is_file(), path
        selected.add(path.relative_to(root).as_posix())

for name in ('bin', 'src', 'old-five-src', 'logs', 'current-source.json', 'old-five-source.json',
             'current-source.tar.gz', 'old-five-source.tar.gz', 'vendor.tar.gz',
             'binaries.sha256', 'package.sha256', 'run.sh', 'run.log', 'run.exit',
             'run.started', 'stages.tsv', 'cycle-summary.json', 'platform.txt', 'rustc.txt',
             'memory-watch.py', 'preserve-cycles.py'):
    add(root / name)

families = []
for family, count in (('graph', 4), ('multimodel', 20), ('lifecycle', 80)):
    report = root / f'{family}-fixtures/REPORT.json'
    specification = validate_report(json.loads(report.read_text()))
    corpus = root / f'{family}-fixtures/corpus'
    validate_corpus(corpus, specification)
    assert len(specification['fixtures']) == count
    cycles = root / f'{family}-cycles/REPORT.json'
    cycle_report = json.loads(cycles.read_text())
    assert cycle_report['result'] == 'PASS' and cycle_report['protected_unchanged']
    assert len(cycle_report['arms']) == count * 4
    assert cycle_report['qualification_report_sha256'] == sha(report)
    for binary in specification['recorded_binaries'].values():
        assert sha(pathlib.Path(binary['path'])) == binary['sha256']
    for path in (report, corpus, cycles, root / f'{family}-build.json'):
        add(path)
    families.append({'family': family, 'originals': count, 'cycles': count * 4,
                     'report': report.relative_to(root).as_posix(), 'report_sha256': sha(report),
                     'corpus': corpus.relative_to(root).as_posix()})

for label, directory in (('current', 'src'), ('old-five', 'old-five-src')):
    for name, expected in json.loads((root / f'{label}-source.json').read_text()).items():
        assert sha(root / directory / name) == expected, (label, name)

entries = {name: {'sha256': sha(root / name), 'bytes': (root / name).stat().st_size,
                  'mode': stat.S_IMODE((root / name).stat().st_mode)} for name in sorted(selected)}
manifest = {'format': 'phase2-arm-candidate-archive-v1',
            'status': 'candidate-not-released-or-frozen',
            'scope': '104 original corpora, ARM binaries, 416 cross-build cycles, 22 native query tests',
            'source_root': str(root), 'runtime': 'unaccepted scalar-membership query candidate',
            'families': families, 'files': entries,
            'excluded': 'Cargo targets, generated mutation copies and mutable memory-watch status',
            'restore': 'Verify every member hash and mode. Run preserved ARM binaries only on Linux aarch64. '
                       'Use src/tools/phase2_rollback_compat.py with explicit restored corpus/report/bin paths; '
                       'reports retain historical absolute paths. Cross-revision needs independently pinned comparison build. '
                       'Original sources and fixtures must stay immutable; use a fresh external work directory. '
                       'src/.cargo/config.toml records the original vendoring location: override it in a build COPY only.'}
manifest_bytes = (json.dumps(manifest, sort_keys=True, indent=2) + '\n').encode()
archive = root / 'phase2-arm-candidate-r1.tar.gz'
assert not archive.exists(), 'refusing to replace an archive'
partial = archive.with_suffix(archive.suffix + '.partial')
assert not partial.exists(), 'refusing to replace a partial archive'
import io
with tarfile.open(partial, 'w:gz', compresslevel=1) as tar:
    for name in sorted(entries):
        tar.add(root / name, arcname=name, recursive=False)
    info = tarfile.TarInfo('ARCHIVE.json'); info.size = len(manifest_bytes); info.mode = 0o644
    tar.addfile(info, io.BytesIO(manifest_bytes))
for name, record in entries.items():
    assert sha(root / name) == record['sha256'], 'input changed during archive: ' + name
with tarfile.open(partial, 'r:gz') as tar:
    members = tar.getmembers()
    assert len(members) == len(entries) + 1
    for member in members:
        assert member.isfile(), member.name
        content = tar.extractfile(member)
        h = hashlib.sha256()
        for chunk in iter(lambda: content.read(1024 * 1024), b''): h.update(chunk)
        expected = hashlib.sha256(manifest_bytes).hexdigest() if member.name == 'ARCHIVE.json' else entries[member.name]['sha256']
        assert h.hexdigest() == expected, member.name
partial.rename(archive)
receipt = {'archive': archive.name, 'bytes': archive.stat().st_size, 'sha256': sha(archive),
           'manifest_sha256': hashlib.sha256(manifest_bytes).hexdigest(),
           'files': len(entries), 'originals': 104, 'rollback_cycles': 416,
           'result': 'PASS', 'scope': manifest['scope']}
(root / 'phase2-arm-candidate-r1.receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps(receipt, indent=2))
