# E4 scalar benchmark review

## Result

`src/bin/phase2_scalar_bench.rs` now supports a fresh-process late-index policy argument:

- `e4 ...` or `e4 ... resumable`: bounded 256-row build steps, one durable commit per step (the existing resumable measurement).
- `e4 ... atomic`: the same bounded 256-row method loop, then one final durable commit.
- `sqlite ...` or `sqlite ... atomic`: one atomic `CREATE INDEX`; `sqlite ... resumable` is refused because SQLite has no equivalent policy here.

Every report now states `late_index_mode`, the publication policy, build batch, build-step count, commit count, a stage-specific sampled disk peak, fresh/no-drop setup, E4 create-time compact-cell selection, and booleans for `compact-cells`, `sqlite-balance`, `keyspace-append`, and `slotref-split`. The default invocation is backward compatible for both engines.

The r5 100K late-index numbers are not a fair build-speed ratio: E4's 3.161 s includes 391 FULL durable publications while SQLite's 0.0527 s includes one. Retain the former as a resumability-cost result. Compare SQLite with the new E4 `atomic` arm for equivalent publication count, while still reporting the different internal build-step counts.

## Correctness and runtime audit

The scalar oracle is independent of both databases: it derives expected external keys from `age(i, round)`. It previously sorted actual keys and therefore proved membership but not index ordering. It now compares exact age/entity-ID order, including the later IDs assigned to delete/reinsert rows; SQLite's untimed churn oracle query now requests the same order. At N <= 1M, the widest probe returns 37,500 rows, below the explicit 65,536 limit, so the exact comparison is not truncated.

E4 is configured with an 8 MiB budget, buffered page-WAL, and `SyncMode::Full`. SQLite requests WAL, `synchronous=FULL`, and `cache_size=-8192`; the harness now reads these PRAGMAs back and aborts unless they are WAL/2/-8192, then reports them and the SQLite version. SQLite's cache PRAGMA limits its page cache rather than all process memory, so “8 MiB” is not an identical total-memory cap. The two FULL modes are each engine's durability policy, not proof of identical syscall sequences.

The r5 artifacts were produced by the default build and do not characterize the retained compact/balance shape. Run and label both default and retained feature configurations. `e4_create_compact_cells` reports the actual format selected for a fresh database; existing databases retain their header-selected encoding regardless of the binary's feature defaults.

## Remaining fairness/evidence limits

- Disk sampling is discrete and remains a lower bound. It misses sub-commit peaks, and SQLite sorter/temp files may be created outside the benchmark directory. The new late-index peak is isolated from any prior index/drop and includes the post-checkpoint retained state, but cannot eliminate those limits.
- r5 is one run per arm. The workload contract still requires at least three alternating repetitions with medians and spread; these results are not Phase 2 acceptance evidence.
- SQLite's reopened connection does not reapply/report the 8 MiB cache setting, and its reopen timing includes a `sqlite_schema` query while E4's does not. Treat reopen ratios as non-equivalent until aligned.
- Automatic checkpoint behavior can enter timed commits differently in the two engines. Commit batch counts match for the stated 10K/100K/1M cases, but the engines' checkpoint triggers are not an identical policy.
- The benchmark is a scalar lifecycle slice only. It does not satisfy the full multimodel fixture, query, reader, recovery, compatibility, or acceptance matrix.

## Validation

- `rustfmt src/bin/phase2_scalar_bench.rs`
- `cargo check --bin phase2_scalar_bench`
- `cargo check --bin phase2_scalar_bench --features compact-cells,sqlite-balance,keyspace-append,slotref-split`

Both compile checks passed (existing dead-code warnings only). No Mac database tests or benchmark runs were performed. tracker was unavailable on the final status read, so no live task status was changed; the full Phase 2 goal remains active.
