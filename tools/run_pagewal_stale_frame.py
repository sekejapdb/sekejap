"""Three rotated raw-KV comparisons of frozen v2, guarded v3 and SQLite."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

base = Path(sys.argv[1]).resolve()
assert str(base) in (
    '<scratch>',
    '<scratch>',
)
pi = str(base).startswith('<home>/')
folder = base / 'comparison'
folder.mkdir()
(base / 'tmp').mkdir(exist_ok=True)
env = dict(os.environ, TMPDIR=str(base / 'tmp'), SQLITE_TMPDIR=str(base / 'tmp'))
records = []
failures = []
for rep in range(3):
    arms = ['pagewal-v2', 'pagewal-v3', 'sqlite']
    arms = arms[rep:] + arms[:rep]
    pair = []
    for arm in arms:
        out = folder / ('r' + str(rep + 1)) / arm
        out.parent.mkdir(parents=True, exist_ok=True)
        assert not out.exists()
        binary = base / ('pagewal-v2' if arm == 'pagewal-v2' else 'pagewal-v3')
        engine = 'sqlite' if arm == 'sqlite' else 'pagewal'
        command = [str(binary), str(out), engine, '400000', '256', '12', 'mixed', '1000']
        if pi:
            command = ['prlimit', '--as=134217728', '--'] + command
        with (out.parent / (arm + '.log')).open('w') as log:
            code = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT).returncode
        if code:
            failures.append(dict(repetition=rep + 1, arm=arm, code=code, path=str(out)))
            print('FAILED', rep + 1, arm, code, flush=True)
        else:
            report_path = out / 'report.json'
            raw = report_path.read_bytes()
            report = json.loads(raw)
            assert report['rows'] == 400000 and len(report['phases']) == 13
            assert report['reopen_verification'] == report['phases'][-1]['verification']
            assert all(p['peak']['errors'] == 0 and p['verification']['rows'] == 400000 for p in report['phases'])
            assert all(p['create_update_delete_counts'] == [40000, 80000, 40000] for p in report['phases'][1:])
            pair.append(report)
            records.append(dict(repetition=rep + 1, arm=arm, path=str(report_path),
                sha256=hashlib.sha256(raw).hexdigest(), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                address_space_limit=134217728 if pi else None, report=report))
            print('completed', rep + 1, arm, flush=True)
        (base / 'comparison-results.json').write_text(json.dumps(dict(records=records, failures=failures), indent=2) + '\n')
    for phase in range(13):
        assert len({r['phases'][phase]['verification']['crc32c'] for r in pair}) <= 1
assert not failures and len(records) == 9
