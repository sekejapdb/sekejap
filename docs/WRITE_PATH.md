# Collection write-path loop — 2026-09-12

E4 now writes vector sidecars only when their encoded bytes change. Removed
slots are deleted; changed slots use an ordinary replacement put. A scalar-only
update with an unchanged vector uses one row mutation instead of vector delete,
vector insert and row mutation. This is the same general principle used by
SQLite's selective index maintenance and PostgreSQL's unchanged-attribute and
external-value handling, adapted to E4's existing tree and immutable layouts.

Replacement and deletion also avoid materializing old embeddings as JSON
numbers. They validate every vector's length and finite f32 coordinates,
retain its bytes, and compare or delete those bytes directly. Patch updates
still materialize the old document because they need its unchanged fields.
The existing one-layout cache now shares an immutable `Arc<Layout>` instead
of cloning all field names for every lookup. There is no growing metadata
cache or per-collection resident index.

The kernel's publication, sync, WAL, checksum, page packing, and recovery
protocols are unchanged. Timestamps remain OFF by default with collection
opt-in. Committed IDs are not reused. SQL and graph/index interfaces remain
separate work. This is an improvement to collection mutation planning, not a
claim that full SQLite speed parity is complete.

## Mac results

The significant gain is in updates that retain existing embeddings. For 10K
entities with 1,536-lane vectors, four update cycles now issue **91.8% fewer
bytes** and take **40.4% less time** than the previous E4 implementation.
Both repetitions reproduce the byte counts exactly. New E4 is about **25.4%
faster than SQLite** in this particular stable-embedding update case.

| Workload | Previous E4 churn s | Current E4 churn s | SQLite churn s | Previous/current E4 issued MiB |
|---|---:|---:|---:|---:|
| 400K mixed, 12 cycles, four lanes | 115.789 | 107.563 | 38.913 | 2,196.439 / 1,801.294 |
| 100K updates, four lanes | 4.847 | 4.015 | 1.985 | 133.053 / 99.873 |
| 10K stable embeddings, 1,536 lanes | 1.158 | 0.690 | 0.925 | 122.514 / 10.041 |
| 10K changing embeddings, 1,536 lanes | 1.157 | 1.120 | 1.423 | 122.514 / 121.851 |
| 10K stable embeddings, held reader | 0.956 | 0.637 | 21.551* | 122.971 / 10.103 |

Times are means of two reversed-order repetitions except the 100K update-only
row, which is one three-arm comparison. Embedding cycles perform 8,000 public
updates total. The 400K mixed trial performs 1.92 million public mutations.
Changing embeddings still requires writing their bytes; that control shows
only a small improvement, as expected. Reinsertion likewise requires new
sidecars and shows no byte reduction (one Mac run was 1.7% slower).

*SQLite's held/rolling-reader times include attempted TRUNCATE checkpoints
that return busy. The bundled rusqlite connection installs a 5,000 ms busy
timeout (`src/inner_connection.rs:119`), and each of the four held-reader cycles
waited about five seconds. This explains most of that timing gap; it is not
evidence of a 34× raw update-throughput advantage. Checkpoint busy/frame results
remain in each raw phase. This behavior is preserved from the comparison harness,
not newly introduced to favor E4.*

| Workload | Previous E4 peak MiB | Current E4 peak MiB | SQLite peak MiB |
|---|---:|---:|---:|
| 400K mixed, four lanes | 140.027 | 139.596 | 121.840 |
| Stable embeddings | 105.841 | 87.413 | 92.546 |
| Changing embeddings | 105.841 | 105.812 | 94.711 |
| Stable embeddings, held reader | 124.312 | 87.413 | 109.747 |

The ordinary 400K mixed workload gains **7.1%** in time and **18.0%** in issued
bytes, while final size stays near the previous packing plateau: current E4
139.589 MiB versus SQLite 117.129 MiB. E4 still takes **2.76×** SQLite's churn
time there. Four-lane held/rolling snapshot peaks still exceed 2× loaded size;
this loop does not erase the broader resource-policy gap.

