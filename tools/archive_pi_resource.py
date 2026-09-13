#!/usr/bin/env python3
"""Retain run evidence; remove only verified, disposable matrix database arms."""
import hashlib
import json
import pathlib
import shutil
import sys
import tarfile

root = pathlib.Path('<scratch>')
matrix = root / 'matrix-address-space'
action = sys.argv[1]
assert action in ('archive', 'cleanup')
assert (matrix / 'complete').is_file(), 'matrix must finish before collection'
results = list(matrix.glob('*/results.json'))
assert len(results) == 18
for path in results:
    r = json.loads(path.read_text())
    assert r['complete']
    assert r['process_memory_limits']['address_space_soft_hard'] == ['134217728'] * 2
    assert all(a['reopened_verify']['exact'] for a in r['arms'])

if action == 'archive':
    target = root.parent / 'pi-evidence-final.tar.gz'
    assert not target.exists()
    files = []
    for p in sorted(root.rglob('*')):
        if not p.is_file() or p.is_symlink():
            continue
        rel = p.relative_to(root)
        if 'store' in rel.parts or 'tmp' in rel.parts:
            continue
        if p.suffix in ('.json', '.log', '.txt', '.sha256', '.started', '.completed') or p.name in (
            'lifecycle-pi', 'lifecycle-postboot-verifier', 'complete', 'build-complete', 'final-validation-complete'
        ):
            files.append(p)
    manifest = [{'path': str(p.relative_to(root)), 'bytes': p.stat().st_size,
                 'sha256': hashlib.sha256(p.read_bytes()).hexdigest()} for p in files]
    (root / 'evidence-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    with tarfile.open(target, 'w:gz') as tar:
        for p in files + [root / 'evidence-manifest.json']:
            tar.add(p, arcname=str(p.relative_to(root)), recursive=False)
    print(json.dumps({'archive': str(target), 'files': len(files),
                      'sha256': hashlib.sha256(target.read_bytes()).hexdigest()}))
else:
    # Controller only invokes this after the archive is copied and independently
    # checked, and the complete paired report is generated on the Mac.
    assert sys.argv[2:] == ['--evidence-copied-and-verified']
    assert (root / 'evidence-manifest.json').is_file()
    entries = []
    for path in sorted(results):
        r = json.loads(path.read_text())
        for arm in r['arms']:
            home = path.parent / f"{arm['case']}-{arm['arm']}-{arm['n']}"
            assert home.is_dir() and not home.is_symlink(), home
            assert home.parent.parent == matrix
            files = [p for p in home.rglob('*') if p.is_file()]
            assert all(not p.is_symlink() for p in home.rglob('*'))
            entries.append({'path': str(home), 'logical_bytes': sum(p.stat().st_size for p in files),
                            'allocated_bytes': sum(p.stat().st_blocks * 512 for p in files),
                            'files': len(files)})
    report = root / 'cleanup.json'
    assert not report.exists()
    report.write_text(json.dumps({'complete': False, 'directories': entries}, indent=2) + '\n')
    for entry in entries:
        shutil.rmtree(entry['path'])
    report.write_text(json.dumps({'complete': True, 'directories': entries,
        'logical_bytes_removed': sum(e['logical_bytes'] for e in entries),
        'allocated_bytes_removed': sum(e['allocated_bytes'] for e in entries)}, indent=2) + '\n')
    print(report.read_text())
