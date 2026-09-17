#!/usr/bin/env python3
"""Qualify a same-copy current-write / preserved-write / current-read cycle.

This gate never generates fixtures. Cross-build mode prepares the first baseline;
cross-revision mode requires distinct recorded engine revisions. Both require
actual preserved executables and unchanged-feature handoffs on guarded copies.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from format_reference_compat import digest, inventory
from phase2_preserved_compat import (
    BASELINE_LABELS, BOUNDARIES, checked_wal_bytes, parse_baselines,
    parse_sha256, require, validate_corpus, validate_report, validate_work_path,
)


def validate_versions(baselines, comparisons, qualification):
    baseline_revisions = {v.get('engine_revision') for v in baselines.values()}
    comparison_revisions = {v.get('engine_revision') for v in comparisons.values()}
    require(len(baseline_revisions) == len(comparison_revisions) == 1,
            'each build group must represent one engine revision')
    require((baseline_revisions == comparison_revisions) == (qualification == 'cross-build'),
            'engine revisions do not match the declared qualification kind')
    for label in BASELINE_LABELS:
        for version in (baselines[label], comparisons[label]):
            require(version.get('rollback_cycle_version') == 1, 'helper lacks rollback cycle v1')
            revision = version.get('engine_revision')
            require(isinstance(revision, str) and revision and revision != 'unrecorded',
                    'helper lacks engine provenance')
        other = 'retained' if label == 'default' else 'default'
        require(baselines[label]['harness'] == comparisons[other]['harness'],
                'comparison helper protocol differs from the preserved helper')
        for field in ('create_compact_cells', 'compile_features'):
            require(field in baselines[label] and field in comparisons[label],
                    f'helper lacks {field} profile')
            require(baselines[label][field] == comparisons[label][field],
                    f'comparison {label} profile differs: {field}')


def main():
    require(sys.flags.optimize == 0, 'optimized Python is forbidden for qualification')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qualification-report', type=Path, required=True)
    parser.add_argument('--report-sha256', type=parse_sha256, required=True)
    parser.add_argument('--corpus', type=Path, required=True)
    parser.add_argument('--baseline-bin', nargs=2, action='append', required=True)
    parser.add_argument('--comparison-bin', nargs=2, action='append', required=True)
    parser.add_argument('--qualification-kind', choices=('cross-build', 'cross-revision'), required=True)
    parser.add_argument('--comparison-build', nargs=2, metavar=('MANIFEST', 'SHA256'),
                        help='required for cross-revision: independently pinned build/source provenance')
    parser.add_argument('--work', type=Path, required=True)
    args = parser.parse_args()
    require(sys.platform == 'linux', 'run database qualification on the authorized Linux host')
    report_path = args.qualification_report.resolve(strict=True)
    corpus = args.corpus.resolve(strict=True)
    baselines = parse_baselines(args.baseline_bin)
    comparisons = parse_baselines(args.comparison_bin)
    require(digest(report_path) == args.report_sha256, 'qualification report hash differs')
    specification = validate_report(json.loads(report_path.read_text()))
    source_before = validate_corpus(corpus, specification)
    hashes = {'baseline': {k: digest(p) for k, p in baselines.items()},
              'comparison': {k: digest(p) for k, p in comparisons.items()}}
    require(all(len(set(group.values())) == 2 for group in hashes.values()),
            'default and retained executables must be distinct within each build group')
    build_path = None
    build = None
    if args.qualification_kind == 'cross-build':
        require(args.comparison_build is None, 'cross-build reuses the report-pinned binaries')
        require(hashes['comparison'] == hashes['baseline'],
                'cross-build comparison must use the exact preserved binaries')
    else:
        require(args.comparison_build is not None, 'cross-revision needs pinned build provenance')
        build_path = Path(args.comparison_build[0]).resolve(strict=True)
        build_hash = parse_sha256(args.comparison_build[1])
        require(digest(build_path) == build_hash, 'comparison build manifest hash differs')
        build = json.loads(build_path.read_text())
        require(build.get('format') == 'phase2-rollback-build-v1', 'unknown build provenance format')
        sources = build.get('source_files_sha256')
        require(isinstance(sources, dict) and sources and 'Cargo.lock' in sources,
                'build manifest lacks captured source hashes')
        for value in sources.values():
            parse_sha256(value)
        require(set(build.get('binaries', {})) == set(BASELINE_LABELS), 'incomplete build binary map')
        for label in BASELINE_LABELS:
            require(build['binaries'][label]['sha256'] == hashes['comparison'][label],
                    'comparison executable differs from independent build capture')
    for label in BASELINE_LABELS:
        require(hashes['baseline'][label] == specification['recorded_binaries'][label]['sha256'],
                'preserved binary differs from pinned report')
        other = 'retained' if label == 'default' else 'default'
        require(hashes['baseline'][label] != hashes['comparison'][other],
                'cycle would use an identical writer executable')
    protected = {'report': report_path, 'corpus': corpus}
    if build_path is not None:
        protected['comparison build provenance'] = build_path
    protected.update({f'baseline-{k}': p for k, p in baselines.items()})
    protected.update({f'comparison-{k}': p for k, p in comparisons.items()})
    work = validate_work_path(args.work, protected)
    work.mkdir(parents=True)
    report = {
        'format': 'phase2-rollback-compat-report-v1', 'result': 'RUNNING',
        'qualification_kind': args.qualification_kind,
        'claim': 'same copied database: comparison writes, preserved writer continues, comparison verifies and reopens writer',
        'fixture_generation_invoked': False, 'qualification_report': str(report_path),
        'qualification_report_sha256': args.report_sha256, 'source_inventory': source_before,
        'binary_sha256': hashes, 'commands': [], 'arms': [],
        'comparison_build': build,
        'comparison_build_sha256': args.comparison_build[1] if args.comparison_build else None,
    }

    def run(purpose, binary, *arguments):
        argv = [str(binary), *map(str, arguments)]
        result = subprocess.run(argv, text=True, capture_output=True)
        report['commands'].append({'purpose': purpose, 'argv': argv, 'exit_code': result.returncode,
                                   'stdout': result.stdout, 'stderr': result.stderr})
        require(result.returncode == 0, f'{purpose} failed: {result.stdout}\n{result.stderr}')
        return result.stdout

    try:
        versions = {}
        for group, binaries in (('baseline', baselines), ('comparison', comparisons)):
            versions[group] = {label: json.loads(run(f'version:{group}:{label}', path, '--version'))
                               for label, path in binaries.items()}
        report['binary_versions'] = versions
        for label in BASELINE_LABELS:
            require(versions['baseline'][label] == specification['recorded_binaries'][label]['version'],
                    'preserved version differs from pinned report')
            if build is not None:
                require(versions['comparison'][label] == build['binaries'][label]['version'],
                        'comparison version differs from independent build capture')
        validate_versions(versions['baseline'], versions['comparison'], args.qualification_kind)
        for name, fixture in sorted(specification['fixtures'].items()):
            source = corpus / name
            manifest = source / specification['manifest']
            old_label = fixture['generator']
            current_label = 'retained' if old_label == 'default' else 'default'
            old, current = baselines[old_label], comparisons[current_label]
            for first_boundary in BOUNDARIES:
                for second_boundary in BOUNDARIES:
                    target = work / f'{name}--{first_boundary}--{second_boundary}'
                    shutil.copytree(source, target)
                    run(f'current-write:{name}', current, 'upgrade', manifest, target,
                        first_boundary, '--confirm-copy')
                    first_wal = checked_wal_bytes(target, first_boundary)
                    updated_bytes = inventory(target)
                    run(f'old-read-updated:{name}', old, 'verify', target, 'updated')
                    require(inventory(target) == updated_bytes, 'old snapshot reader changed copy')
                    # Crucial distinction from pairwise replay: no new copy is made here.
                    run(f'old-write-current-bytes:{name}', old, 'continue-upgrade', manifest,
                        target, second_boundary, '--confirm-copy')
                    second_wal = checked_wal_bytes(target, second_boundary)
                    roundtrip_bytes = inventory(target)
                    run(f'current-read-roundtrip:{name}', current, 'verify', target, 'roundtrip')
                    require(inventory(target) == roundtrip_bytes, 'current snapshot reader changed copy')
                    run(f'current-writer-reopen:{name}', current, 'verify-writer', manifest,
                        target, 'roundtrip', '--confirm-copy')
                    reopened_bytes = inventory(target)
                    run(f'old-read-reopened:{name}', old, 'verify', target, 'roundtrip')
                    require(inventory(target) == reopened_bytes, 'old snapshot reader changed reopened copy')
                    require(inventory(source) == fixture['files'], 'cycle changed preserved source')
                    arm = {'fixture': name, 'source_generator': old_label,
                           'comparison': current_label, 'first_handoff': first_boundary,
                           'second_handoff': second_boundary, 'first_wal_bytes': first_wal,
                           'second_wal_bytes': second_wal, 'result': 'PASS', 'source_unchanged': True,
                           'final_copy_inventory': inventory(target)}
                    report['arms'].append(arm)
                    (work/'REPORT.json').write_text(json.dumps(report, indent=2, sort_keys=True)+'\n')
                    print(json.dumps({k: v for k, v in arm.items() if k != 'final_copy_inventory'}), flush=True)
        require(len(report['arms']) == len(specification['fixtures']) * 4, 'incomplete boundary matrix')
        report['result'] = 'PASS'
    finally:
        unchanged = digest(report_path) == args.report_sha256 and inventory(corpus) == source_before
        if build_path is not None:
            unchanged = unchanged and digest(build_path) == args.comparison_build[1]
        for group, binaries in (('baseline', baselines), ('comparison', comparisons)):
            unchanged = unchanged and all(digest(path) == hashes[group][label] for label, path in binaries.items())
        report['protected_unchanged'] = unchanged
        if not unchanged or report['result'] == 'RUNNING':
            report['result'] = 'FAIL'
        (work/'REPORT.json').write_text(json.dumps(report, indent=2, sort_keys=True)+'\n')
        require(unchanged, 'preserved report, source or executable changed')


if __name__ == '__main__':
    main()
