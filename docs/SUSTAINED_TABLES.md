# Sustained benchmark tables

MB means decimal megabytes. Times are medians across the stated repetitions; peaks are the largest sampled value across those repetitions. Peaks include all arm files and close/reopen. Requested sampling interval: 1 ms. These are observations, not enforced limits.

Load uses 1,000-operation transactions and automatic checkpoints. Every later churn run starts with the same fresh load. Rows contain typed scalars, a point, binary JSON and schemaless extras; no timestamps, vectors, graph edges or indexes.

## Churn

| Variant / case / rows | Pairs | E4 time s (range) | SQLite time s (range) | E4 / SQLite logical peak MB | E4 / SQLite allocated peak MB | E4 / SQLite final MB |
|---|---:|---:|---:|---:|---:|---:|
| baseline/mixed_long/100000 | 3 | 16.48 (16.20–19.04) | 40.35 (39.04–44.11) | 190.93 / 464.84 | 207.84 / 470.12 | 186.17 / 32.90 |
| baseline/mixed_none/100000 | 3 | 16.84 (15.72–19.26) | 41.03 (39.62–47.15) | 90.86 / 40.47 | 106.97 / 42.28 | 86.19 / 32.90 |
| baseline/updates/100000 | 3 | 9.80 (9.70–10.45) | 24.93 (24.48–25.71) | 60.79 / 25.75 | 61.85 / 26.61 | 56.54 / 17.75 |
| candidate/mixed_long/100000 | 3 | 51.94 (51.75–68.82) | 37.17 (36.13–41.71) | 69.16 / 464.84 | 85.60 / 470.69 | 68.45 / 32.90 |
| candidate/mixed_none/100000 | 3 | 62.04 (47.13–69.80) | 44.62 (40.45–49.77) | 49.68 / 40.47 | 50.63 / 41.70 | 49.03 / 32.90 |
| candidate/updates/100000 | 3 | 21.58 (21.54–22.58) | 25.08 (24.88–28.96) | 35.59 / 25.75 | 36.34 / 26.61 | 35.18 / 17.75 |
| direct/mixed_long/100000 | 3 | 43.76 (43.46–44.14) | 33.08 (32.68–33.24) | 69.16 / 464.84 | 85.60 / 470.34 | 68.45 / 32.90 |
| direct/mixed_none/100000 | 3 | 40.86 (40.82–41.34) | 36.68 (36.60–36.76) | 49.68 / 40.47 | 50.74 / 41.73 | 49.03 / 32.90 |
| direct/updates/100000 | 3 | 19.47 (19.46–19.48) | 23.57 (23.46–23.57) | 35.59 / 25.75 | 35.79 / 25.78 | 35.18 / 17.75 |
| matrix/delete_reinsert/100000 | 1 | 13.34 (13.34–13.34) | 15.53 (15.53–15.53) | 36.38 / 25.75 | 37.21 / 25.78 | 35.96 / 17.52 |
| matrix/mixed_short/100000 | 1 | 43.48 (43.48–43.48) | 14.30 (14.30–14.30) | 71.39 / 2162.72 | 85.61 / 2168.25 | 70.68 / 32.90 |
| matrix/mixed_none/400000 | 1 | 201.74 (201.74–201.74) | 181.06 (181.06–181.06) | 161.27 / 141.02 | 168.85 / 142.72 | 160.51 / 132.77 |
| matrix/mixed_short_gap/100000 | 1 | 44.02 (44.02–44.02) | 15.18 (15.18–15.18) | 71.39 / 294.32 | 85.61 / 302.24 | 70.68 / 32.90 |

## Fresh load, before any update or delete

Separate from the earlier bulk/phase-end ingest reports: the 4 MiB WAL-or-page checkpoint experiment runs during this load, retaining reusable CoW pages.

| Variant / rows | Paired loads | E4 / SQLite load s | E4 / SQLite logical peak MB | E4 / SQLite retained MB at load end |
|---|---:|---:|---:|---:|
| baseline / 100000 | 9 | 2.42 / 5.43 | 52.17 / 25.75 | 50.68 / 17.55 |
| candidate / 100000 | 9 | 6.58 / 5.40 | 29.62 / 25.75 | 29.41 / 17.55 |
| direct / 100000 | 9 | 5.86 / 5.38 | 29.62 / 25.75 | 29.41 / 17.55 |
| matrix / 100000 | 3 | 5.86 / 5.38 | 29.62 / 25.75 | 29.41 / 17.55 |
| matrix / 400000 | 1 | 37.86 / 32.64 | 87.50 / 79.10 | 87.28 / 71.00 |

## Observed expansion from each engine’s post-load footprint

The denominator is that engine’s retained size after the same automatic-checkpoint load, already including reusable pages. It is not compact live payload size. Factors are measured maxima, not safety guarantees; allocated filesystem blocks can exceed logical lengths.

| Variant / case / rows | E4 / SQLite peak factor | E4 / SQLite growth between final two even cycle ends, bytes |
|---|---:|---:|
| direct/mixed_long/100000 | 2.35× / 26.48× | 0 / 0 |
| direct/mixed_none/100000 | 1.69× / 2.31× | 0 / 0 |
| direct/updates/100000 | 1.21× / 1.47× | 0 / 0 |
| matrix/delete_reinsert/100000 | 1.24× / 1.47× | 0 / 0 |
| matrix/mixed_short/100000 | 2.43× / 123.22× | 0 / 339,430,600 |
| matrix/mixed_none/400000 | 1.85× / 1.99× | 0 / 0 |
| matrix/mixed_short_gap/100000 | 2.43× / 16.77× | 0 / 0 |

## Full typed reads and reopen

Warm OS cache after verification, fixed engine cache. Point reads materialize 3,000 complete rows. Scan materializes all ordered rows including ID, without oracle generation or checksum serialization. SQLite converts JSONB through json() and the Rust consumer parses it into the same Value type. These are consumer-visible typed reads, not raw-page or cold-volume throughput.

| Variant / case / rows | E4 / SQLite point ms | E4 / SQLite scan ms | E4 / SQLite reopen ms |
|---|---:|---:|---:|
| direct/mixed_long/100000 | 10.13 / 15.07 | 168.36 / 216.19 | 1.28 / 0.65 |
| direct/mixed_none/100000 | 9.99 / 14.96 | 166.74 / 218.78 | 0.82 / 0.65 |
| direct/updates/100000 | 9.39 / 13.74 | 163.50 / 213.27 | 0.83 / 0.65 |
| matrix/delete_reinsert/100000 | 9.20 / 13.65 | 165.03 / 216.67 | 1.17 / 0.58 |
| matrix/mixed_short/100000 | 10.02 / 15.76 | 168.71 / 218.59 | 1.24 / 0.72 |
| matrix/mixed_none/400000 | 10.76 / 15.32 | 664.94 / 862.95 | 1.50 / 0.55 |
| matrix/mixed_short_gap/100000 | 10.21 / 15.10 | 165.95 / 218.48 | 1.26 / 0.62 |

## Evidence roots

- `<scratch>`
- `<scratch>`
- `<scratch>`
- `<scratch>`

See [interpretation, policy differences, validation and limits](SUSTAINED_FOUNDATION.md). The machine restarted between candidate and direct runs; before/after wall-time differences are not wholly attributable to the codec. Each table retains a contemporaneous SQLite counterpart.
