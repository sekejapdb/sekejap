#!/usr/bin/env python3
"""Prove selected recovery guards are tested; restore source after every run."""
from pathlib import Path
import json
import os
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
output = Path(sys.argv[1]).resolve()
if not output.is_relative_to('<scratch>'):
    raise SystemExit('mutation artifacts must stay on scratch')
output.mkdir()
cases = [
    ('descriptor-crc', 'src/lib.rs',
     '|| crc32c::crc32c(&b[..2077]) != u32::from_le_bytes(b[2077..].try_into()?)',
     '|| false', 'schema_recovery', 'schema_descriptor_checksum_is_independent_of_page_checksum'),
    ('conflicting-id', 'src/recovery.rs',
     'if self.path(id, "conflict").try_exists()? {', 'if false {',
     'schema_recovery', 'schema_conflicting_ids_never_pick_a_winner'),
    ('overflow-value-crc', 'kernel/src/recover/reader.rs',
     'if seen != bound || bytes != total || crc != want {',
     'if seen != bound || bytes != total {',
     'recovery_faults', 'overflow_crossed_chain_is_one_known_loss'),
]
results = []
env = dict(os.environ, TMPDIR='<scratch>')
env.pop('E4_SCHEMA_ARTIFACTS', None)
for name, filename, before, after, target, test in cases:
    path = root / filename
    original = path.read_text()
    if original.count(before) != 1:
        raise RuntimeError(f'{name}: guard changed; inspect rather than guessing')
    changed = original.replace(before, after)
    (output / (name + '.original')).write_text(original)
    try:
        path.write_text(changed)
        run = subprocess.run(['cargo', 'test', '--release', '--offline', '--features',
                              'sqlite-balance,compact-cells', '--test', target, test, '--', '--exact'],
                             cwd=root, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        (output / (name + '.log')).write_text(run.stdout)
        killed = run.returncode != 0 and 'test result: FAILED' in run.stdout and f'test {test} ... FAILED' in run.stdout
        results.append({'mutation': name, 'test': test, 'detected_at_runtime': killed})
        if not killed:
            raise RuntimeError(f'{name}: mutation survived or did not reach runtime')
    finally:
        if path.read_text() != changed:
            raise RuntimeError(f'{filename} changed concurrently; original retained in {output}')
        path.write_text(original)
    print(json.dumps(results[-1]), flush=True)
(output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
