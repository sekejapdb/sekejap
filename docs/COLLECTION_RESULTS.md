# Typed collection loop results — 2026-09-11

The internal collection boundary is implemented; **330 workspace tests pass**
(0 failed, 0 ignored), including 12 new collection tests.
**The complete API is not at SQLite parity.** These measurements include the
catalog, immutable layouts, composite IDs, external-key index and separate
vector records that earlier entry-only benchmarks did not include. Do not
apply the earlier 3–4% density result to this workload.

## Main paired comparison

Apple M3 Pro, 18 GiB RAM, macOS 26.5.1; both engines stored on the same scratch
volume. Rust 1.96.0; bundled SQLite 3.46.0. Two repetitions, reversed engine order. Times below are means of both
runs; peaks are the maximum sampled in either run. MiB = 1,048,576 bytes.

| Rows | Timestamps | Engine | Load s | Loaded MiB | Three-cycle mixed churn s | Churn peak MiB | Churn final MiB |
|---:|---|---|---:|---:|---:|---:|---:|
| 100K | OFF | E4 | 5.542 | 30.707 | 6.314 | 40.558 | 40.302 |
| 100K | OFF | SQLite | 1.463 | 23.281 | 2.333 | 33.961 | 29.258 |
| 100K | ON | E4 | 5.334 | 32.914 | 6.349 | 41.996 | 41.740 |
| 100K | ON | SQLite | 1.468 | 24.219 | 2.367 | 34.875 | 30.293 |
| 400K | OFF | E4 | 21.776 | 126.684 | 25.625 | 160.996 | 160.740 |
| 400K | OFF | SQLite | 5.776 | 93.266 | 9.237 | 121.840 | 117.129 |
| 400K | ON | E4 | 21.549 | 137.141 | 25.841 | 169.915 | 169.658 |
| 400K | ON | SQLite | 5.856 | 97.020 | 9.420 | 125.851 | 121.254 |

At 400K/OFF, E4 is **35.8% larger after load**, takes **3.77×** the load time,
and **2.77×** the mixed-churn time. Retained size after three cycles is **37.2%
larger**. These are complete API costs, not a regression comparison against
an identical previous entry-only workload.

Adding timestamps costs E4 **10.46 MiB / 8.25%** after the 400K load here,
versus **3.75 MiB / 4.02%** for SQLite. This includes resulting page occupancy
and retained physical space, not just two logical integer values. Timing
variation makes E4/ON slightly faster for this load; it is not evidence that
adding timestamps accelerates writes. OFF remains the default.

See [every arm and phase](COLLECTION_TABLES.md) and [machine-readable results](COLLECTION_RESULTS.json).

## Fairness and exactness

- Two collections, each holding half the population. Ordered external keys
  `person/00000000` etc.; sequential IDs per collection. A deleted and reinserted
  key receives a new sequence in both engines. The corpus includes Unicode
  names, nullable real values, a Boolean, a point, nested JSON, unsigned u64
  extras and four exact f32 vector lanes.
- E4 uses dense-v3 rows, a separate vector keyspace, hidden recoverable external
  keys in rows, an external-key index, and triplicated metadata. SQLite uses a
  `WITHOUT ROWID` table with composite `(cid, seq)` primary key and unique
  `(cid, external_key)` index, collection/sequence/layout metadata, native
  scalar columns, JSONB, and inline vector BLOBs. SQLite is allowed its compact
  native representation; no artificial vector side table is imposed on it.
- Both use 4096-byte pages, 8 MiB writer cache, buffered I/O, full durability
  with macOS fullfsync enabled, and batches of 1000 public mutations. Documents
  are generated inside the timed region in both arms. Initial collection/table
  setup is excluded and its footprint is recorded separately. No sorted bulk
  shortcut or public scalar/vector/spatial index is involved.
- Both publish each transaction. E4 currently checkpoints each API commit;
  SQLite commits to WAL with native `wal_autocheckpoint=1000`, then checkpoints
  at each phase end. We do not force SQLite to checkpoint on every commit.
  A final empty transaction is included in each phase for both engines.
