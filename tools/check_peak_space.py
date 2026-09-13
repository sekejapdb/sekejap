#!/usr/bin/env python3
"""Check retained peak-space evidence and emit a compact, reproducible summary."""
import json
import re
import sys
from pathlib import Path


def summarise_arm(a, reference=None):
    stages = a['stages']
    assert stages and all(s['verify']['exact'] for s in stages)
    assert all(s['peak']['sample_errors'] == 0 for s in stages)
    for s in stages:
        if s.get('snapshot_verify'):
            assert s['snapshot_verify']['exact']
        if s.get('accounting'):
            assert s['accounting']['unaccounted_pages'] == 0
        assert s['structure'].get('integrity', 'ok') == 'ok'
    # Exclude SQLite's transient SHM from the original, settled reference.
    own_initial = stages[0]['disk']['files'].get('data.sqlite', a['initial_bytes'])
    initial = reference or own_initial
    selected = stages[1:] or stages
    close = a.get('closing_peak', {})
    assert close.get('sample_errors', 0) == 0
    sampled = max(max(s['peak']['sampled_peak_bytes'] for s in selected), close.get('sampled_peak_bytes', 0))
    bound = max(sampled, max(s['checkpoint_upper_bound_bytes'] for s in selected))
    allocated = max(max(s['peak']['sampled_peak_allocated_bytes'] for s in selected), close.get('sampled_peak_allocated_bytes', 0))
    out = dict(case=a['case'], arm=a['arm'], n=a['n'], initial_bytes=initial, run_initial_bytes=own_initial,
               load_peak_bytes=stages[0]['peak']['sampled_peak_bytes'],
               load_seconds=stages[0]['seconds'],
               mutation_seconds=sum(s['seconds'] for s in stages[1:]),
               sampled_peak_bytes=sampled, logical_upper_bound_bytes=bound,
               sampled_peak_allocated_bytes=allocated, allocated_peak_factor=allocated/initial,
               peak_factor=sampled/initial, upper_factor=bound/initial,
               final_bytes=a['final_disk']['total_bytes'],
               twofold_result='fail_observed' if max(sampled, allocated) > 2*initial else ('within_logical_bound_only' if bound <= 2*initial else 'not_proven'),
               current_checks=len(stages), snapshot_checks=sum(bool(s.get('snapshot_verify')) for s in stages),
               current_rows_checked=sum(s['verify']['rows'] for s in stages),
               snapshot_rows_checked=sum(s.get('snapshot_verify', {}).get('rows', 0) if s.get('snapshot_verify') else 0 for s in stages),
               final_counters=stages[-1]['counters'])
    if 'maintenance' in a:
        m = a['maintenance']
        assert m['source_unchanged'] and not m['published_over_source'] and m['verify']['exact']
        assert m['peak']['sample_errors'] == 0
        assert m['verify']['crc32c'] == stages[-1]['verify']['crc32c']
        out['maintenance'] = dict(sampled_peak_bytes=m['peak']['sampled_peak_bytes'],
            sampled_peak_allocated_bytes=m['peak']['sampled_peak_allocated_bytes'],
            rebuilt_bytes=m['rebuilt_disk']['total_bytes'], seconds=m['seconds'],
            peak_over_original=m['peak']['sampled_peak_bytes']/initial,
            peak_over_source=m['peak']['sampled_peak_bytes']/a['final_disk']['total_bytes'])
    return out


def check_run(path, references):
    d = json.loads(path.read_text())
    assert d['complete'], path
    arms = d['arms']
    assert len(arms) % 2 == 0
    for e4, sql in zip(arms[::2], arms[1::2]):
        assert e4['arm'] == 'e4' and sql['arm'] == 'sqlite'
        assert (e4['case'], e4['n']) == (sql['case'], sql['n'])
        assert len(e4['stages']) == len(sql['stages'])
        for a, b in zip(e4['stages'], sql['stages']):
            assert (a['cycle'], a['stage'], a['verify']['crc32c'], a['verify']['rows']) == (b['cycle'], b['stage'], b['verify']['crc32c'], b['verify']['rows'])
    return dict(path=str(path), checkpoint_operations=d.get('checkpoint_operations', 0), e4_publications=d.get('e4_publications_per_checkpoint', 1), arms=[summarise_arm(a, references[(a['n'],a['arm'])]) for a in arms])


def main():
    root = Path(sys.argv[1])
    required = ['before-100k', 'before-400k', 'fixed', 'frequent-load-100k', 'frequent-load-400k', 'frequent-mixed-100k', 'frequent-mixed-400k', 'frequent-updates-100k', 'frequent-updates-400k', 'double-updates-100k', 'double-updates-400k', 'double-frequent-100k', 'double-frequent-400k', 'frequent-long-100k']
    fixed = json.loads((root/'fixed'/'results.json').read_text())
    references = {(a['n'], a['arm']): a['final_disk']['total_bytes'] for a in fixed['arms'] if a['case'] == 'load'}
    assert len(references) == 4
    runs = {name: check_run(root/name/'results.json', references) for name in required}
    assert len(runs['fixed']['arms']) == 24
    red = (root/'reader-release-red.log').read_text()
    assert 'closed reader still pins pages until checkpoint' in red and '1 failed' in red
    tests = (root/'workspace-tests.log').read_text()
    results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', tests)
    assert results and all(f == '0' and i == '0' for _, f, i in results)
    passed = sum(int(p) for p, _, _ in results)
    assert passed >= 297, passed
    policy = (root/'policy-tests.log').read_text()
    assert '2 passed; 0 failed; 0 ignored' in policy
    assert 'repeated_root_publication_reuses_two_trees_and_survives_one_lost_meta ... ok' in policy
    assert 'commit_refreshes_reuse_after_reader_release_without_publishing ... ok' in tests
    for log in ['default-reader-tests.log', 'debug-reader-tests.log']:
        assert '2 passed; 0 failed; 0 ignored' in (root/log).read_text()
    crashes = json.loads((root/'crashes'/'results.json').read_text())
    assert crashes['complete'] and len(crashes['crashes']) == 2
    assert all(c['verify']['exact'] and c['verify']['rows'] == 1000 for c in crashes['crashes'])
    summary = dict(runs=runs, workspace_tests_passed=passed, additional_policy_regressions=1, distinct_tests_passed=passed+1, quota_enforced=False,
                   note='Sampled logical/allocated peaks; separate normal-checkpoint logical bound. Not a hard disk quota or APFS physical-space guarantee.')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
