# P1: compact entry storage, 2026-09-09

The entry-only convergence loop reached **1.0017–1.0631× SQLite's complete
post-checkpoint file size**, below the working 1.10× target in all 12 cases.
Three repetitions per case produced identical sizes; all 36 runs passed
reopen, exhaustive point-read equality, canonical full-output checksum equality,
and structural/integrity verification. This is a storage-density result at
10K/40K, not the full multimodel or 48M gate.

## What SQLite taught us

Source downloaded to `<scratch>`, pinned at
`f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9` (2026-09-04), from
[SQLite's repository](https://github.com/sqlite/sqlite).
The measured SQLite library is rusqlite's bundled **3.46.0**; the source-study
checkout is newer. These are distinct and deliberately identified.

- [vdbeaux.c](https://github.com/sqlite/sqlite/blob/f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9/src/vdbeaux.c#L3864): serial-type metadata separates compact typed bodies from their widths.
- [vdbe.c](https://github.com/sqlite/sqlite/blob/f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9/src/vdbe.c#L3577): MakeRecord assembles a header followed by positional values.
- [btree.c, integer leaf cells](https://github.com/sqlite/sqlite/blob/f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9/src/btree.c#L1290): compact integer identity and payload framing.
- [btree.c, balance_quick](https://github.com/sqlite/sqlite/blob/f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9/src/btree.c#L8039): dense rightmost append behavior.
- [btree.c, balance_nonroot](https://github.com/sqlite/sqlite/blob/f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9/src/btree.c#L8240): redistribution across a bounded sibling neighborhood.

E4 adapts these techniques to its existing checked, copy-on-write B+tree;
it does not implement SQLite's file format or replace the pager/WAL.
The earlier README claim that the entire file-size gap was encoding alone
was incomplete: catalog placement and split occupancy also mattered.

## Scope and fairness

Each shared JSONL fixture contains stable integer IDs, external text `_key`,
name, two integers, and a spatial point. `json` adds nested JSON with Unicode,
arrays, nulls, booleans, fractional numbers and undeclared fields including
u64::MAX. `large_json` additionally gives 1% of rows a roughly 6.8KB string,
forcing real overflow chains.

Both arms use an integer primary key. The external text key is a stored
column, without a uniqueness index in either arm. This entry-first experiment
has no edges, vectors, or secondary indexes in either arm, following the
user's revised scope. The older [P0 mixed experiment](PROTOTYPE_RESULTS.md)
still measures vectors, graph edges, and indexes; its ratios are not directly
comparable with P1. SQLite stores native scalar columns and JSONB; E4 stores
typed records and binary JSON. Points are two f64 in both.

All databases, shared fixtures, WALs, temporary files and result JSON live on
the same scratch volume. Same 4096-byte pages, 8MiB engine page-cache budget,
mmap disabled, full durability settings, commit every 1000 entries, checkpoint
at completion. No VACUUM, whole-database sorting or compaction in either arm.
Sizes sum every database-directory file after closing; fixture files are not
counted as database storage. Peak ingest footprint is not measured in P1.
Caches do not include OS cache, codecs, or statement allocations. Timings are
warm-system measurements, not cold-cache claims; engine order alternates
between repetitions. Ordered and arithmetic-permutation shuffled inputs
contain the same rows.

## Final measurements

Time ratios are median E4 time / median SQLite time across three runs.
`Output` means scan, decode and serialize every complete document to the same
canonical JSON representation. Lower is better.

| Rows | Shape | Order | E4 bytes | SQLite bytes | Size | Insert | Output |
|---|---|---|---:|---:|---:|---:|---:|
| 10,000 | scalar | ordered | 622,636 | 602,112 | 1.0341× | 1.097× | 1.230× |
| 10,000 | scalar | shuffled | 696,364 | 655,360 | 1.0626× | 1.381× | 1.045× |
| 10,000 | json | ordered | 1,789,996 | 1,716,224 | 1.0430× | 1.047× | 1.070× |
| 10,000 | json | shuffled | 2,019,372 | 1,912,832 | 1.0557× | 1.208× | 1.182× |
| 10,000 | large_json | ordered | 2,588,716 | 2,465,792 | 1.0499× | 1.076× | 1.051× |
| 10,000 | large_json | shuffled | 2,830,380 | 2,662,400 | 1.0631× | 1.108× | 1.071× |
| 40,000 | scalar | ordered | 2,416,684 | 2,412,544 | 1.0017× | 1.174× | 1.459× |
| 40,000 | scalar | shuffled | 2,719,788 | 2,686,976 | 1.0122× | 1.093× | 1.344× |
| 40,000 | json | ordered | 7,090,220 | 6,852,608 | 1.0347× | 1.060× | 1.120× |
| 40,000 | json | shuffled | 8,011,820 | 7,630,848 | 1.0499× | 0.844× | 1.330× |
| 40,000 | large_json | ordered | 10,268,716 | 9,859,072 | 1.0415× | 0.987× | 1.244× |
| 40,000 | large_json | shuffled | 11,239,468 | 10,616,832 | 1.0586× | 0.779× | 1.007× |

Scalar record payload is 50 bytes; at 40K ordered inserts the complete E4
store is 60.42 bytes/entry versus SQLite's 60.31. Names and fixture values
differ from the earlier README's 57-byte estimate; compare identical data.

Insert ratios range from 0.779× to 1.381×. Full-output ratios range from
1.007× to 1.459×: **the README's 1.2× output gate is not yet met everywhere**.
The dense codec currently reconstructs intermediate codec buffers on reads;
a direct accessor/decoder is still needed. Neighbor balancing also buys
space with extra bounded page copies and writes. Size convergence does not
establish query-speed convergence.

## Ablation trail

These are cumulative experimental checkpoints, not independent factorial
ablations. Each row gives the 40K size ratios, ordered / shuffled. Exact
fixtures, per-case results and databases remain under the listed run suffix
in `<scratch>/`.

| Change | Scalar | JSON | Large JSON | Run |
|---|---:|---:|---:|---|
| P0 entry codec and inherited packing | 2.548 / 1.567 | 2.158 / 1.403 | 1.820 / 1.301 | entry-s0-1788950692 |
| Catalog sorts before entries, enabling dense append | 1.280 / 1.567 | 1.153 / 1.427 | 1.106 / 1.314 | entry-s1-1788950722 |
| Variable-width integer ID | 1.190 / 1.572 | 1.098 / 1.346 | 1.088 / 1.261 | entry-s2-1788950750 |
| Tighter ID, point type, optional states/extras | 1.080 / 1.532 | 1.050 / 1.305 | 1.057 / 1.238 | entry-s3-1788951178 |
| Redistribute across two existing siblings | 1.080 / 1.133 | 1.050 / 1.098 | 1.057 / 1.104 | entry-s3-1788951260 |
| Three existing leaves + packed layout flags | 1.061 / 1.066 | 1.050 / 1.097 | 1.055 / 1.094 | entry-s4-1788951633 |
| Pack integer-width directory | 1.046 / 1.053 | 1.050 / 1.088 | 1.055 / 1.085 | entry-s5-1788951778 |
| Allow three leaves to grow into four | 1.046 / 1.079 | 1.050 / 1.086 | 1.055 / 1.078 | entry-s5-1788951900 |
| Compact integer-key cell framing | 1.002 / 1.012 | 1.035 / 1.050 | 1.042 / 1.059 | entry-s5-1788952353 |

The three-to-four change alone did not improve every case; it is documented
rather than attributed a universal win. The final compact framing brought
all 10K and 40K cases below 1.10×.

## Reproduce and inspect

From the E4 checkout:

```sh
cargo run --release --offline --features sqlite-balance,compact-cells --bin entry -- 5 3
```

This creates a fresh timestamped scratch run. `stage` selects record/key
ablations 0–5; features select final neighbor balancing and compact cells.
Intermediate two-sibling algorithm revisions are historical measurements,
not retained selectable algorithms. The final implementation is retained.

Final evidence: `<scratch>`.
Test log: same directory, `test-results.log`.
See [ENTRY_STORAGE.md](ENTRY_STORAGE.md) for the concrete record/cell format,
changes, recovery checks and remaining graph work.

## Validation completed

- Full workspace with `sqlite-balance,compact-cells`: **267 passed, 0 failed,
  0 ignored**, including inherited graph/vector/spatial/text kernel tests.
- Default-feature prototype, kernel unit tests and bulk tests: **156 passed,
  0 failed, 1 intentionally ignored**. The ignored test is the new shuffled
  density target, which the old balancing policy fails; it passes with the
  candidate enabled. This second run is targeted, not another full workspace
  integration run. Log: `default-test-results.log` beside the main log.
- Root prototype formatting check and documentation links pass. E3's working
  tree remains clean. Existing inherited dead-code/unused-variable warnings
  remain; no release or commit was made.

The overflow-recovery regression was first reproduced with the experimental
features disabled, then fixed and verified in both configurations. A separate
corrupt-chain test confirms recovery leaves the source file byte-for-byte
unchanged on refusal.
