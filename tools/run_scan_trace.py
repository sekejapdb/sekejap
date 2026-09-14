"""Bounded concurrent independent-store correctness probe, never timing evidence."""
import json, os, subprocess, sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()
assert root == Path('<scratch>')
binary = '/tmp/e4-scan-trace-target/release/deps/law1_heap-6fe3b446bd5d8488'
results = []
for batch in range(2):
    running = []
    for worker in range(4):
        name = f'concurrent-{batch}-{worker}'
        assert not (root / name).exists()
        output = (root / (name + '.log')).open('w')
        env = dict(os.environ, TMPDIR=str(root / 'tmp'), E4_LAW1_ARTIFACTS=str(root / name))
        process = subprocess.Popen([binary, '--nocapture', '--test-threads=1'], env=env, stdout=output, stderr=subprocess.STDOUT)
        running.append((name, process, output))
    for name, process, output in running:
        code = process.wait()
        output.close()
        results.append(dict(name=name, exit_code=code))
        print(name, code, flush=True)
    (root / 'concurrent-results.json').write_text(json.dumps(results, indent=2) + '\n')
    if any(r['exit_code'] for r in results):
        sys.exit(1)
