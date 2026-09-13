#!/usr/bin/env python3
"""Read-only progress for the isolated Pi matrix."""
import json
import pathlib
import time

root = pathlib.Path('<scratch>')
matrix = root / 'matrix-address-space'
started = sorted(matrix.glob('*.started'), key=lambda p: p.stat().st_mtime)
done = list(matrix.glob('*.completed'))
report = {'completed_pairs': len(done), 'matrix_complete': (matrix / 'complete').exists()}
if started:
    newest = started[-1]
    report['latest_pair'] = newest.stem
    report['pair_elapsed_seconds'] = round(time.time() - newest.stat().st_mtime)
    log = matrix / (newest.stem + '.log')
    if log.exists():
        for line in reversed(log.read_text(errors='replace').splitlines()):
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if isinstance(row, dict) and 'stage' in row:
                report['last_stage'] = {k: row.get(k) for k in ('arm', 'cycle', 'stage', 'seconds', 'operations')}
                break
controller = root / 'resume-controller.log'
if not controller.exists():
    controller = root / 'final-controller.log'
report['controller_tail'] = controller.read_text(errors='replace')[-1000:]
report['temperature_c'] = int(pathlib.Path('/sys/class/thermal/thermal_zone0/temp').read_text()) / 1000
print(json.dumps(report))