The [complete Mac tables](WRITE_PATH_TABLES.md) include all 48 arms, load-only,
timestamps, both repetitions, per-phase measurements and allocated peaks.
[Raw audited results](WRITE_PATH_RESULTS.json) contain **140 three-way state
comparisons** and **60.3 million exact-oracle row visits**, with zero monitor
errors. These checks include IDs and old snapshots, not just final row counts.

## Raspberry Pi result

The corrected 12-arm Pi matrix passes **36 three-way state comparisons** and
**18.75 million exact-oracle row visits** under a 128 MiB address-space limit.
All **163 Pi tests pass**, including the fixed-budget embedding test. These
are single runs per arm on the shared Pi 5; existing services remain active.

| Pi workload | Previous E4 churn s | Current E4 churn s | SQLite churn s |
|---|---:|---:|---:|
| 400K mixed, four lanes, 12 cycles | 178.741 | 161.576 | 76.289 |
| 10K stable embeddings | 2.943 | 1.045 | 2.350 |
| 10K changing embeddings | 2.945 | 2.817 | 4.948 |
| 10K stable embeddings, held reader | 2.071 | 0.832 | 21.395* |

Stable-embedding updates improve **64.5%** over previous E4 and **55.5%** over
SQLite in this trial. Issued-byte reductions and logical footprints reproduce
the Mac measurements. The 400K mixed workload improves **9.6%**, but still
takes **2.12×** SQLite's time. Maximum child RSS is 10.672 MiB for current E4
in the 400K run; held embeddings reach 14.031 MiB, versus SQLite 28.047 MiB.
RSS excludes filesystem cache and other services. The asterisk has the same
checkpoint-wait meaning explained above.

[Pi phase tables](WRITE_PATH_PI_TABLES.md) and [raw audited Pi results](WRITE_PATH_PI_RESULTS.json)
include load time, all peaks and process measurements. The first Pi matrix is
explicitly excluded: a shared Cargo target directory left the same baseline
executable reused for both E4 labels, even though the new unit tests compiled
and passed. Identical hashes and counters exposed it. The source was rebuilt
in an isolated target directory and all twelve timing arms rerun. Both runners
and the auditor now reject identical baseline/current binary hashes. Invalid
logs and reports remain under `matrix-invalid-identical-binaries` for traceability.

## Source findings and what was transferred

SQLite source is retained at `<scratch>`, commit
`f3b9f74d81132426dee1ccc07a67fdad2ccfeaa9`. In `src/update.c:573–600`, SQLite
decides which indexes might be affected by the updated columns and leaves
unaffected indexes unopened. Primary-key changes, partial-index expressions,
foreign keys and replacement conflict handling require additional work.
E4's corresponding case is an unchanged vector sidecar and an unchanged
external-key mapping. E4's external-key mapping already avoided rewrites on
replacement; this loop extends selective maintenance to vectors.

PostgreSQL source was downloaded to `<scratch>`,
branch `REL_17_STABLE`, commit
`a11bce64a3be38f726cdca682f1e5b723cfd34d1`. Relevant locations:

- `src/backend/access/heap/heapam.c:4137`: HOT eligibility checks whether
  modified attributes overlap attributes used by HOT-blocking indexes, and
  whether the new tuple fits on the same page. E4 does not adopt HOT chains;
  it adopts the narrower idea that unchanged auxiliary entries need no write.
- `src/backend/access/heap/heapam.c:4622`: `HeapDetermineColumnsInfo` compares
  interesting old/new attributes using physical representation rules.
- `src/backend/access/table/toast_helper.c:63–98`: unchanged external TOAST
  references are retained, while changed/removed references are marked for
  cleanup. This compares external references, not arbitrary large values for
  deep equality. E4 compares validated sidecar bytes because its sidecar key
  already identifies the entity and physical field slot.

