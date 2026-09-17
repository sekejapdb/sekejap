#!/usr/bin/env python3
"""Generate candidate scalar/graph fixtures and verify cross-build compatibility.

This does not freeze or release the format. All DB work runs in a new Linux
artifact directory; no Phase1 corpus is opened for writing.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from format_reference_compat import digest, inventory, new_artifact_path


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--default-bin', type=Path, required=True)
    p.add_argument('--retained-bin', type=Path, required=True)
    p.add_argument('--work', type=Path, required=True)
    args = p.parse_args()
    if sys.platform != 'linux':
        raise SystemExit('Run database fixture qualification on the authorized Linux host')
    bins = {k: v.resolve(strict=True) for k, v in
            [('default', args.default_bin), ('retained', args.retained_bin)]}
    work = new_artifact_path(args.work)
    work.mkdir(parents=True)
    corpus = work / 'corpus'
    corpus.mkdir()
    report = {'result': 'RUNNING', 'status': 'candidate-not-released-or-frozen',
              'binaries': {k: {'path': str(v), 'sha256': digest(v)} for k, v in bins.items()},
              'fixtures': [], 'arms': [], 'commands': []}

    def run(binary, *argv, expected=0):
        cmd = [str(binary), *map(str, argv)]
        r = subprocess.run(cmd, text=True, capture_output=True)
        report['commands'].append({'argv': cmd, 'exit_code': r.returncode,
                                   'stdout': r.stdout, 'stderr': r.stderr})
        if expected == 0:
            assert r.returncode == 0, f'{cmd}\n{r.stdout}\n{r.stderr}'
        else:
            assert r.returncode != 0, f'negative path guard admitted {cmd}'
        return r

    try:
        for mode, binary in bins.items():
            version = json.loads(run(binary, '--version').stdout)
            report['binaries'][mode]['version'] = version
        for mode, generator in bins.items():
            other = bins['retained' if mode == 'default' else 'default']
            for initial in ['checkpointed', 'wal-pending']:
                name = f'{mode}-{initial}'
                source = corpus / name
                run(generator, 'generate', source, initial)
                before = inventory(source)
                run(other, 'verify', source, 'original')
                assert inventory(source) == before, 'cross-build read changed source fixture'
                manifest = source / 'PHASE2_FIXTURE.json'
                # Same path and nested aliases cannot be used as copied targets.
                refusal = run(other, 'upgrade', manifest, source, 'wal-pending', '--confirm-copy', expected=1)
                assert 'refusing to mutate source fixture' in refusal.stderr
                assert inventory(source) == before
                # A copy with a hardlinked primary file must fail before mutation.
                hard = work / f'{name}-hardlink-negative'
                shutil.copytree(source, hard)
                regular = next(f for f in source.iterdir() if f.is_file() and f.name != 'PHASE2_FIXTURE.json')
                (hard / regular.name).unlink()
                (hard / regular.name).hardlink_to(regular)
                refusal = run(other, 'upgrade', manifest, hard, 'wal-pending', '--confirm-copy', expected=1)
                assert 'multiple hard links' in refusal.stderr
                assert inventory(source) == before, 'hardlink refusal changed source fixture'
                (hard / regular.name).unlink()
                shutil.copyfile(regular, hard / regular.name)
                report['fixtures'].append({'name': name, 'generator': mode, 'files': before})
                for handoff in ['checkpointed', 'wal-pending']:
                    target = work / f'{name}-upgrade-{handoff}'
                    shutil.copytree(source, target)
                    run(other, 'upgrade', manifest, target, handoff, '--confirm-copy')
                    target_before = inventory(target)
                    # Original generator is first typed reader after the other writer closes.
                    run(generator, 'verify', target, 'updated')
                    assert inventory(target) == target_before, 'rollback reader modified upgraded files'
                    assert inventory(source) == before, 'upgrade changed source fixture'
                    arm = {'fixture': name, 'handoff': handoff, 'result': 'PASS',
                           'source_unchanged': True, 'upgraded_files': target_before}
                    report['arms'].append(arm)
                    print(json.dumps({k: v for k, v in arm.items() if k != 'upgraded_files'}), flush=True)
        assert len(report['arms']) == 8
        for mode, binary in bins.items():
            assert digest(binary) == report['binaries'][mode]['sha256']
        report['result'] = 'PASS'
    finally:
        if report['result'] == 'RUNNING':
            report['result'] = 'FAIL'
        (work / 'REPORT.json').write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')
    print(json.dumps({'result': report['result'], 'arms': len(report['arms']),
                      'report': str(work / 'REPORT.json')}))


if __name__ == '__main__':
    main()
