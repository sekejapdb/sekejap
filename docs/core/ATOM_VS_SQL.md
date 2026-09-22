# ATOM vs SQL — what the SQL surface costs on the same engine

Round 6 of the `battle50k` loop, 50,000 rows, run 2026-09-23 on a quiet
machine at commit `c933e5d`. Both arms are the SAME engine on the SAME
corpus: `e4` asks through the embedded `Database` API, `e4-sql` asks the same
questions in SQL through `prepare_query`. The Postgres and SQLite columns are
carried over from earlier rounds and are not re-measured here.

Reproduce with:

```
battle50k e4     --data places-50000.jsonl --queries queries.json --out r6-e4.json     --db-dir <dir> --graph
battle50k e4-sql --data places-50000.jsonl --queries queries.json --out r6-e4-sql.json --db-dir <dir> --graph
battle50k compare r6-e4.json r6-e4-sql.json postgres-loop2.json v2-sqlite.json
```

## The answer

The SQL surface is not a second engine and does not behave like one.

| measure | value |
| --- | --- |
| cases compared | 43 |
| disagreements | 0 |
| disk, atomic | 64,608,560 bytes |
| disk, SQL | 64,608,560 bytes |
| median latency ratio, SQL over atomic | 1.23x |
| median ABSOLUTE overhead | 60.3 us |

Both arms write the same bytes: the disk figures are identical, not close.
Every case agrees on row count, and every ordered case agrees on the key
sequence, so no ratio here is a speed win standing on a correctness loss.

## Read the ratio, then read the microseconds

The 1.23x median is the wrong number to carry away, because the ratio is
largest exactly where the absolute cost is smallest.

| case | atomic us | SQL us | ratio | delta us |
| --- | ---: | ---: | ---: | ---: |
| `agg_distinct_kind` | 10.9 | 70.3 | 6.47x | 59.4 |
| `plot_contains_pt` | 15.9 | 76.2 | 4.80x | 60.3 |
| `agg_count_radius_by_kind` | 121.9 | 339.1 | 2.78x | 217.2 |
| `graph_1hop_weight_top10` | 11.7 | 31.8 | 2.72x | 20.1 |
| `bool_not_kind` | 5248.8 | 5255.6 | 1.00x | 6.8 |
| `bool_not_null_born` | 5181.8 | 5178.8 | 1.00x | -3.0 |
| `bool_exists_related` | 24033.5 | 23807.9 | 0.99x | -225.6 |

The worst RATIO in the table is a 59 microsecond query. The cases where SQL
looks expensive are the cases that were already free, and what SQL adds to
them is a roughly constant compile: the vector cases report it directly as
`parse` ≈ 114 us. Once a query does real work, the surface disappears into
the noise, and on the three largest cases the SQL arm is level with or
faster than the atomic one.

## Where SQL genuinely costs something

Writes, and one read.

| case | atomic us | SQL us | delta us |
| --- | ---: | ---: | ---: |
| `vec_bulk_write_1k` | 12382.2 | 16643.2 | 4261.0 |
| `vec_bulk_write_1k_noindex` | 4848.4 | 9060.3 | 4211.9 |
| `vec_exact_10` | 6939.4 | 9584.3 | 2644.9 |

The two write cases are 1,000 rows each, so the 4.2 ms is about 4 us of
per-statement cost per row — the same compile constant, paid a thousand
times instead of once. A caller loading in bulk should reach for the atomic
API or keep the statement prepared; a caller running queries should not
think about this at all.

## Approximate vector search

Both arms sweep `ef` and both reach recall 1.000 at every point, so the
sweep is a latency comparison and nothing else.

| point | atomic median us | SQL median us |
| --- | ---: | ---: |
| ef20 | 2399.8 | 2492.4 |
| ef50 | 2496.8 | 2555.1 |
| ef100 | 2625.2 | 2687.6 |
| ef200 | 2859.3 | 2947.9 |
| ef400 | 3359.4 | 3418.2 |

For context from the carried-over arms: Postgres never reaches recall 0.95
on this corpus — its best point is 0.788 at 3610.4 us — and SQLite has no
approximate family at all, so its only honest point is a 14790.0 us scan.