Schema ordinals are not permanent semantic field identities. E4 decodes the
old row with its old layout and encodes the new row with the current layout.
It then compares the actual sidecar key/byte pairs. Reordering fields can
require replacing both physical slots even when the named vectors are unchanged.
The implementation does not assume that equal names mean equal sidecar keys.

The source sweeps used budget-limited Claude Haiku, including the user's
notes configuration. Qwen returned HTTP 403 and did not complete its sweep.
Sweep claims were checked against source: an agent's suggestion that ordinals
remain invariant across schema changes was rejected. A second delegated pass
drafted the benchmark auditor; main-agent review added missing row-count,
operation-count and issued-byte consistency checks before using it.

## Profile and remaining cost

`sample-before.txt` retains a 40-second, 2 ms Mac sample of the previous E4
binary loading and beginning churn at 400K. It contains 16,754 main-thread
samples, with 9,093 at the benchmark's three commit call sites: about 54%.
These are sampled stacks from a profiled diagnostic, not precise stage timers
or the headline benchmark. Data flushes, freelist publication and WAL/directory
sync appear prominently. The ordinary collection API publishes on every commit;
SQLite uses its native WAL auto-checkpoint policy.

Reducing sidecar writes does not remove that publication cost. This loop keeps
the same 1,000 public mutations per transaction and the same FULL durability
settings in every timed arm. It does not skip checksums, lengthen transactions,
defer acknowledged visibility or change sync primitives to manufacture a gain.

## Correctness and fixed-budget evidence

Mac validation passes **344 distinct tests**: 343 in the complete workspace
run, followed by the new fixed-budget integration test. Nineteen collection,
codec and budget tests also pass with default features. These repeat runs are
not added to the distinct-test total.

The pre-change scalar-update test fails when only one Store write is allowed;
the optimized path passes. Failure injection now covers the two real writes
of a changed-vector replacement instead of the previous three. Other multi-key
failure cases continue to refuse publication and recover the previous state.

New tests cover unchanged-vector writes, field reordering and removal, nullable
vectors, exact negative-zero/positive-zero bits, snapshot/reopen consistency,
missing sidecars, invalid lengths, NaN and both infinities. Mutation reads and
public reads reject the same malformed vector fixtures before replacing the row.
Public decoding still validates all inline bytes before fetching external
vectors. The inherited codec equivalence, corruption, overflow, process-kill,
packing and resource tests remain part of validation.

The fixed-budget test uses 1,024 entities with 1,536-lane embeddings and an old
snapshot held across twelve full update cycles. Both implementations receive
exactly the same persistent limits: 14 MiB data, 1 MiB WAL, 1,024 tracked pages,
four reader slots, 16 KiB maximum record and 256 KiB recovery allowance. The
managed total is **16,040,184 bytes**, and the test asserts that this is less
than twice the actual loaded footprint.

The previous implementation safely refuses with `retired-page bookkeeping
full`; the optimized implementation completes, preserves the old snapshot,
reopens, and verifies all 1,024 final documents. This proves completion for this
specified capped workload. It does not establish an arbitrary per-collection
filesystem quota: the existing limits apply to the database, exclude filesystem
allocation rounding/unrelated files, and may safely refuse other workloads.

Tradeoff: mutation planning retains old vector bytes plus the new encoded row
and sidecars until the operation is applied. Memory remains proportional to
one entity's payload and bounded schema size, not collection size. Large vectors
are still read and validated even when unchanged; the optimization avoids their
JSON materialization and writes, not their integrity checks. Exact comparisons
use encoded bytes, so JSON numeric equality cannot erase distinct float bits.

## Benchmark design and reproduction

