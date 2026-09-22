# V2 foundation loop — typed collections on the page-WAL store, 2026-09-16

## Status

**Candidate r3 tested on Linux (`FINAL_EXIT0` main job, `LEAN_EXIT0` remaining
lean/kernel job) and retained for continued development.**
Source is identical to tested candidate r3 (archive
`ee3ae0196ae832de72d67e9c73218d071288f52578df5e07c5b8bce5652feaf1`). The integration commit records this tested source without a co-author. The
accepted commit `64b6663` contains **only** a prior raw-KV page-WAL recovery
fix (binding WAL recovery to the database identity and checkpoint history) —
it does **not** contain this typed-collection integration.

This is a **functional-improvement retention for continued development.** It
is explicitly **not**: raw-write-optimization acceptance, a stable disk-format
freeze, or a production release. `L8-COMPAT` stays PENDING.

**Disk comparison accepted by the owner:** “as long it's comparable with
SQLite is okay.” Peak allocated space was 620.04 MB versus 615.74 MB with
timestamps off (**+0.70%**), and 937.96 MB versus 900.30 MB with timestamps on
(**+4.18%**). Final logical size was about **11–12% larger**. A separate 2x
physical-space ceiling is **not a blocker for this loop** under that
clarification. Write/checkpoint/reopen time remains **2.28x SQLite off** and
**2.18x on**; this integration is not a raw-write speed improvement claim.

Allocated peaks exceeded logical peaks in both engines. Keep those real
costs visible: samples are lower bounds, and `ResourceLimits` guards logical
data/WAL bytes rather than guaranteeing physical block reservation. The
sampler does not establish the allocation spike's cause. None of these
limitations changes the owner's acceptance of the measured disk comparison.

## What is in candidate r3

`collections::Database` stores every operation through `PageWalStore`
(`E4PWAL02`); the inherited kernel `Store` is no longer a collection backend
(`src/collection_backend.rs` is the one place the backend is selected). Full
description of the surface, publication protocol and limits: see
[`docs/core/V2_COLLECTION_INTEGRATION.md`](V2_COLLECTION_INTEGRATION.md) (exact
technical protocol preserved there; only its status framing was corrected).

