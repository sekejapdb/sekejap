"""Remove only audited disposable databases, retaining all experiment evidence."""
import hashlib
import json
import shutil
from pathlib import Path

root = Path(__file__).resolve().parents[1]
base = Path('<scratch>')
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
decision = json.loads((base / 'decision.json').read_text())
assert decision['decision'].startswith('rejected:')
assert sha(Path(decision['prototype']['archive'])) == decision['prototype']['archive_sha256']
results = json.loads((root / 'docs/PAGE_EDIT_RESULTS.json').read_text())
assert results['arms'] == 20 and results['five_way_state_comparisons'] == 24
manifest_path = base / 'cleanup.json'
assert not manifest_path.exists(), 'Cleanup already planned or complete; inspect first'
retained, deleted = [], []

def item(report, expected_sha=None):
    if expected_sha:
        assert sha(report) == expected_sha
    folder = report.with_suffix('')
    assert folder.is_dir() and not folder.is_symlink()
    assert folder.resolve() == folder and base in folder.parents
    files = []
    for p in sorted(folder.rglob('*')):
        assert not p.is_symlink(), str(p)
        if p.is_file():
            s = p.stat()
            files.append(dict(path=str(p.relative_to(folder)), logical=s.st_size,
                              allocated=s.st_blocks * 512))
    assert files
    return dict(path=str(folder), report=str(report), report_sha256=sha(report),
                files=files, logical=sum(f['logical'] for f in files),
                allocated=sum(f['allocated'] for f in files))

for report in results['reports']:
    entry = item(Path(report['evidence']), report['sha256'])
    keep = report['label'] == 'mixed-100000-r1' and report['arm'] in ('baseline', 'cell', 'sqlite')
    (retained if keep else deleted).append(entry)

profile = base / 'profile-baseline/e4-400000-off.json'
d = json.loads(profile.read_text())
assert d['rows'] == 400000 and d['cycles'] == 2 and len(d['phases']) == 3
for key in ('rows', 'crc32c'):
    assert d['reopen_verification'][key] == d['phases'][-1]['verification'][key]
assert all(p['verification']['rows'] == 400000 for p in d['phases'])
deleted.append(item(profile))
assert len(retained) == 3 and len(deleted) == 18
manifest = dict(status='planned', retained=retained, deleted=deleted,
                removed_logical_bytes=sum(p['logical'] for p in deleted),
                removed_allocated_bytes=sum(p['allocated'] for p in deleted))
manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
for entry in deleted:
    assert item(Path(entry['report']), entry['report_sha256']) == entry
    shutil.rmtree(entry['path'])
for entry in retained:
    assert Path(entry['path']).is_dir()
for entry in deleted:
    assert not Path(entry['path']).exists()
    assert sha(Path(entry['report'])) == entry['report_sha256']
manifest['status'] = 'complete'
manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
(root / 'docs/PAGE_EDIT_CLEANUP.json').write_text(manifest_path.read_text())
with (root / 'docs/PAGE_EDIT_EXPERIMENT.md').open('a') as f:
    f.write('\n## Evidence retention and cleanup\n\n'
            'Rejected prototypes and binaries, raw reports, logs, profile and hashes\n'
            'remain under `<scratch>/`. The first\n'
            'mixed run retains accepted E4, cell-only prototype and SQLite databases.\n'
            'The other 17 timing databases and the diagnostic profile database were\n'
            'deleted after report and source-archive verification: '
            f"{manifest['removed_allocated_bytes']:,} allocated bytes removed "
            '(file block accounting; filesystem free-space changes can differ).\n'
            'See [cleanup manifest](PAGE_EDIT_CLEANUP.json).\n')
for name in ('PAGE_EDIT_EXPERIMENT.md', 'PAGE_EDIT_RESULTS.json',
             'PAGE_EDIT_PROVENANCE.json', 'PAGE_EDIT_CLEANUP.json'):
    shutil.copy2(root / 'docs' / name, base / name)
for name in ('profile_mutations.py', 'run_page_edit_experiment.sh',
             'record_page_edit_experiment.py', 'stage_page_edit_experiment.py',
             'reject_page_edit_experiment.py', 'cleanup_page_edit_experiment.py'):
    shutil.copy2(root / 'tools' / name, base / name)
print(json.dumps({k: v for k, v in manifest.items() if k not in ('retained', 'deleted')}))
