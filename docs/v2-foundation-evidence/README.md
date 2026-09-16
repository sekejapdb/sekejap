# V2 foundation evidence — Linux, 2026-09-16

The source tested is candidate r3, archive SHA-256
`ee3ae0196ae832de72d67e9c73218d071288f52578df5e07c5b8bce5652feaf1`.
The earlier commit `64b6663` supplies the raw page-WAL recovery baseline;
it does not contain the typed collection integration.

- Main job: `v2-candidate-r3-test-20260916`, `FINAL_EXIT=0`.
- Additional lean job: `v2-r3-lean-evidence-20260916`, `LEAN_EXIT=0`.
- Selected tests: 178 executions; remaining lean tests: 74; total 252.
  Child-process summaries are not counted twice. Compact-only suites have
  zero tests in the default configuration; only actual executions count.
- Both 10K smoke variants and both 1M variants passed. Every variant compares
  E4 and SQLite sequentially with timestamps either off for both or on for both.
- Four bounded cross-binary fixture checks passed. Original fixture hashes
  stayed unchanged. These are pre-release encoding/container tests, not
  released-file or persisted-index compatibility qualification.

`logs/` preserves exact commands and stdout/stderr from Linux. The benchmark
subdirectories contain raw JSON and the harness-generated Markdown. The
reviewed interpretation is [V2_FOUNDATION_LOOP.md](../V2_FOUNDATION_LOOP.md).
In particular, harness Markdown's phase peak columns are **logical** peaks;
allocated peaks are in JSON and must also be considered. No database, WAL or
binary is copied into this evidence directory.

Full source, binaries and immutable compatibility fixtures remain on
the authorized PVC under `<scratch>/`.
Candidate artifacts are under `candidate-r3`; the older helper and untouched
reference archive are under `reference-fixture`. Keep the reference archive.

The Linux evidence export was SHA-256 verified before extraction:
`2c303cb8ec19e653eef788a53d7e16d9de25e9066b91eabad5ea614ce69af084`.
`SHA256SUMS` covers the extracted evidence and this note. Source and binary
hashes are separately retained in `logs/`. The source tree's documentation
was subsequently updated with results; implementation source is unchanged.

After verification, the eight generated benchmark database directories were
removed (`logs/benchmark-cleanup.stdout.log`, `CLEANUP_EXIT=0`). Reports and
small compatibility reference files remain. The cleanup log was collected
after the original export, so it is covered by SHA256SUMS, not its tar hash.