- Each churn cycle replaces 20% of rows, deletes/reinserts another 10% and leaves
  70% unchanged: **0.4 × population public mutations per cycle**. One percent
  of rows alternate 24/1024-character JSON notes. Fixed timestamps make both
  engines produce identical managed values; SQLite's benchmark writer maintains
  its explicit timestamp columns without SQL triggers. This tests the whole
  E4 API against equivalent SQLite operations, not a trigger implementation.
- No old reader is held across churn in this benchmark. Snapshot correctness
  is tested separately. These tables do not re-prove held-reader disk bounds,
  Pi timing, physical disk reservation or recovery-workspace limits.
- Every phase, the pre-alter state and the reopened post-alter state are checked
  against the independent document generator. IDs and external keys participate
  in cross-engine digests, and SQLite additionally runs `integrity_check`.
  **16 arms, 40 paired state checks, 24 million exact row visits** pass.
- Both collections receive a new optional declared field after churn. Row
  digests remain unchanged after alter/reopen. The E4 unit test independently
  checks raw old-row bytes and removal of obsolete vector entries. Warm reopen
  and schema-change costs are reported separately in the full tables.

## Space during work

Directory-wide monitoring samples logical lengths and filesystem allocation
at approximately 1 ms intervals, including data, WAL and bookkeeping files.
Samples are **lower bounds**, not guarantees about an unsampled instant.
Allocated peaks can exceed logical lengths because the filesystem allocates
larger extents. These ordinary-store runs do not enforce a storage cap.

At 400K/OFF across the initial three cycles:

| Measure | E4 | SQLite |
|---|---:|---:|
| Loaded logical MiB | 126.684 | 93.266 |
| Maximum sampled logical MiB | 160.996 | 121.840 |
| Logical peak / loaded size | 1.271× | 1.306× |
| Maximum sampled allocated MiB | 176.652 | 134.027 |

A smaller expansion factor does not make the larger absolute E4 footprint
smaller than SQLite. Also, E4 grew through all three cycles; those results alone
cannot establish a steady state or a universal 1.5×/2× safety factor.

## Twelve-cycle follow-up: no convergence yet

The three-cycle growth warranted one additional matched **400K/OFF** pair,
E4 first. Both run 12 churn cycles with the same operations and exact oracles.
This is a single diagnostic pair, separate from the repeated matrix above.

| Measure | E4 | SQLite |
|---|---:|---:|
| Twelve-cycle mixed churn time | 107.392 s | 37.325 s |
| Maximum sampled logical size | 258.117 MiB | 121.840 MiB |
| Retained logical size after cycle 12 | 258.111 MiB | 117.129 MiB |
| Logical peak / loaded size | **2.037×** | 1.306× |
| Maximum sampled allocated size | 272.168 MiB | 134.027 MiB |
| Allocated peak / loaded allocation | **2.124×** | 1.381× |

E4 **fails a 2× expansion target here**, even without a pinned old reader.
It keeps growing; 2.037× is not a safe capacity recommendation or an upper bound.
SQLite's retained logical size is unchanged after the first cycle. The full
[cycle table](COLLECTION_SUSTAINED_TABLES.md) and [raw results](COLLECTION_SUSTAINED_RESULTS.json)
show both counterparts throughout. This additional pair passes 14 paired state
checks and 12 million exact row visits. Combined evidence: **18 arms, 54 paired
state checks, 36 million exact row visits**.

Independent structural verification after schema alteration finds:

| E4 state | Live records | Reachable tree pages | Other non-meta pages | Physical pages |
|---|---:|---:|---:|---:|
| After cycle 3 | 1,200,029 | 40,666 | 480 | 41,148 |
| After cycle 12 | 1,200,029 | 65,590 | 483 | 66,075 |

