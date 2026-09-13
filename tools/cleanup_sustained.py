#!/usr/bin/env python3
"""Remove only completed sustained-run arm directories, preserving result evidence."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

root = Path(sys.argv[1]).resolve()
if not root.is_relative_to(Path('<scratch>')):
    raise SystemExit('Only E4 scratch artifacts are eligible')
# Independently validate both engines, every state, snapshots, and reopen first.
subprocess.run([sys.executable, str(Path(__file__).with_name('check_sustained.py')), str(root)],
               check=True, stdout=subprocess.DEVNULL)
manifest_path = root / 'cleanup.json'
if manifest_path.exists():
    raise SystemExit('A cleanup manifest already exists; review it before any further cleanup')
entries = []
for result in sorted(root.glob('*/results.json')):
    data = json.loads(result.read_text())
    if 'sqlite_native_autocheckpoint_pages' not in data:
        continue
    assert data['complete']
    for arm in data['arms']:
        path = result.parent / f"{arm['case']}-{arm['arm']}-{arm['n']}"
        if path.is_symlink() or path.resolve().parent != result.parent.resolve():
            raise ValueError(f'Unexpected arm path: {path}')
        if not path.is_dir():
            continue
        files = []
        for p in sorted(path.rglob('*')):
            if p.is_symlink():
                raise ValueError(f'Refusing symlink: {p}')
            if p.is_file():
                stat = p.stat()
                files.append({'path': str(p.relative_to(root)), 'bytes': stat.st_size,
                              'allocated_bytes': getattr(stat, 'st_blocks', 0) * 512})
        entries.append({'directory': str(path.relative_to(root)), 'files': files,
                        'result': str(result.relative_to(root)),
                        'result_sha256': hashlib.sha256(result.read_bytes()).hexdigest(),
                        'removed': False})
manifest = {'complete': False, 'root': str(root), 'entries': entries,
            'logical_bytes': sum(f['bytes'] for e in entries for f in e['files']),
            'allocated_bytes': sum(f['allocated_bytes'] for e in entries for f in e['files']),
            'note': 'Generated arm databases only. Results, event logs, crash evidence, executables and source archives retained. Allocated file blocks are not a claim about APFS volume space recovered.'}
manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
for entry in entries:
    shutil.rmtree(root / entry['directory'])
    entry['removed'] = True
    manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
manifest['complete'] = True
manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps({k: manifest[k] for k in ('complete', 'logical_bytes', 'allocated_bytes')}))