Artifacts: source archive
`ee3ae0196ae832de72d67e9c73218d071288f52578df5e07c5b8bce5652feaf1`; benchmark
binary
`d717f3955870867661d3ecd5df3d0e5f4e1c34c44d1ef7f112596b9b7656000a`; portable
evidence export (this report's underlying JSON/logs, packaged)
`2c303cb8ec19e653eef788a53d7e16d9de25e9066b91eabad5ea614ce69af084`
(`Linux evidence.tar.gz`; raw files also under `docs/v2-foundation-evidence/`
in this repo).

## Test evidence (candidate r3, Linux)

Default and compact-cells feature builds each pass, from the actual test
logs:

| Test target | Passed |
|---|---:|
| `lib` | 25 |
| `collection_pagewal` | 15 |
| `collections` | 7 |
| `pagewal` | 13 |
| `pagewal_repair` | 1 |
| `pagewal_recovery_identity` | 6 |
| `pagewal_transaction_capacity` | 1 |
| `recovery_faults` | 14 |
| `schema_recovery` | 7 |
| **Total per feature mode** | **89** |

**89 unique entries × 2 feature modes = 178** (child subprocess reader/writer
summaries inside `collection_pagewal` are excluded from this count — they
are covered inside their parent test). Release build passed. Both 10K smoke
runs (timestamps off/on) passed.

The separate lean job (`LEAN_EXIT0`) additionally ran the
kernel/io/packing lean groups: 74 further executions across both feature
modes (35 default + 39 compact — `lean-typed`, `lean-io`, `lean-packing`,
`lean-kernel` logs). **178 + 74 = 252 total test executions across
default+compact.** Two of the compact-only groups (`compact_cell_bounds` in
`lean-kernel-default`, `neighbor_capacity_tests` in `lean-packing-default`)
correctly report 0 passed / all filtered out under the default feature mode
— that is expected feature-gating, not missing coverage; their compact-mode
counterparts (1 and 3 passed respectively) are the real count.

## Compatibility fixture evidence

Four checks against candidate r3's compat-copy, all completed and all
original fixture hashes unchanged (`sha256sum -c` on every original
`old-source`/`projected` file: `OK`):

| Check | Result |
|---|---|
| `compat-new` (`verify-and-update --confirm-copy`) | Both `checkpointed` and `wal_pending` targets: baseline verified (5 entities, 2 deletions), update+insert+delete+commit+reopen passed |
| `compat-old-raw` (`verify-raw`) | Both targets: raw `PageWalStore` opened and scanned 37 entries (structural check only) |
| `compat-old-typed` (`legacy-replay`, checkpointed) | Old typed decoder read 5 entities from 37 replayed KV pairs (encoding test only) |
| `compat-old-typed-wal` (`legacy-replay`, wal-pending) | Old typed decoder read 5 entities from 37 replayed KV pairs (encoding test only) |

The reference fixture itself was built with the **accepted `64b6663`**
`Database` (old typed encoder over the old `Store`, since that commit
predates this integration) and streamed into a fresh `PageWalStore` via a
helper — not through candidate r3's integrated writer. This remains bounded
preparatory evidence that the typed encoding survives the move into the
page-WAL container; it is **not** proof of the candidate writer's
byte-for-byte compatibility and **not** a released or frozen baseline. See
[`docs/core/V2_COMPAT_FIXTURES.md`](V2_COMPAT_FIXTURES.md). No index-compatibility
proof exists yet.

## Benchmark: 1M mixed-CRUD, off/on timestamps

Protocol, fairness knobs, schema, oracle design and named asymmetries are
fully specified in
[`docs/core/V2_BENCHMARK_PROTOCOL.md`](V2_BENCHMARK_PROTOCOL.md) and are
preserved unchanged here — only the results below are new. Cache 8 MiB,
`SyncMode::Full`, batch 1000, `wal_autocheckpoint=1000` pages for SQLite. One
sequential sekejap-then-SQLite run per variant, on the same Linux server; no
repeated/averaged runs and no isolated timestamp-generation-cost claim
(content generation is included in write time for both
engines identically; oracle verification time is real but excluded from
these totals and reported separately). No VACUUM/rebuild in either arm.
Native API differences (e.g. sekejap's `put()` always performing its own
existence lookup vs. SQLite's driver-known INSERT/UPDATE) are named in the
protocol doc, not repeated here. **This measures raw mixed-CRUD throughput
and disk footprint only — full multimodel query/SELECT performance is not
measured, and these numbers are not directly comparable to the earlier,
separate raw-KV 1M results** (different data model and API).

Both off and on runs: **2,400,000 total operations each** (1,000,000 load +
200,000+200,000 updates + 100,000 delete + 100,000 reinsert + (200,000
update + 100,000 delete + 100,000 create) × 2 mixed rounds), independently
verified — exact id, exact deleted-key absence, exact timestamps (on run),
and a cross-engine CRC32C match — at every phase and after reopen, on both
engines, in both runs.

All disk values below use decimal MB (1 MB = 1,000,000 bytes).

### Load-only

| Variant | Engine | Load seconds | Final logical | Final allocated |
|---|---|---:|---:|---:|
| off | E4 | 50.001 | 285.30 MB | 285.30 MB |
| off | SQLite | 17.580 | 257.85 MB | 257.85 MB |
| on | E4 | 50.220 | 296.15 MB | 296.16 MB |
| on | SQLite | 19.915 | 271.27 MB | 271.27 MB |

### Per-phase operation counts and write time (seconds; identical counts off/on)

| Phase | Creates | Updates | Deletes | E4 off | SQLite off | E4 on | SQLite on |
|---|---:|---:|---:|---:|---:|---:|---:|
| load | 1,000,000 | 0 | 0 | 50.001 | 17.580 | 50.220 | 19.915 |
| update_round_1 | 0 | 200,000 | 0 | 12.168 | 5.376 | 9.169 | 6.769 |
| update_round_2 | 0 | 200,000 | 0 | 9.693 | 5.698 | 9.528 | 5.728 |
| delete_only | 0 | 0 | 100,000 | 7.150 | 3.156 | 7.726 | 3.621 |
| replacement_reinsert | 100,000 | 0 | 0 | 5.647 | 3.459 | 4.708 | 1.894 |
| mixed_round_1 | 100,000 | 200,000 | 100,000 | 17.461 | 8.136 | 23.139 | 9.714 |
| mixed_round_2 | 100,000 | 200,000 | 100,000 | 19.063 | 9.676 | 21.759 | 10.150 |

Write-time-only sum (the seven `seconds` values above): off sekejap 121.184 /
SQLite 53.080; on sekejap 126.249 / SQLite 57.791.

### Checkpoint and reopen (separate from write time and from each other)

| Variant | Engine | Cumulative per-phase checkpoint | Final checkpoint | Reopen | Total (writes+checkpoints+reopen) |
|---|---|---:|---:|---:|---:|
| off | E4 | 0.132037 s | 0.000211 s | 0.019050 s | **121.335738429 s** |
| off | SQLite | 0.069579 s | 0.000115 s | 0.003406 s | **53.153442817 s** |
| on | E4 | 0.109374 s | 0.000255 s | 0.018878 s | **126.377621387 s** |
| on | SQLite | 0.104049 s | 0.000197 s | 0.003470 s | **57.898439067 s** |

Every per-phase and final checkpoint reports `completed: true`/no busy
readers on both engines in both runs — no deferred checkpoint in this run.
Oracle verification (excluded from every total above, reported separately):
cumulative verify seconds — off sekejap 93.255 / SQLite 98.748; on sekejap 107.179 /
SQLite 112.709; reopen verify — off sekejap 13.460 / SQLite 14.913; on sekejap 15.710 /
SQLite 16.340.

### Disk: logical vs. allocated (1 ms-sampled peaks are a lower bound, not a
guaranteed maximum)

| Variant | Engine | Final logical | Final allocated | Peak logical (any phase) | Peak allocated (any phase) | Peak allocated ÷ post-load allocated |
|---|---|---:|---:|---:|---:|---:|
| off | E4 | 380.42 MB | 380.42 MB | 384.74 MB | **620.04 MB** | **2.17x** |
| off | SQLite | 339.24 MB | 339.24 MB | 343.85 MB | **615.74 MB** | **2.39x** |
| on | E4 | 393.15 MB | 393.15 MB | 397.61 MB | **937.96 MB** | **3.17x** |
| on | SQLite | 355.34 MB | 355.34 MB | 360.20 MB | **900.30 MB** | **3.32x** |

(Final size numbers, exact bytes: off 380416096/339243008; on
393150560/355344384 logical, allocated 380420096/339243008 off and
393154560/355344384 on — matching `results.json` exactly.) All sampled
peaks report `sample_errors: 0`. Final allocated settles close to final
logical in every arm, but that does **not** erase the transient allocation
pressure recorded mid-run above — it is a real, if temporary, disk-budget
event on both engines, worse on sekejap in the timestamps-on run. The peak allocated value in
every case above lands in `mixed_round_1` or `mixed_round_2` (the phases
combining create+update+delete), on both engines.

## Eight-law coverage versus specific unmet criteria

All "covered by candidate r3" entries below refer to the tested, retained
integration — not the separately accepted `64b6663` commit.

| Law | Status | Covered by candidate r3 | Specific unmet criteria |
|---|---|---|---|
| 1 Disk-first | PENDING | No change from prior lean/kernel coverage | Aggregate pager/repair memory accounting; large-data process-budget confirmation |
| 2 Work ∝ change | FAIL (unchanged) | No change from prior scaling result | Strict flat-latency law for scattered insertion remains unmet; unrelated to this loop |
| 3 Nothing fallible may delete | PENDING | `pagewal_recovery_identity` (6 tests): foreign/stale-WAL refusal, damaged-checkpoint-header recovery; `collection_pagewal`: rollback beside a live snapshot, reopen recovering committed WAL before any checkpoint, unsupported-source refusal before any byte changes | Full create/open/repair-destination failure and publication matrix for the integrated typed path |
| 4 Name your sacrifice | PASS (reporting practice, not a completeness claim) | This report names the allocated-vs-logical peak gap, the sampler's lack of per-file attribution, and every deviation/limit in V2_COLLECTION_INTEGRATION.md/V2_BENCHMARK_PROTOCOL.md | No full-engine qualification implied; disk comparison accepted against SQLite; allocation cause is not established |
| 5 No corruption unrecoverable | FAIL (unchanged overall) | Identity-bound WAL refusal and schema/resource-policy regressions above | Corrupt-WAL-region salvage and rootless current-membership proof remain open; no released-format fixture |
| 6 Write never blocks read | PENDING (**not** zero-blocking) | `collection_pagewal`: real cross-process reader excluding a concurrent checkpoint and staying stable; cross-process writer restart beside a pinned reader | Reader **admission** can block behind an in-flight checkpoint (an intentional admission wait — see "Live-writer admission" / "Quiescent admission" in V2_COLLECTION_INTEGRATION.md); only *after* admission does a snapshot read perform no further coordination I/O. Sustained-load restart beside a survivor, resource/scale cross-process concurrency and the zero-degradation latency/I-O regression gate remain open |
| 7 Ingest usable on device | PENDING | Typed collection tests now exercise the integrated page-WAL store, not the inherited Store | Bulk import, late indexing, indexed live writes; device (Pi/app-scale) qualification of the integrated path |
| 8 Compatibility is permanent | PENDING | Reference-fixture pass and the four compat checks above are **bounded pre-release encoding-compatibility evidence only** | No released baseline exists yet; persisted-index compatibility, minor-version rollback and public-interface compatibility remain entirely untested |

`docs/FOUNDATION_GATES.json` is intentionally not promoted or edited by this
report; it remains the parent's record of overall qualification status.

## Decision and next milestone

Keep the integration and the benchmark evidence. Disk behavior is accepted
relative to SQLite; do not start another disk optimization loop or make a
separate 2x allocation ceiling a prerequisite. The next product milestone is
the named format/index namespace and interface contract described in
[FORMAT_V2.md](FORMAT_V2.md), with remaining correctness and
compatibility requirements explicit. Write throughput remains a measured
cost to improve; combined multimodel query benefits have not been measured.

Per-file allocation attribution is optional follow-up work if a future
application requires a hard physical-space cap. This run does not provide
such a guarantee and does not claim a frozen format or production release.

## Evidence and retention

[Portable logs and provenance](v2-foundation-evidence/README.md) and the raw
[off JSON](v2-foundation-evidence/bench-1m-off/v2-foundation-1000000-1789519186/results.json)
and [on JSON](v2-foundation-evidence/bench-1m-on/v2-foundation-1000000-1789519581/results.json)
contain the exact measurements. Larger generated benchmark databases are
removed after verification; source, binaries, reports and the small
compatibility reference databases are preserved on the Linux PVC.
