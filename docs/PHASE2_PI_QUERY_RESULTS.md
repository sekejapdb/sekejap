# Phase 2 native Pi query qualification

The captured query candidate passes **22 native integration tests** on the
Raspberry Pi: 8 multimodel and 3 scalar tests in each of the default and retained
feature configurations. There are no failures, ignores or filtered tests in
these suites. The enclosing correctness run exits 0, with all 11 stages passing.
This is correctness evidence, not performance acceptance or a full-workspace run.

The tests cover mixed-family results, single-snapshot behavior across writes,
filtering before ranking, complete pagination beyond 65,536 matches, null and
missing scalar values, exact numeric semantics, cancellation and budget retry,
and malformed scalar-posting rejection. The complete test names and assertions
remain in the captured source; raw logs are preserved in
[pi-query-r1](phase2-evidence/pi-query-r1/).

The exact source is pinned by `current-source.json` (472 files). The runtime
includes the strict scalar-equality membership experiment in `src/query.rs`;
its source SHA-256 is
`fe339a3e6261d97361e5ca4ad5e0271358f35351618d3d1edfd9039fc57a397d`.
Retained features are `compact-cells,sqlite-balance,keyspace-append,slotref-split`.
Both commands use release builds, the same lockfile and offline dependencies,
and run `query_scalar` plus `query_multimodel` with one test thread. The captured
`run.sh`, platform and Rust toolchain details are included with the evidence.

The existing Pi services remain active. This run has a one-CPU quota and low
priority; its times must not be compared with server's benchmark. The sampled
memory guard never stopped the task. Its observed RSS peak was 738,312,192 bytes,
but monitoring began after initial compilation and the Pi has no kernel memory
controller. This is neither a whole-run peak nor a hard memory-cap claim. The
guard's terminal `STOPPED_MONITOR_DIFFERENT_INVOCATION` means the monitored
service invocation ended; it does not report a test failure.

The same captured engine also passes the separate
[416-cycle rollback matrix](PHASE2_ROLLBACK_RESULTS.md). The full-workspace
547/552 passes qualify the earlier R7 runtime, not this candidate. Retaining the
optimization still requires measured benefit in the queued read-only
baseline/candidate/SQLite replay; otherwise the experiment will be reverted.
