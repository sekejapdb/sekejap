#!/usr/bin/env python3
"""Check retained lean-gate evidence without needing the disposable databases."""
import json
import re
import sys
from pathlib import Path


def check(root):
    read = lambda p: json.loads((root / p).read_text())
    before, fixed = read('measured/results.json'), read('fixed/results.json')
    assert before['complete'] and fixed['complete']
    assert fixed['sizes'] == [100000, 400000] and not fixed['timestamps']
    summary = {'sizes': [], 'tests': {}, 'crash_cases': []}
    for n in fixed['sizes']:
        e4, sql = [a for a in fixed['arms'] if a['n'] == n]
        original = next(a for a in before['arms'] if a['n'] == n and a['arm'] == 'e4')
        assert e4['arm'] == 'e4' and sql['arm'] == 'sqlite'
        assert len(e4['stages']) == len(sql['stages']) == 13
        for x, y in zip(e4['stages'], sql['stages']):
            assert (x['cycle'], x['stage']) == (y['cycle'], y['stage'])
            assert x['verify']['exact'] and y['verify']['exact']
            assert (x['verify']['rows'], x['verify']['crc32c']) == (y['verify']['rows'], y['verify']['crc32c'])
            if x.get('pinned'):
                for a, stage in [(e4, x), (sql, y)]:
                    assert stage['snapshot_verify']['exact']
                    assert stage['snapshot_verify']['rows'] == n
                    assert stage['snapshot_verify']['crc32c'] == a['stages'][0]['verify']['crc32c']
            if x['cycle'] and x['cycle'] % 2 == 0:
                account = x['accounting']
                assert account['unaccounted_pages'] == 0
                assert account['physical_pages'] == account['meta_pages'] + account['reachable_tree_pages'] + account['freelist_pages']
        for a in [e4, sql]:
            file = 'data' if a['arm'] == 'e4' else 'data.sqlite'
            plateau = [s['disk']['files'][file] for s in a['stages'] if s['cycle'] >= 3 and s['stage'] == 'reinsert']
            assert len(plateau) == 4 and len(set(plateau)) == 1
        old = original['stages'][-1]
        untracked = old['disk']['files']['data'] // 4096 - 2 - old['structure']['reachable_pages'] - old['counters']['free_eligible'] - old['counters']['free_waiting']
        assert untracked == n // 100 * 3 * 3
        repack = read(f'fixed/repack-{n}.json')
        assert not repack['published_over_source']
        for a in ['e4', 'sqlite']:
            assert repack[a]['source_unchanged'] and repack[a]['verify']['exact']
            assert repack[a]['verify']['rows'] == n
            assert repack[a]['verify']['crc32c'] == e4['stages'][-1]['verify']['crc32c']
        assert repack['e4']['known_value_losses'] == repack['e4']['unknown_extents'] == 0
        summary['sizes'].append({
            'rows': n, 'before_untracked_pages': untracked, 'now_untracked_pages': 0,
            'bytes_returned_to_recycling': untracked * 4096,
            'e4_final_bytes': e4['final_disk']['total_bytes'],
            'sqlite_final_bytes': sql['final_disk']['total_bytes'],
            'e4_rebuilt_bytes': repack['e4']['rebuilt_disk']['total_bytes'],
            'sqlite_rebuilt_bytes': repack['sqlite']['rebuilt_disk']['total_bytes'],
            'e4_mutation_seconds': sum(s['seconds'] for s in e4['stages'][1:]),
            'sqlite_mutation_seconds': sum(s['seconds'] for s in sql['stages'][1:]),
            'e4_rebuild_seconds': repack['e4']['seconds'],
            'sqlite_rebuild_seconds': repack['sqlite']['seconds'],
            'final_crc32c': e4['stages'][-1]['verify']['crc32c'],
            'e4_final_accounting': e4['stages'][-1]['accounting'],
        })
    crash = read('crash-retirement/results.json')
    assert crash['complete'] and crash['overflow_retirement'] and len(crash['crashes']) == 2
    for case in crash['crashes']:
        assert case['verify']['exact'] and case['verify']['rows'] == 1000
        summary['crash_cases'].append(case['case'])
    for name, expected in [('final-workspace-tests.log', 296), ('default-overflow-tests.log', 5), ('debug-overflow-tests.log', 5)]:
        log = (root / name).read_text()
        rows = re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored', log)
        assert rows and all(r[0] == 'ok' and int(r[2]) == int(r[3]) == 0 for r in rows)
        passed = sum(int(r[1]) for r in rows)
        assert passed == expected, (name, passed)
        summary['tests'][name] = passed
    summary['passed'] = True
    return summary


if __name__ == '__main__':
    print(json.dumps(check(Path(sys.argv[1])), indent=2))