Almost all the added pages are **reachable tree pages**: +24,924, or 97.36 MiB.
The other-page count increases by only three. Therefore this is not explained
primarily by a growing unreclaimed freelist. Live typed bytes are actually
smaller in cycle 12, because that even cycle has short JSON notes. The next
investigation should measure leaf occupancy, deletion merging and reuse when
reinserted entities receive new IDs across the row/vector/index keyspaces.
The page counts narrow the investigation; they do not yet isolate a specific
algorithmic defect. No page-packing fix is claimed in this API loop.

Earlier entry churn reused the same underlying numeric key on reinsertion.
The collection API assigns a fresh sequence to a deleted/reinserted entity,
which preserves graph identity safety and changes the physical insertion
pattern. We must benchmark that real pattern rather than reuse committed IDs
to improve the chart. The internal API contract can stand while this storage
implementation is improved; SQL/service expansion should wait for this new
full-API space/performance gate.

## Where the bytes and work go

Read-only attribution after the 400K/OFF three-cycle run finds, before page
framing and unused capacity:

| Live keyspace | Key + value MiB |
|---|---:|
| Typed entity rows, including recoverable external keys | 73.57 |
| External-key index | 8.28 |
| Vector records | 9.42 |
| Header, catalog, sequence and immutable layouts | 0.06 |

Exact counts are retained as `footprint-off.json` and `footprint-on.json` in the
artifact directory. This is **live record attribution after churn**, not a
physical-page allocation breakdown; the difference to the file size includes
page headers, occupancy, retained/reusable pages and bookkeeping. It cannot
assign the whole gap to one cause.

The implementation performs three tree writes for a new vector-bearing entity
(vector, row, external-key mapping). Replacement reads the old entity and
removes/replaces vector entries; every commit publishes a root. Those are
concrete additional operations worth profiling. The current measurements do
not isolate how much time each costs. Large vectors may amortize their separate
key cost differently from the four-lane vectors here; that is not measured.

The next optimization target is this full API path: reduce avoidable descents,
old-value decoding and publication overhead while preserving its semantics,
then measure sustained delete/reinsert space with non-reused IDs. Keep vectors
in their keyspace as required by the README. No kernel or format rewrite was
made to manufacture a win in this loop.

## Reproduction and evidence

```sh
cargo build --release --offline --features sqlite-balance,compact-cells --bin collections
sh tools/run_collections_loop.sh
python3 tools/record_collections.py
TMPDIR=<scratch> \
  cargo test --release --offline --workspace \
  --features sqlite-balance,compact-cells -- --test-threads=1
```

The runner refuses existing arm directories. Set `E4_COLLECTION_ARTIFACTS` to
a fresh scratch subdirectory for both the runner and recorder when reproducing. Input is generated deterministically;
reports and logs remain outside the disposable database directories.
Artifact root: `<scratch>/`.
The [collection contract](COLLECTIONS.md) names the recovery limits and API
semantics. SQL, service deployment, full vector candidate recovery and broader
resource gates remain separate work.

## Final validation, cleanup and handoff

The full release workspace suite passed **330 tests**, with zero failures or
ignored tests. Afterward the identity regression was extended and rerun to prove
that deleting the last row, committing and reopening still cannot reuse its
committed ID. The footprint inspector also independently verified both 400K
E4 tree structures and their 1,200,029 live records. The first workspace build
caught a path-type error in the new inspector; the final suite passed after its
fix. Inherited compiler warnings remain.

tracker marks `typed-interface-bridge` and `typed-metadata` done for this
internal slice. `full-collection-parity` is the next task, and broader hybrid
storage depends on it. Existing recovery/resource/foundation gates remain open.

After verification, 18 disposable matrix/smoke databases were removed:
**1,498,549,392 logical bytes / 1,590,042,624 allocated bytes**. The exact
[cleanup manifest](COLLECTION_CLEANUP.json), reports, red/green logs, structural
counts and source evidence are retained. The two 12-cycle databases remain at
`<scratch>/` for the next
occupancy investigation. No Raspberry Pi workload was stopped or changed in
this loop; these new timings are Mac results.
