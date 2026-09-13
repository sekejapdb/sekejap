"""Summarize fixed-work profiles without conflating correctness and Law 2.

Usage: python3 tools/report_foundation_scale.py MAC_ROOT PI_REPORT_ROOT OUTPUT_JSON
The second root can contain downloaded results.json files, without databases.
"""
import hashlib
import json
import math
from pathlib import Path
import statistics
import sys


def median(values):
    return statistics.median(values)


def summarize(base):
    profiles = {}
    for name in ['scale', 'large']:
        raw = (base / name / 'results.json').read_bytes()
        d = json.loads(raw)
        assert d['status'] == 'PASS' and not d['failures']
        expected = 36 if name == 'scale' else 4
        assert len(d['records']) == expected
        profiles[name] = dict(sha256=hashlib.sha256(raw).hexdigest(), result=d)
    records = [v for d in profiles.values() for v in d['result']['records']]
    rows = []
    for n in [10000, 100000, 1000000, 10000000]:
        for locality in ['local', 'scattered']:
            arms = {}
            for engine in ['pagewal', 'sqlite']:
                chosen = [v['report'] for v in records if v['report']['rows'] == n
                    and v['report']['engine'] == engine and v['report']['locality'] == locality]
                assert len(chosen) == (1 if n == 10000000 else 3)
                assert all(v['changes_per_phase'] == 1000 and v['verification']['rows'] == n for v in chosen)
                operations = {}
                for i, op in enumerate(['insert', 'update', 'delete']):
                    samples = [v['phases'][i] for v in chosen]
                    assert all(v['operation'] == op and v['changes'] == 1000 for v in samples)
                    operations[op] = dict(seconds=median([v['seconds'] for v in samples]),
                        all_seconds=[v['seconds'] for v in samples],
                        commit_seconds=median([v['commit_seconds'] for v in samples]),
                        checkpoint_seconds=median([v['checkpoint_seconds'] for v in samples]))
                    if engine == 'pagewal':
                        for key in ['total_read_calls', 'total_write_calls', 'issued_write_bytes']:
                            operations[op][key] = median([v['io'][key] for v in samples])
                            operations[op]['all_' + key] = [v['io'][key] for v in samples]
                    else:
                        operations[op]['cache_events_only'] = {key: median([v['io'][key] for v in samples])
                            for key in ['cache_hits', 'cache_misses', 'cache_writes']}
                arms[engine] = dict(repetitions=len(chosen), operations=operations,
                    load_seconds=median([v['load']['seconds'] for v in chosen]),
                    loaded_logical=median([v['load']['disk']['logical'] for v in chosen]),
                    final_logical=median([v['final_disk']['logical'] for v in chosen]),
                    final_allocated=median([v['final_disk']['allocated'] for v in chosen]))
            ratios = {op: arms['pagewal']['operations'][op]['seconds'] / arms['sqlite']['operations'][op]['seconds']
                for op in ['insert', 'update', 'delete']}
            size_ratio = arms['pagewal']['final_logical'] / arms['sqlite']['final_logical']
            rows.append(dict(rows=n, locality=locality, arms=arms, sqlite_time_ratios=ratios,
                sqlite_time_targets={op: ('PASS' if ratio < 1.5 else 'FAIL') for op, ratio in ratios.items()},
                sqlite_final_size_ratio=size_ratio, sqlite_size_target='PASS' if size_ratio <= 1.1 else 'FAIL',
                acceptance_repetitions='PENDING' if n == 10000000 else 'PASS'))
    growth = []
    for locality in ['local', 'scattered']:
        low = next(v for v in rows if v['rows'] == 10000 and v['locality'] == locality)
        for n in [100000, 1000000, 10000000]:
            high = next(v for v in rows if v['rows'] == n and v['locality'] == locality)
            for engine in ['pagewal', 'sqlite']:
                for op in ['insert', 'update', 'delete']:
                    before, after = [v['arms'][engine]['operations'][op] for v in [low, high]]
                    ratio = after['seconds'] / before['seconds']
                    item = dict(locality=locality, engine=engine, operation=op, from_rows=10000, to_rows=n,
                        time_ratio=ratio, descriptive_time_exponent=math.log(ratio)/math.log(n/10000),
                        all_high_times_exceed_all_low_times=min(after['all_seconds']) > max(before['all_seconds']),
                        repeated_evidence=n != 10000000)
                    if engine == 'pagewal':
                        item['io_growth'] = {}
                        for key in ['total_read_calls', 'total_write_calls', 'issued_write_bytes']:
                            factor = after[key] / before[key]
                            item['io_growth'][key] = dict(ratio=factor, descriptive_exponent=math.log(factor)/math.log(n/10000))
                    growth.append(item)
    # No tolerance is invented after observing the data. A separated repeated
    # latency increase falsifies the strict flat-latency claim for this workload.
    observed = [v for v in growth if v['engine'] == 'pagewal' and v['repeated_evidence']
        and v['all_high_times_exceed_all_low_times']]
    return dict(profiles=profiles, rows=rows, growth=growth,
        law2_status='FAIL' if observed else 'PENDING',
        law2_reason='Repeated measured latency growth; no whole-database scan or complexity class inferred'
            if observed else 'No separated repeated growth detected; finite measurements do not prove strict flat latency',
        separated_growth_cases=observed)


def main():
    mac, pi, output = map(Path, sys.argv[1:])
    result = dict(version='F1-fixed-work-1', platforms={name: summarize(base) for name, base in [('Mac', mac), ('Pi', pi)]},
        production_promotable=False, contract='Exactly seven laws; unchanged',
        limits=['Raw KV only; no collection/index parity claim',
            '10M is single-run confirmation; smaller ladder has three repetitions',
            'Empty engine caches each phase; OS caches not flushed',
            'E4 FileIo requests and SQLite cache events are different counters',
            'Phase-boundary logical/allocated sizes; no peak or enforced-cap claim',
            'Exponents describe finite measurements; they are not asymptotic-complexity proofs'])
    for profile in ['scale', 'large']:
        m = result['platforms']['Mac']['profiles'][profile]['result']
        p = result['platforms']['Pi']['profiles'][profile]['result']
        core = [k for k in m['source_sha256'] if k.startswith(('src/', 'kernel/src/')) or k in ['Cargo.toml', 'Cargo.lock', 'kernel/Cargo.toml', 'CONTRACT.md']]
        assert all(m['source_sha256'][k] == p['source_sha256'][k] for k in core), 'platform source mismatch'
        left = {(v['repetition'],v['report']['rows'],v['report']['locality'],v['report']['engine']):v['report']['verification'] for v in m['records']}
        right = {(v['repetition'],v['report']['rows'],v['report']['locality'],v['report']['engine']):v['report']['verification'] for v in p['records']}
        assert left == right, 'platform oracle mismatch'
    output.write_text(json.dumps(result, indent=2) + '\n')
    for name, p in result['platforms'].items():
        print(name, 'L2', p['law2_status'])
        for r in p['rows']:
            print(r['rows'], r['locality'],
                [[round(r['arms'][e]['operations'][op]['seconds']*1000,2) for op in ['insert','update','delete']] for e in ['pagewal','sqlite']],
                'size', round(r['sqlite_final_size_ratio'],4))


if __name__ == '__main__':
    main()
