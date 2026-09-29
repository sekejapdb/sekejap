#!/usr/bin/env python3
"""The 0.19 format gate: a fixed measurement set, run by already-built bench
binaries, and a strict comparison against the 0.18.5 baseline.

    format_gate.py run --rev LABEL --size 50k|1m --out DIR [--reps N]
    format_gate.py compare BASELINE.json CANDIDATE.json
    format_gate.py build-help --rev REV

What is measured, and why, is docs/core/FORMAT_GATE.md (the gate itself is
docs/core/SUPPORTIVE.md section 5). Standard library only. `run` never
builds: it runs the binaries in $CARGO_TARGET_DIR/release (or --bin-dir).
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import struct
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
FEATURES = 'compact-cells,sqlite-balance,keyspace-append,slotref-split'
TOLERANCE = 1.05

# Every program the gate runs. `build-help` builds exactly these and copies
# the gate's versions of their sources into the tree it builds, so the
# baseline and the candidate run the same measurement code.
PROGRAMS = [
    'two_ways', 'q6_budget', 'q3_budget', 'battle50k', 'popsim',
    'phase2_scalar_bench', 'g2_budget', 'graph_write_budget', 'hop1_budget',
    'foundation_space', 'format_gate_cases',
]
OVERLAY = [f'bench/src/bin/{p}.rs' for p in PROGRAMS]

SIZES = {
    # rows, iterations for the per-execution budget programs, narrow rows,
    # table counts for the reopen case. `smoke` proves the wiring only.
    '50k': dict(rows=50_000, iters=100_000, narrow=50_000, tables=(1, 100, 10_000)),
    '1m': dict(rows=1_000_000, iters=20_000, narrow=50_000, tables=(1, 100, 10_000)),
    'smoke': dict(rows=2_000, iters=2_000, narrow=2_000, tables=(1, 10, 100)),
}

# Metric rules. `time` and `cost`: candidate at most TOLERANCE x baseline.
# `pages`: never more than the baseline (no tolerance). `equal`: identical
# (byte digests). `recall`: never lower. `info`: printed, never gated.
RULES = ('time', 'cost', 'pages', 'equal', 'recall', 'info')


# ── small helpers ─────────────────────────────────────────────────────────

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def dir_bytes(path):
    path = Path(path)
    return sum(p.stat().st_size for p in path.iterdir() if p.is_file()) if path.is_dir() else 0


class Case:
    """One case of one repetition: its metrics, units and rules."""

    def __init__(self, name, operation, program, args):
        self.name, self.operation, self.program, self.args = name, operation, program, args
        self.metrics, self.units, self.rules = {}, {}, {}
        self.status, self.note = 'ok', ''

    def put(self, metric, value, unit, rule):
        assert rule in RULES, rule
        self.metrics[metric] = value
        self.units[metric] = unit
        self.rules[metric] = rule


def run_program(bins, program, args, case_dir, log):
    exe = Path(bins) / program
    if not exe.is_file():
        raise FileNotFoundError(f'{program} is not built in the binary directory')
    tmp = case_dir / 'tmp'
    tmp.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, TMPDIR=str(tmp))
    started = time.monotonic()
    proc = subprocess.run([str(exe), *map(str, args)], cwd=case_dir, env=env,
                          capture_output=True, text=True)
    (case_dir / f'{log}.stdout.txt').write_text(proc.stdout)
    (case_dir / f'{log}.stderr.txt').write_text(proc.stderr)
    if proc.returncode != 0:
        tail = proc.stderr.strip().splitlines()[-3:]
        raise RuntimeError(f'{program} exited {proc.returncode}: ' + ' | '.join(tail))
    return proc.stdout, time.monotonic() - started


def number_after(pattern, text, what):
    m = re.search(pattern, text, re.M)
    if not m:
        raise ValueError(f'no {what} in the output')
    return float(m.group(1))


# ── the byte-identity digest ──────────────────────────────────────────────
#
# Every leaf cell of a checkpointed `data` file, decoded the way the kernel
# decodes it (core/kernel/src/page.rs, core/kernel/src/btree.rs
# `validated_leaf`), grouped by the key's first byte -- the keyspace. Each
# group's digest is order-free (a sum of per-cell hashes), so a page split in
# another place does not change it, and an overflow value is hashed by its
# length and checksum, not by the page number it starts at. Free pages are
# stamped free by the page-WAL, so only live cells are counted.

PAGE = 4096
LEAF = 2


def leaf_cells(data):
    size = os.path.getsize(data)
    with open(data, 'rb') as f:
        for no in range(2, size // PAGE):
            f.seek(no * PAGE)
            b = f.read(PAGE)
            if struct.unpack_from('<I', b, 0)[0] != 0x53454B32:
                continue
            kind, _tree, n = struct.unpack_from('<HHH', b, 6)
            if kind != LEAF:
                continue
            for i in range(n):
                off, ln = struct.unpack_from('<HH', b, 40 + 4 * i)
                rec = b[off:off + ln]
                if rec[0] == 0xFF and 0x81 <= rec[1] <= 0x88:
                    end = 2 + (rec[1] - 0x80)
                    yield rec[1:end], rec[end:], False
                elif rec[1] & 0xF0 == 0x40:
                    end = 2 + (struct.unpack_from('<H', rec, 0)[0] & 0x0FFF)
                    yield rec[2:end], rec[end:], False
                else:
                    k = struct.unpack_from('<H', rec, 0)[0]
                    v = struct.unpack_from('<H', rec, 2 + k)[0]
                    if v == 0xFFFF:
                        marker = rec[4 + k:16 + k]
                        # total length and value checksum; not the head page
                        yield rec[2:2 + k], marker[0:4] + marker[8:12], True
                    else:
                        yield rec[2:2 + k], rec[4 + k:4 + k + v], False


def keyspace_digests(data):
    groups = {}
    for key, value, overflow in leaf_cells(data):
        ks = f'{key[0]:02x}' if key else 'empty'
        h = hashlib.blake2b(digest_size=16)
        h.update(len(key).to_bytes(4, 'little'))
        h.update(key)
        h.update(b'\x01' if overflow else b'\x00')
        h.update(value)
        g = groups.setdefault(ks, [0, 0, 0])
        g[0] += 1
        g[1] += len(key) + len(value)
        g[2] = (g[2] + int.from_bytes(h.digest(), 'little')) % (1 << 128)
    return {ks: {'cells': c, 'bytes': n, 'digest': f'{c}:{s:032x}'}
            for ks, (c, n, s) in sorted(groups.items())}


def whole_file(case, bins, db, case_dir):
    """Case 13 on one database: file bytes, page inventory, keyspace digests."""
    out, _ = run_program(bins, 'foundation_space', ['pagewal', db], case_dir, 'foundation_space')
    inv = json.loads(out)
    case.put('data_bytes', os.path.getsize(Path(db) / 'data'), 'bytes', 'cost')
    case.put('dir_bytes', dir_bytes(db), 'bytes', 'cost')
    case.put('physical_pages', inv['physical_pages'], 'pages', 'cost')
    case.put('leaf_records', inv['leaf_records'], 'cells', 'info')
    case.put('leaf_unused_bytes', inv['leaf_unused_bytes'], 'bytes', 'cost')
    for ks, g in keyspace_digests(Path(db) / 'data').items():
        case.put(f'keyspace_{ks}_digest', g['digest'], 'cells:blake2b-sum', 'equal')
        case.put(f'keyspace_{ks}_bytes', g['bytes'], 'bytes', 'info')


# ── the synthetic corpus (battle50k's shape) ──────────────────────────────
#
# bench/tests/battle50k_smoke.rs's generator, re-expressed: same vocabulary,
# kinds, region, xorshift and field rules. A run given --corpus/--queries uses
# those files instead; either way their SHA-256 is recorded and `compare`
# refuses two results measured over different corpora.

VOCAB = ['garden', 'harbour', 'mountain', 'workshop', 'village', 'coffee', 'field',
         'lake', 'market', 'office', 'forest', 'resort']
KINDS = ['depot', 'farm', 'home', 'mill', 'park', 'port', 'school', 'shop']
WEST, EAST, SOUTH, NORTH = 106.90, 107.10, -6.30, -6.10
M64 = (1 << 64) - 1
DIM = 32


def rng(x):
    x ^= (x << 13) & M64
    x ^= x >> 7
    x ^= (x << 17) & M64
    return x


def unit(r):
    return (r % 100_000) / 100_000


def unit_vector(seed):
    r = rng(seed | 1)
    v = []
    for _ in range(DIM):
        v.append(unit(r) - 0.5)
        r = rng(r)
    norm = max(math.sqrt(sum(x * x for x in v)), 1e-6)
    return [round(x / norm, 7) for x in v]


def write_corpus(directory, rows):
    directory.mkdir(parents=True, exist_ok=True)
    data, queries = directory / f'places-{rows}.jsonl', directory / 'queries.json'
    if data.is_file() and queries.is_file():
        return data, queries
    part = data.with_suffix('.partial')
    with open(part, 'w') as out:
        for i in range(rows):
            r = rng(((i + 1) * 0x9E3779B97F4A7C15 & M64) | 1)
            first = VOCAB[r % 12]
            r = rng(r)
            second = VOCAB[r % 12]
            r = rng(r)
            words = []
            for _ in range(8):
                words.append(VOCAB[r % 12])
                r = rng(r)
            lon = WEST + unit(r) * (EAST - WEST)
            r = rng(r)
            lat = SOUTH + unit(r) * (NORTH - SOUTH)
            r = rng(r)
            year = 1940 + r % 80
            r = rng(r)
            month = 1 + r % 12
            r = rng(r)
            day = 1 + r % 28
            d = 0.001
            ring = [[lon - d, lat - d], [lon + d, lat - d], [lon + d, lat + d],
                    [lon - d, lat + d], [lon - d, lat - d]]
            out.write(json.dumps({
                'key': f'p{i:07}', 'name': f'{first} {second}', 'desc': ' '.join(words),
                'born': year * 10_000 + month * 100 + day, 'kind': KINDS[i % 8],
                'loc': {'type': 'Point', 'coordinates': [lon, lat]},
                'plot': {'type': 'Polygon', 'coordinates': [ring]},
                'emb': unit_vector(i + 7),
            }, separators=(',', ':')) + '\n')
    part.rename(data)
    q = {k: [] for k in ('points', 'boxes', 'polygons', 'radii', 'vectors', 'terms')}
    for i in range(50):
        r = rng(((i + 101) * 0xA24BAED4963EE407 & M64) | 1)
        lon = WEST + unit(r) * (EAST - WEST)
        r = rng(r)
        lat = SOUTH + unit(r) * (NORTH - SOUTH)
        q['points'].append([lon, lat])
        q['boxes'].append([lon - 0.02, lon + 0.02, lat - 0.02, lat + 0.02])
        q['polygons'].append({'type': 'Polygon', 'coordinates': [[
            [lon - 0.03, lat - 0.03], [lon + 0.03, lat - 0.02], [lon + 0.02, lat + 0.03],
            [lon - 0.03, lat + 0.02], [lon - 0.03, lat - 0.03]]]})
        q['radii'].append([lon, lat, 2000.0])
        q['vectors'].append(unit_vector(i + 9001))
        q['terms'].append(VOCAB[i % len(VOCAB)])
    queries.write_text(json.dumps(q))
    return data, queries


# ── the cases ─────────────────────────────────────────────────────────────
#
# Each function runs one program in its own fresh directory and fills a Case.
# `ctx` carries the size, the binary directory, the corpus and the databases
# one case leaves for a later one (popsim's file is read by q6_budget).

def c_point_read(ctx, d):
    rows = ctx['rows']
    c = Case('point_read', 'point read by key', 'two_ways',
             [rows, '--only', 'eq_indexed_one', '--pages'])
    out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
    line = next(l for l in out.splitlines() if re.match(r'^filter\s+eq_indexed_one\s', l))
    c.put('wall_us', float(line.split()[2]), 'us per read (median)', 'time')
    c.put('pages', number_after(r'^pages filter/eq_indexed_one (\d+)$', out, 'pages line'),
          'pool accesses per read', 'pages')
    return c


def c_insert_popsim(ctx, d):
    rows = ctx['rows']
    c = Case('insert_popsim', 'insert, commit every 256; late index build (4 indexes)',
             'popsim', ['e4', rows, 'pop', '--only', 'point_lookup', '--reps', 5])
    run_program(ctx['bins'], c.program, c.args, d, c.program)
    rep = json.loads((d / 'pop' / 'popsim-e4.json').read_text())
    st = rep['stages']
    c.put('insert_us_per_row', st['load_s'] * 1e6 / rows, 'us per row', 'time')
    for k in ('index_text_s', 'index_scalar_s', 'index_point_s', 'index_geometry_s'):
        c.put(k[:-2] + '_us_per_row', st[k] * 1e6 / rows, 'us per row', 'time')
    c.put('bytes_on_disk', rep['bytes_on_disk'], 'bytes', 'cost')
    c.put('bytes_per_row', rep['bytes_per_row'], 'bytes per row', 'cost')
    ctx['popsim_db'] = d / 'pop' / 'e4'
    return c


def c_whole_file_popsim(ctx, d):
    c = Case('whole_file_popsim', 'whole file (popsim database)', 'foundation_space',
             ['pagewal', '<insert_popsim>/pop/e4'])
    whole_file(c, ctx['bins'], ctx['popsim_db'], d)
    return c


def c_key_scan(ctx, d):
    c = Case('key_scan', 'key-only scan (popsim database, 8 MiB pool)', 'q6_budget',
             ['<insert_popsim>/pop/e4', '--stage', 'page', '--passes', 2])
    out, _ = run_program(ctx['bins'], c.program,
                         [ctx['popsim_db'], '--stage', 'page', '--passes', 2], d, c.program)
    lines = [l for l in out.splitlines() if l.startswith('page ')]
    m = re.search(r'([\d.]+) ns/row\s+pool/row=\s*([\d.]+) miss/row=\s*([\d.]+)', lines[-1]) if lines else None
    if m is None:
        raise RuntimeError(f"q6_budget printed no 'page' line this harness understands: {lines[-1] if lines else out[-300:]!r}")
    c.put('wall_ns_per_row', float(m.group(1)), 'ns per row (second pass)', 'time')
    c.put('pages_per_row', float(m.group(2)), 'pool accesses per row', 'pages')
    c.put('misses_per_row', float(m.group(3)), 'pool misses per row', 'pages')
    return c


Q3_TIMES = {
    '2a filter/range_open (Ids)': 'range_open_us',
    '5a scan/full_one_col (Fields[cat])': 'full_one_col_us',
    '5b filter/two_ranges (one non-driving field predicate)': 'two_ranges_us',
}
Q3_COUNTS = {
    'filter/range_open': 'range_open',
    'scan/full_one_col': 'full_one_col',
    'filter/two_ranges': 'two_ranges',
}


def c_filtered_scan(ctx, d):
    c = Case('filtered_scan', 'filtered scan with projection', 'q3_budget',
             [ctx['rows'], ctx['iters']])
    out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
    for line in out.splitlines():
        for stage, metric in Q3_TIMES.items():
            if line.startswith(stage):
                c.put(metric, float(line[len(stage):].split()[0]), 'us per execution (median)', 'time')
        parts = line.split()
        if len(parts) >= 7:
            label = ' '.join(parts[:-6])
            if label in Q3_COUNTS:
                n, b, pool, cand, _per, acc = parts[-6:]
                m = Q3_COUNTS[label]
                c.put(f'{m}_allocations', int(n), 'allocations per execution', 'cost')
                c.put(f'{m}_alloc_bytes', int(b), 'bytes allocated per execution', 'cost')
                c.put(f'{m}_pages', int(pool), 'pool accesses per execution', 'pages')
                c.put(f'{m}_pages_per_candidate', float(acc), 'pool accesses per candidate', 'pages')
                c.put(f'{m}_candidates', int(cand), 'candidates', 'info')
    if len(c.metrics) < 3 * 5 + 3:
        raise ValueError('q3_budget output lacks an expected stage')
    return c


def c_churn(ctx, d):
    rows = ctx['rows']
    c = Case('churn', 'insert/update/delete with an index live', 'phase2_scalar_bench',
             ['e4', rows, 'db', '--pages'])
    out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
    rep = json.loads(out)
    ctx['features'] = rep.get('compile_features')
    c.put('insert_us_per_row', rep['load_s'] * 1e6 / rows, 'us per row', 'time')
    c.put('late_index_us_per_row', rep['late_index_s'] * 1e6 / rows, 'us per row', 'time')
    c.put('indexed_bytes', rep['indexed_bytes'][0], 'bytes', 'cost')
    rounds = rep['churn']
    changed = len(range(0, rows, 10))
    for op, per in (('update', rows), ('delete', changed), ('reinsert', changed)):
        total = per * len(rounds)
        c.put(f'{op}_us_per_row', sum(r[f'{op}_s'] for r in rounds) * 1e6 / total, 'us per row', 'time')
        c.put(f'{op}_pages_per_row', sum(r[f'{op}_pages'] for r in rounds) / total,
              'pool accesses per row', 'pages')
    c.put('final_bytes', rep['final_bytes'][0], 'bytes', 'cost')
    c.put('reopen_us', rep['reopen_s'] * 1e6, 'us', 'time')
    return c


BATTLE_CASES = {
    # case name -> (operation, heap measured)
    'text_one': 'text search, one term',
    'text_top10': 'text search, BM25 top 10',
    'vec_exact_10': 'vector search, exact top 10',
    'vec_ann_10@ef100': 'vector search, quantized ANN top 10 (ef 100)',
    'pt_radius': 'spatial, point radius',
    'plot_intersects': 'spatial, polygon intersects',
    'knn_10': 'spatial, nearest 10',
    'agg_count_all': 'aggregate, count(*)',
    'agg_count_kind': 'aggregate, count by kind',
    'agg_count_radius_by_kind': 'aggregate, count by kind within a radius',
}


def c_battle(ctx, d):
    rows = ctx['rows']
    c = Case('battle_load', 'insert, commit every 256; late index build (10 indexes)', 'battle50k',
             ['e4', '--data', '<corpus>', '--queries', '<queries>', '--out', 'battle.json',
              '--db-dir', 'db', '--heap'])
    run_program(ctx['bins'], c.program,
                ['e4', '--data', ctx['corpus'], '--queries', ctx['queries'],
                 '--out', 'battle.json', '--db-dir', 'db', '--heap'], d, c.program)
    rep = json.loads((d / 'battle.json').read_text())
    stages = {s['name']: s for s in rep['stages']}
    c.put('insert_us_per_row', stages['load']['ms'] * 1e3 / rows, 'us per row', 'time')
    for name, s in stages.items():
        if name.startswith('index:') and s.get('ms') is not None:
            c.put(f'{name.replace(":", "_")}_us_per_row', s['ms'] * 1e3 / rows, 'us per row', 'time')
    c.put('disk_bytes', stages['disk_bytes']['bytes'], 'bytes (after every index)', 'cost')
    ctx['battle_db'] = d / 'db'
    results = [c]
    by_name = {x['name']: x for x in rep['cases']}
    for name, operation in BATTLE_CASES.items():
        q = Case(f'battle_{name}', operation, 'battle50k', ['(the battle_load run)'])
        x = by_name.get(name)
        if x is None or x.get('median_us') is None:
            q.status, q.note = 'missing', 'the battle50k report has no measurement for it'
        else:
            q.put('median_us', x['median_us'], 'us per query (median of 50)', 'time')
            q.put('p90_us', x['p90_us'], 'us per query (p90 of 50)', 'info')
            q.put('peak_heap_bytes', x.get('peak_heap_bytes'), 'bytes (live-heap high-water)', 'cost')
            q.put('total_rows', x['total_rows'], 'rows', 'equal')
            if x.get('recall_at_k') is not None:
                q.put('recall_at_10', x['recall_at_k'], 'fraction', 'recall')
        results.append(q)
    return results


def c_whole_file_battle(ctx, d):
    c = Case('whole_file_battle', 'whole file (battle50k database)', 'foundation_space',
             ['pagewal', '<battle_load>/db'])
    whole_file(c, ctx['bins'], ctx['battle_db'], d)
    return c


EDGE_LINE = re.compile(r'^(\S.*?)\s+([\d.]+)\s+([\d.]+)%\s+([\d.]+)$')


def edge_metrics(c, out):
    for line in out.splitlines():
        m = EDGE_LINE.match(line)
        if m:
            stage = re.sub(r'[^a-z0-9]+', '_', m.group(1).lower()).strip('_')
            c.put(f'stage_{stage}_us_per_edge', float(m.group(2)), 'us per edge', 'time')
            c.put(f'stage_{stage}_pages_per_edge', float(m.group(4)), 'pages per edge', 'pages')
    c.put('commit_us_per_edge', number_after(r'^commit\s+([\d.]+)$', out, 'commit line'),
          'us per edge', 'time')
    c.put('pages_per_edge', number_after(r'pool accesses ([\d.]+)/edge', out, 'pool accesses'),
          'pool accesses per edge', 'pages')
    c.put('wal_frames_per_edge', number_after(r'WAL frames ([\d.]+)/edge', out, 'WAL frames'),
          'WAL frames per edge', 'pages')
    c.put('bytes_per_edge', number_after(r'^bytes fixture \d+ after_edges \d+ per_edge ([\d.]+)$',
                                         out, 'bytes line'), 'bytes per edge', 'cost')


def c_edge_single(ctx, d):
    rows = ctx['rows']
    c = Case('edge_single', 'edge write, one put_edge at a time', 'g2_budget',
             [rows, 3 * rows, 'db', '--bytes'])
    out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
    c.put('wall_us_per_edge', number_after(r'= ([\d.]+) us/edge', out, 'wall line'), 'us per edge', 'time')
    edge_metrics(c, out)
    ctx['g2_db'] = d / 'db'
    return c


def c_whole_file_edges(ctx, d):
    c = Case('whole_file_edges', 'whole file (g2_budget database, edges)', 'foundation_space',
             ['pagewal', '<edge_single>/db'])
    whole_file(c, ctx['bins'], ctx['g2_db'], d)
    return c


def edge_corpus_case(ctx, d, many):
    name = 'edge_batched' if many else 'edge_corpus'
    op = ('edge write, batched (link_many), battle corpus' if many
          else 'edge write, one put_edge at a time, battle corpus')
    args = ['<corpus>', 'db', '--rows', ctx['rows'], '--bytes'] + (['--many'] if many else [])
    c = Case(name, op, 'graph_write_budget', args)
    real = [ctx['corpus'], 'db', '--rows', ctx['rows'], '--bytes'] + (['--many'] if many else [])
    out, _ = run_program(ctx['bins'], c.program, real, d, c.program)
    c.put('wall_us_per_edge', number_after(r'^edge writes [\d.]+ s = ([\d.]+) us/edge', out,
                                           'edge writes line'), 'us per edge', 'time')
    edge_metrics(c, out)
    m = re.search(r'^graph_2hop\s+([\d.]+)', out, re.M)
    if m:
        c.put('graph_2hop_us', float(m.group(1)), 'us per walk (median of 50)', 'time')
    return c


def c_edge_corpus(ctx, d):
    return edge_corpus_case(ctx, d, False)


def c_edge_batched(ctx, d):
    return edge_corpus_case(ctx, d, True)


def c_graph_walk(ctx, d):
    c = Case('graph_walk', 'graph walk, 1 and 2 hops', 'hop1_budget',
             [ctx['rows'], ctx['iters'], '--pages'])
    out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
    stage = lambda p: number_after(rf'^{re.escape(p)}.*?\s([\d.]+)\s{{3}}', out, p)
    c.put('hop1_query_us', stage('E  graph1 + Ids'), 'us per query (median)', 'time')
    c.put('hop2_query_us', stage('G  graph2 + Ids'), 'us per query (median)', 'time')
    c.put('hop1_bfs_us', stage('B  traverse_bfs(depth 1)'), 'us per walk (median)', 'time')
    c.put('hop2_bfs_us', number_after(r'^B2 traverse_bfs\(depth 2\)\s+([\d.]+)', out, 'B2'),
          'us per walk (median)', 'time')
    for label, metric in (('B', 'hop1_bfs'), ('B2', 'hop2_bfs'), ('E', 'hop1_query'), ('G', 'hop2_query')):
        c.put(f'{metric}_pages', number_after(rf'^pages {label}\s+(\d+)$', out, f'pages {label}'),
              'pool accesses per execution', 'pages')
    return c


def c_reopen(tables):
    def run(ctx, d):
        c = Case(f'reopen_{tables}', f'reopen at {tables} tables; WAL bytes of one DDL commit',
                 'format_gate_cases', ['reopen', 'db', tables])
        out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
        r = json.loads(out)
        c.put('reopen_us', r['reopen_median_us'], 'us per open + resolve + read (median)', 'time')
        c.put('open_pages', r['open_pages'], 'pool accesses (cold open)', 'pages')
        c.put('resolve_pages', r['resolve_pages'], 'pool accesses (resolve one table)', 'pages')
        c.put('read_pages', r['read_pages'], 'pool accesses (one point read)', 'pages')
        c.put('ddl_commit_wal_bytes', r['ddl_commit_wal_bytes'], 'WAL bytes per DDL commit', 'cost')
        c.put('ddl_commit_wal_frames', r['ddl_commit_wal_frames'], 'WAL frames per DDL commit', 'pages')
        c.put('create_us_per_table', r['create_s'] * 1e6 / tables, 'us per table (DDL + one row)', 'time')
        c.put('bytes_on_disk', r['bytes_on_disk'], 'bytes', 'cost')
        return c
    run.__name__ = f'c_reopen_{tables}'
    return run


def c_narrow(ctx, d):
    rows = ctx['narrow']
    c = Case('narrow', f'narrow table (one integer column) at {rows} rows', 'format_gate_cases',
             ['narrow', 'db', rows])
    out, _ = run_program(ctx['bins'], c.program, c.args, d, c.program)
    r = json.loads(out)
    c.put('insert_us_per_row', r['insert_us_per_row'], 'us per row', 'time')
    c.put('bytes_per_row', r['bytes_per_row'], 'bytes per row', 'cost')
    c.put('point_read_us', r['point_read_us'], 'us per read (mean of the probe run)', 'time')
    c.put('point_read_pages', r['point_read_pages'], 'pool accesses per read', 'pages')
    c.put('scan_ns_per_row', r['scan_ns_per_row'], 'ns per row', 'time')
    c.put('scan_pages_per_row', r['scan_pages_per_row'], 'pool accesses per row', 'pages')
    c.put('ddl_commit_wal_bytes', r['ddl_commit_wal_bytes'], 'WAL bytes per DDL commit', 'cost')
    return c


def cases_for(size):
    # Order matters: whole_file_popsim reads popsim's file before q6_budget
    # opens it; whole_file_* read a database a previous case left behind.
    return ([c_point_read, c_insert_popsim, c_whole_file_popsim, c_key_scan, c_filtered_scan,
             c_churn, c_battle, c_whole_file_battle, c_edge_single, c_whole_file_edges,
             c_edge_corpus, c_edge_batched, c_graph_walk]
            + [c_reopen(t) for t in SIZES[size]['tables']] + [c_narrow])


# ── run ───────────────────────────────────────────────────────────────────

def cmd_run(a):
    bins = Path(a.bin_dir or os.path.join(os.environ.get('CARGO_TARGET_DIR', ''), 'release'))
    if not (bins / 'popsim').is_file():
        sys.exit(f'no bench binaries in {bins}: build them first (format_gate.py build-help)')
    size = SIZES[a.size]
    out = Path(a.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    if a.corpus:
        corpus, queries = Path(a.corpus).resolve(), Path(a.queries).resolve()
    else:
        corpus, queries = write_corpus(out / f'corpus-{size["rows"]}', size['rows'])
    wanted = set(a.only.split(',')) if a.only else None
    base = out / f'{a.rev}-{a.size}'
    if base.exists():
        sys.exit(f'{base.name} already exists under the output directory: use a new --out or --rev')
    reps = []
    features = None
    for rep in range(a.reps):
        ctx = dict(bins=bins, corpus=corpus, queries=queries, **size)
        cases = []
        for fn in cases_for(a.size):
            name = fn.__name__[2:]
            if wanted and name not in wanted:
                continue
            d = base / f'rep{rep + 1}' / name
            d.mkdir(parents=True)
            started = time.monotonic()
            try:
                got = fn(ctx, d)
                got = got if isinstance(got, list) else [got]
            except Exception as e:  # recorded, never hidden
                got = [Case(name, '', '', [])]
                got[0].status, got[0].note = 'failed', f'{type(e).__name__}: {e}'
            for c in got:
                print(f'[rep {rep + 1}] {c.name:<28} {c.status:<8} '
                      f'{time.monotonic() - started:7.1f} s  {c.note}', flush=True)
            cases.extend(got)
        features = features or ctx.get('features')
        reps.append(cases)
        if not a.keep:
            for p in (base / f'rep{rep + 1}').glob('*/*'):
                if p.is_dir() and p.name in ('db', 'pop', 'tmp', 'db-bulk'):
                    shutil.rmtree(p, ignore_errors=True)
    result = {
        'revision': a.rev,
        'size': a.size,
        'rows': size['rows'],
        'features': features,
        'feature_flags_expected': FEATURES,
        'corpus_sha256': sha256_file(corpus),
        'queries_sha256': sha256_file(queries),
        'binaries_sha256': {p: sha256_file(bins / p) for p in PROGRAMS if (bins / p).is_file()},
        'repetitions': a.reps,
        'cases': merge(reps),
    }
    path = out / f'{a.rev}-{a.size}.json'
    path.write_text(json.dumps(result, indent=1))
    print(f'wrote {path.name} in the output directory')
    failed = [c['case'] for c in result['cases'] if c['status'] != 'ok']
    if failed:
        print('not measured: ' + ', '.join(failed))
    return 1 if failed else 0


def merge(reps):
    """One entry per case: the per-repetition values and the gated value."""
    merged = {}
    for cases in reps:
        for c in cases:
            m = merged.setdefault(c.name, {
                'case': c.name, 'operation': c.operation, 'program': c.program,
                'args': [str(x) for x in c.args], 'status': c.status, 'note': c.note,
                'units': c.units, 'rules': c.rules, 'repetitions': []})
            if c.status != 'ok':
                m['status'], m['note'] = c.status, c.note
            m['units'].update(c.units)
            m['rules'].update(c.rules)
            m['repetitions'].append(c.metrics)
    for m in merged.values():
        metrics = {}
        for name, rule in m['rules'].items():
            values = [r[name] for r in m['repetitions'] if name in r and r[name] is not None]
            if not values:
                continue
            if rule == 'equal':
                metrics[name] = values[0] if len(set(values)) == 1 else 'UNSTABLE:' + '|'.join(map(str, values))
            else:
                metrics[name] = statistics.median(values)
        m['metrics'] = metrics
    return list(merged.values())


# ── compare ───────────────────────────────────────────────────────────────

def fmt(v):
    if isinstance(v, float):
        return f'{v:.6g}'
    s = str(v)
    return s if len(s) <= 22 else s[:10] + '..' + s[-10:]


def cmd_compare(a):
    base = json.loads(Path(a.baseline).read_text())
    cand = json.loads(Path(a.candidate).read_text())
    problems = []
    for k in ('size', 'rows', 'corpus_sha256', 'queries_sha256'):
        if base.get(k) != cand.get(k):
            problems.append(f'{k} differs: {base.get(k)} vs {cand.get(k)}')
    allow = set(a.allow_digest.split(',')) if a.allow_digest else set()
    rows, fails, drift = [], 0, []
    cmap = {c['case']: c for c in cand['cases']}
    for b in base['cases']:
        c = cmap.get(b['case'])
        if b['status'] != 'ok' or c is None or c['status'] != 'ok':
            why = 'MISSING' if c is None else f'baseline {b["status"]}, candidate {c["status"]}'
            rows.append((b['case'], '-', '-', '-', '-', f'FAIL ({why})'))
            fails += 1
            continue
        for name, rule in b['rules'].items():
            bv, cv = b['metrics'].get(name), c['metrics'].get(name)
            unit = b['units'].get(name, '')
            if bv is None and cv is None:
                continue
            ratio, verdict = '-', 'PASS'
            if cv is None or bv is None:
                verdict = 'FAIL (missing)'
            elif rule == 'info':
                verdict = 'info'
            elif rule == 'equal':
                ks = name.split('_')[1] if name.startswith('keyspace_') else None
                if bv != cv:
                    verdict = 'REPORTED (allowed)' if ks in allow else 'FAIL (not identical)'
                elif str(bv).startswith('UNSTABLE'):
                    verdict = 'FAIL (unstable)'
            elif rule == 'pages':
                ratio = f'{cv / bv:.4f}' if bv else ('1.0000' if cv == bv else 'inf')
                verdict = 'PASS' if cv <= bv else 'FAIL (more page accesses)'
                if cv < bv:
                    verdict = 'PASS (fewer)'
            elif rule == 'recall':
                ratio = f'{cv / bv:.4f}' if bv else '-'
                verdict = 'PASS' if cv >= bv else 'FAIL (lower recall)'
            else:
                r = cv / bv if bv else (1.0 if cv == bv else math.inf)
                ratio = f'{r:.4f}'
                if r > a.tolerance:
                    verdict = f'FAIL (> {a.tolerance})'
                if rule == 'time' and bv and abs(r - 1) > a.noise:
                    drift.append((b['case'], name, r))
            if verdict.startswith('FAIL'):
                fails += 1
            rows.append((b['case'], f'{name} [{unit}]', fmt(bv), fmt(cv), ratio, verdict))
    for c in cand['cases']:
        if c['case'] not in {b['case'] for b in base['cases']}:
            rows.append((c['case'], '-', '-', '-', '-', 'info (candidate only)'))
    widths = [max(len(str(r[i])) for r in rows + [('case', 'metric', 'baseline', 'candidate', 'ratio', 'verdict')])
              for i in range(6)]
    head = ('case', 'metric', 'baseline', 'candidate', 'ratio', 'verdict')
    print(f'baseline {base["revision"]}  candidate {cand["revision"]}  size {base["size"]}  '
          f'tolerance {a.tolerance}  (wall time: median of {base["repetitions"]} / '
          f'{cand["repetitions"]} repetitions)')
    for p in problems:
        print('FAIL: ' + p)
    print('  '.join(h.ljust(w) for h, w in zip(head, widths)))
    print('  '.join('-' * w for w in widths))
    for r in rows:
        print('  '.join(str(x).ljust(w) for x, w in zip(r, widths)))
    print()
    if drift:
        print(f'{len(drift)} wall-time metric(s) differ by more than {a.noise:.0%} '
              '(against itself: add repetitions):')
        for case, name, r in drift:
            print(f'  {case} {name} {r:.4f}')
    fails += len(problems)
    print(f'{fails} FAIL' if fails else 'gate: PASS')
    return 1 if fails else 0


# ── build-help ────────────────────────────────────────────────────────────

def cmd_build_help(a):
    rev = a.rev
    safe = re.sub(r'[^A-Za-z0-9._-]', '_', rev)
    if a.overlay_rev:
        copy = (f'for f in {" ".join(OVERLAY)}; do\n'
                f'  git show "{a.overlay_rev}:$f" > "$WT/$f"\n'
                f'done')
    else:
        copy = f'cp {" ".join(OVERLAY)} "$WT/bench/src/bin/"'
    bins = ' '.join(f'--bin {p}' for p in PROGRAMS)
    print(f'''# Build the format-gate bench binaries of {rev}, run from the repository
# root of the checkout that holds tools/format_gate.py. SCRATCH and
# GATE_TARGETS are directories OUTSIDE the repository.
F={FEATURES}
REV={rev}
WT="$SCRATCH/gate-wt-{safe}"
export CARGO_TARGET_DIR="$GATE_TARGETS/{safe}"
git worktree add --detach "$WT" "$REV"
# The gate's measurement code, identical for every revision it measures:
{copy}
(cd "$WT" && cargo build --release --locked -p sekejap-bench --features $F {bins})
git worktree remove --force "$WT"
# Then: python3 tools/format_gate.py run --rev {safe} --size 50k --out "$GATE_OUT" \\
#         --bin-dir "$GATE_TARGETS/{safe}/release"''')
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='cmd', required=True)
    r = sub.add_parser('run', help='run every case with already-built binaries')
    r.add_argument('--rev', required=True, help='a label for the revision the binaries were built from')
    r.add_argument('--size', required=True, choices=sorted(SIZES))
    r.add_argument('--out', required=True, help='output directory (outside the repository)')
    r.add_argument('--reps', type=int, default=3)
    r.add_argument('--bin-dir', help='default: $CARGO_TARGET_DIR/release')
    r.add_argument('--corpus', help='battle50k-shaped JSONL; default: generated, deterministic')
    r.add_argument('--queries', help='queries.json for --corpus')
    r.add_argument('--only', help='comma-separated case names (wiring checks only)')
    r.add_argument('--keep', action='store_true', help='keep every database the cases wrote')
    c = sub.add_parser('compare', help='baseline vs candidate; exit 1 on any FAIL')
    c.add_argument('baseline')
    c.add_argument('candidate')
    c.add_argument('--tolerance', type=float, default=TOLERANCE)
    c.add_argument('--noise', type=float, default=0.02,
                   help='list wall-time metrics differing by more than this fraction')
    c.add_argument('--allow-digest', help='comma-separated keyspace bytes (hex, e.g. 01) whose digest '
                   'may change; reported, not failed')
    b = sub.add_parser('build-help', help='print the commands that build one revision')
    b.add_argument('--rev', required=True, help='git revision, e.g. v0.18.5')
    b.add_argument('--overlay-rev', help='take the gate sources from this revision instead of '
                   'the working tree')
    a = p.parse_args()
    if a.cmd == 'run' and bool(a.corpus) != bool(a.queries):
        p.error('--corpus and --queries go together')
    return {'run': cmd_run, 'compare': cmd_compare, 'build-help': cmd_build_help}[a.cmd](a)


if __name__ == '__main__':
    sys.exit(main())