The baseline is the completed packing implementation, not the pre-packing
kernel. Baseline and current binaries use the same extended benchmark harness.
The Mac matrix has 48 arms: repeated 100K/400K twelve-cycle mixed churn in
reversed arm orders; 100K load-only, updates, reinsertion, held/rolling snapshots
and timestamp opt-in; and 10K realistic embeddings with stable vectors,
changing vectors, and a held reader. All three arms—old E4, new E4 and native
SQLite—appear in every case. Four-lane payloads remain the original corpus.
Embedding cases also have two repetitions in reversed arm orders because
their timed mutation phases are short.

The realistic embedding cases use 1,536 exact f32 lanes. Stable-vector cases
change scalar/JSON values while keeping embeddings; changing-vector controls
change every vector belonging to an updated row. Updates touch 20% of rows;
reinsertion deletes and recreates 10%; mixed combines them for 40% public
operations per cycle. IDs, scalar/JSON/point/vector values, full scans, old
snapshots and reopened states are verified. SQLite also runs integrity checks.

Issued-byte counters count successful writes handed to E4's data/WAL/sidecar
writers, including reused pages. They are not file sizes or device NAND writes.
SQLite issued-byte counters were not instrumented. Both E4 binaries enable the
same counters. Logical and allocated disk peaks include all files, reader
release and schema alteration; 1 ms samples are lower bounds, not hard caps.
The timed matrix uses ordinary stores. The persistent-quota test above is
separate evidence and is not relabeled as a capped benchmark.

Some current E4 phases issue zero physical WAL bytes. Removing redundant
mutations keeps these transactions below the existing WAL buffer-flush threshold.
The unchanged checkpoint protocol durably publishes data and metadata before
discarding that buffer. WAL framing/CRC/LSNs still occur; logging and durability
were not disabled. For the 400K case, data-page writes alone fall about 7.9%;
the larger 18.0% total reduction includes avoided WAL flushes.

Mac: M3 Pro, 18 GiB, buffered I/O, 8 MiB database cache, Rust 1.96.0, bundled
SQLite 3.46.0, FULL sync with platform-native barriers. The Pi uses the same
source and cache, existing isolated toolchain/vendor directory and a 128 MiB
virtual-address-space limit. Shared Pi sensor/LLM services are left running;
RSS excludes filesystem cache and other processes. Reopens use warm OS caches.

Artifacts live under `<scratch>/` and
`<scratch>/`.
The source before this loop is archived by the packing report. The baseline
build is isolated from the working tree; neither prior retained database pair
is modified. Reproduction scripts:

```sh
# Copy separately built same-harness binaries to the isolated artifact root.
bash tools/run_write_path.sh
python3 tools/record_write_path.py <scratch>
# On the already provisioned Pi, validation and benchmarking are separate.
bash write_path_pi.sh validate
bash write_path_pi.sh bench
```

`COLLECTION_VECTOR_DIM` and `COLLECTION_CHANGE_VECTORS` select the shared corpus
for both engines. The runner refuses an existing matrix directory; use a fresh
artifact root when repeating rather than overwriting prior evidence.

## Retained evidence and cleanup

The final [source provenance](WRITE_PATH_PROVENANCE.json) records independently
verified archive members, distinct benchmark binaries, reference commits and
matching Pi source hashes. CONTRACT.md and all kernel files remain byte-for-byte
identical to the packing baseline.

After successful audits, cleanup removed 48 disposable Mac database directories
(3,343,473,388 logical bytes; 3,639,959,552 allocated bytes) and 22 Pi directories
(2,372,872,744 logical bytes; 2,372,911,104 allocated bytes). The latter includes
the twelve excluded initial timing arms. Generated data can be reproduced;
reports, test logs, sources and the main comparison databases remain retained.
See [Mac manifest](WRITE_PATH_CLEANUP.json) and [Pi manifest](WRITE_PATH_PI_CLEANUP.json).

This bounded write-path loop is complete. Overall collection parity remains open:
mixed-workload commit cost and a general bounded-expansion gate still need work.
