"""One command for lean regressions, repeated fixed-work scaling, or 10M confirmation.

Writes fixtures only to authorized artifact roots. A successful run means its
tests/oracles passed, not that L2 latency or the eight-law release gate passed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
# The authorized artifact area: SEKEJAP_BENCH_ROOT, else the system temp dir.
BENCH_ROOT = Path(os.environ.get('SEKEJAP_BENCH_ROOT', tempfile.gettempdir())).resolve()


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('profile', choices=['lean', 'scale', 'large'])
    p.add_argument('output', type=Path)
    p.add_argument('--binary', type=Path)
    p.add_argument('--limit-address-space', action='store_true',
                   help='run each scale binary under prlimit --as=128MiB (Linux)')
    a = p.parse_args()
    out = a.output.resolve()
    if not (out.is_relative_to(BENCH_ROOT) and out != BENCH_ROOT):
        p.error('output must be a new subdirectory of the authorized artifact area')
    out.mkdir(parents=True, exist_ok=False)
    tmp = out / 'tmp'; tmp.mkdir()
    env = dict(os.environ, TMPDIR=str(tmp), SQLITE_TMPDIR=str(tmp))
    limited = a.limit_address_space
    groups = json.loads((ROOT / 'docs/FOUNDATION_LEAN_GROUPS.json').read_text())
    source = {}
    for folder in ['core/engine/src', 'core/engine/tests', 'core/kernel/src', 'core/kernel/tests',
                   'lang/src', 'lang/tests', 'dist/src', 'bench/src', 'bench/tests', 'tools']:
        for path in sorted((ROOT / folder).rglob('*')):
            if path.is_file() and path.suffix in ['.rs', '.py', '.sh']:
                source[str(path.relative_to(ROOT))] = sha(path)
    for name in ['Cargo.toml', 'Cargo.lock', 'core/kernel/Cargo.toml', 'core/engine/Cargo.toml',
                 'lang/Cargo.toml', 'dist/Cargo.toml', 'bench/Cargo.toml',
                 'CONTRACT.md', 'docs/FOUNDATION_LEAN_GROUPS.json']:
        source[name] = sha(ROOT / name)
    report = dict(version='F1-lean-1', profile=a.profile, platform=platform.platform(),
        machine=platform.machine(), source_sha256=source, groups=groups, commands=[], records=[],
        failures=[], status='RUNNING', qualification='NO promotion or flat-latency claim')

    def save():
        (out / 'results.json').write_text(json.dumps(report, indent=2) + '\n')

    def run(label, command):
        started = time.monotonic()
        with (out / (label + '.log')).open('w') as log:
            result = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
        report['commands'].append(dict(label=label, command=command, code=result.returncode,
            seconds=time.monotonic()-started, log_sha256=sha(out / (label + '.log'))))
        save()
        if result.returncode:
            report['failures'].append(label); report['status']='FAIL'; save()
            raise SystemExit(f'{label} failed; evidence preserved in {out}')

    save()
    if a.profile == 'lean':
        for label, args in groups['commands'].items():
            run('tests-' + label, ['cargo', 'test', '--release', '--offline', '--features',
                'sqlite-balance,compact-cells', *args, '--', '--test-threads=1'])
            print('passed test group', label, flush=True)
    binary = a.binary.resolve() if a.binary else Path(env.get('CARGO_TARGET_DIR', str(ROOT/'target'))) / 'release/foundation_scale'
    if not a.binary:
        run('build', ['cargo', 'build', '--release', '--offline', '-p', 'sekejap-bench', '--features',
            'sqlite-balance,compact-cells', '--bin', 'foundation_scale'])
    report['binary_sha256'] = sha(binary)
    sizes, repetitions, changes = {
        'lean': ([1000], 1, 100),
        'scale': ([10000, 100000, 1000000], 3, 1000),
        'large': ([10000000], 1, 1000),
    }[a.profile]
    for rep in range(repetitions):
        rotated = sizes[rep:] + sizes[:rep]
        for n in rotated:
            for locality in ['local', 'scattered']:
                pair = []
                for engine in (['pagewal', 'sqlite'] if rep % 2 == 0 else ['sqlite', 'pagewal']):
                    label = f'r{rep+1}-{n}-{locality}-{engine}'
                    dest = out / label
                    command = [str(binary), str(dest), engine, str(n), locality, str(changes)]
                    if limited:
                        command = ['prlimit', '--as=134217728', '--', *command]
                    run(label, command)
                    result = json.loads((dest/'report.json').read_text())
                    assert result['rows']==n and result['verification']['rows']==n
                    assert result['changes_per_phase']==changes and len(result['phases'])==3
                    assert all(v['changes']==changes and v['changed_point_checks']==changes for v in result['phases'])
                    pair.append(result['verification'])
                    report['records'].append(dict(repetition=rep+1, path=str(dest/'report.json'),
                        report_sha256=sha(dest/'report.json'), address_space_limit=134217728 if limited else None, report=result))
                    save()
                    print('completed', label, flush=True)
                assert pair[0] == pair[1], 'E4/SQLite oracle mismatch'
    assert len(report['records']) == len(sizes)*repetitions*4
    report['status'] = 'PASS'; save()
    print('Profile complete; correctness passed. Scaling verdict requires the recorded measurements.', flush=True)


if __name__ == '__main__':
    main()
