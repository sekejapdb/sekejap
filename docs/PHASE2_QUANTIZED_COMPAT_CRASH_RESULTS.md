# Quantized-family compatibility and crash evidence

The isolated server Linux runs `quant-fixture-r1` and `quant-crash-r2` both
finished with exit 0 on 2026-09-17. These qualify the captured candidate;
they are not a Phase 2 release or a format freeze.

## Compatibility

Twenty immutable source fixtures cover five profiles (exact vector, spatial,
text, quantized vector, all families), two build configurations and both
checkpointed and pending-WAL sources. All 40 current cross-build semantic
read/write/readback arms passed. Sixteen arms exercise the quantized/all
profiles, including pending WAL at source and handoff.

The preserved earlier five-family engine supports logical mask 31. Its 40
snapshot/writer admission arms passed: 24 accepted supported profiles and 16
refused the new quantized feature, bit 32, before changing source bytes.
All 60 source-alias/overlap guards passed. This is additive capability admission:
opening an existing ordinary database does not enable the new family.

This report adds candidate-family evidence to the separate preserved Phase 1
baseline checks. Same-candidate cross-build handoffs alone do not prove future
release compatibility. Captured engine sources and binaries remain on server;
report inventories record their identities.

## Process interruption

Both default and retained builds completed 42 cases, 84 total:

| Cases per build | Coverage |
|---:|---|
| 6 | Whole multimodel transaction: before commit, after commit, four timed commit-window kills; exact original or updated state and unchanged held snapshot |
| 30 | Scalar, exact-vector, spatial, text and quantized build/drop, each interrupted before/during/after commit; exact lifecycle state and resumability |
| 6 | Deterministic existing PageWAL checkpoint interruption stages |

All source inventories remained unchanged. In this run every timed commit
sample recovered the updated endpoint. That is an observation of scheduling,
not proof that each internal commit write was interrupted. Exhaustive in-crate
write-fault tests provide separate boundary coverage. The quantized oracle
includes an `ef=1` result that changes when the persisted code changes, as well
as full exact reranking. These tests do not claim physical power-loss coverage.

## Evidence and remaining work

Raw reports, source inventories, completion markers and run logs are in
[quant-compat-crash-r1](phase2-evidence/quant-compat-crash-r1/).

- Compatibility report SHA-256: `681f6437ae2f7c8be38b9fe4f0e3acca4dcbfad63937dd8b0b7e5f4212ddd9a0`.
- Crash report SHA-256: `5b4aad76ace1ef67514260d8b08a745075aa547ddda464b545152aca6bc61a4b`.
- Native root: `<scratch>` in server namespace
  `sekejap-benchmark`, pod `e4-phase2-20260916-c9rp8`.
- Fixture source: `quant-fixture-r1-src`; crash source: `quant-crash-r2-src`.
  Both derive from `approx-query-r1-src` with separately inventoried harness
  changes. Later `query-guards-r2` changes are not covered by these captures.

Final-source workspace qualification, the pending query guard run, complete
10K/100K/representative-1M comparison, reader-refusal committed-state checks,
and the scoped acceptance audit remain required. No Phase 2 commit is claimed.
