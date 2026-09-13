#!/usr/bin/env python3
"""Check the CLI against retained independent 10K/40K fixture oracles."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

fixtures, output = map(Path, sys.argv[1:3])
if not output.resolve().is_relative_to('<scratch>'):
    raise SystemExit('recovery artifacts must stay on scratch')
output.mkdir()
binary = Path(__file__).resolve().parents[1] / 'target/release/recover'


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''):
            h.update(b)
    return h.hexdigest()


def source_hashes(path):
    return {name: sha(path / name) for name in ('data', 'wal', 'free') if (path / name).exists()}


def command(*args):
    return json.loads(subprocess.check_output([str(binary), *map(str, args)], text=True))


results = []
for oracle_path in sorted(fixtures.glob('*/result.json')):
    oracle = json.loads(oracle_path.read_text())
    fixture = oracle_path.parent
    source = fixture / 'source'
    destination = output / fixture.name
    before = source_hashes(source)
    report = command('schema', source, destination)
    for field in ('layouts', 'raw_records', 'decoded_records', 'missing_layout_records', 'damaged_pages'):
        assert report[field] == oracle[field], (fixture.name, field, report)
    for name in ('records.raw', 'decoded.jsonl'):
        assert sha(destination / name) == sha(fixture / 'recovery' / name), (fixture.name, name)
    assert source_hashes(source) == before
    verified = command('verify', destination)
    assert verified['verified'] and verified['membership'] == 'candidate'
    results.append({'case': fixture.name, 'input_rows': oracle['input_rows'], 'report': report,
                    'verified': verified, 'source_sha256': before,
                    'fixture_known_lost_rows': oracle['physical_page_losses_known_by_fixture']})
    print(json.dumps({'case': fixture.name, 'decoded': report['decoded_records'],
                      'raw': report['raw_records'], 'seconds': report['elapsed_seconds']}), flush=True)
assert len(results) == 8
# Verify catches a corrupt export frame, before accepting it as usable evidence.
victim = output / results[0]['case'] / 'records.raw'
with victim.open('r+b') as f:
    f.seek(32)
    original = f.read(1)
    try:
        f.seek(32); f.write(bytes([original[0] ^ 1])); f.flush()
        failed = subprocess.run([str(binary), 'verify', str(victim.parent)], text=True, capture_output=True)
        assert failed.returncode != 0 and 'checksum' in failed.stderr, failed
    finally:
        f.seek(32); f.write(original); f.flush()
assert command('verify', victim.parent)['verified']
(output / 'results.json').write_text(json.dumps({'cases': results, 'corrupt_export_rejected': True}, indent=2) + '\n')
