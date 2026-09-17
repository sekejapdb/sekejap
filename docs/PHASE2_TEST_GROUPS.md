# Phase 2 test groups

Status: candidate qualification map, 2026-09-17. Passing a group below is
evidence for the named behavior; it is not by itself a Phase 2 release, a
format freeze, or proof of all eight laws.

## Reusable entrypoint

`tools/phase2_qualify.sh` has two Cargo profiles and three build selections.
Database execution is Linux-only. The caller must supply an absolute,
pre-existing, authorized temporary root and a new output directory:

```sh
tools/phase2_qualify.sh \
  --profile lean \
  --config both \
  --tmpdir /authorized/e4/test-tmp \
  --output /authorized/e4/evidence/phase2-lean-r1
```

Passing `--tmpdir` is the caller's authorization to create database artifacts
there. The script never falls back to `/tmp`. It creates and retains a new
run subdirectory, exports that exact path as `TMPDIR`, and records it in
`metadata.txt` and `summary.txt`. `SQLITE_TMPDIR` is exported to the same path,
so full-profile SQLite tests cannot spill temporary files into an implicit
system directory. Every Cargo invocation uses `--locked` and `--offline`. A
failing group does not suppress later groups; each command, combined log,
exact Cargo and log-writer exit codes, runner validation result, reason and
elapsed time is retained. A log-write failure fails the run. The script exits
nonzero if any group fails.

`--config default` enables no Cargo features. `--config retained` enables the
four retained storage defaults used by the existing Phase 2 evidence:
`compact-cells,sqlite-balance,keyspace-append,slotref-split`. `both` runs the
same source and test selection sequentially under both configurations.

Argument parsing can be checked without database work, including on macOS:

```sh
tools/phase2_qualify.sh --validate-only \
  --profile lean --config both \
  --tmpdir /authorized/e4/test-tmp \
  --output /authorized/e4/evidence/phase2-lean-r1
```

## Lean selection

Lean is the repeatable pre-merge correctness gate. It deliberately favors
small deterministic fixtures. It contains these groups:

| Group | Test sources | What it checks |
|---|---|---|
| Small semantics | `tests/collections.rs`, `index_scalar_oracle.rs`, `graph_collections.rs`, `index_vector.rs`, `index_vector_quantized.rs`, `index_spatial.rs`, `index_text.rs` | Typed CRUD and stable IDs; independent scalar/vector/quantized/spatial/text oracles; graph direction, context, cycles, cancellation, cascade and bounds; snapshots, reopen and rollback. |
| Lifecycle | `tests/index_lifecycle.rs` | Late build, uniqueness refusal, resumable build/drop, publication state and snapshots. |
| Admission | `tests/index_admission.rs`, `graph_admission.rs`, `vector_admission.rs`, `spatial_admission.rs`, `text_admission.rs`, plus quantized admission cases in `index_vector_quantized.rs` | Unknown feature/version/option refusal before mutation, replica fallback, hidden namespace refusal, and malformed authoritative or derived values. |
| Family write faults | `src/index_fault_tests.rs`, `graph_fault_tests.rs`, `vector_fault_tests.rs`, `spatial_fault_tests.rs`, `text_fault_tests.rs`, and the fault module in `quantized_vector_indexes.rs` | Exhaustive key-write boundary failures for family catalog/CRUD/build/drop paths, rollback/reopen, atomic multi-key state and held snapshots. |
| Small query | `tests/query_scalar.rs` and `query_multimodel.rs` with the two named 65,537-match tests skipped | Exact scalar/JSON semantics, retry after cancellation/budget errors, graph/scalar/spatial/text filtering before exact or approximate ranking, deterministic page ties, orphan refusal, diagnostics and one-snapshot mutation behavior. |
| Current source reader | Unit tests in `src/pagewal/current_reader.rs` | Checkpointed and committed-WAL current state, deleted-row absence, interior traversal, corruption/budget refusal, writer exclusion, fingerprint recheck and source preservation. |
| Verifier and rebuild | `tests/index_verifier.rs`, `index_rebuild.rs` | Scratch-free two-way family verification, damage classification, lifecycle incompleteness, source invariance, authoritative-loss refusal, derived rebuild into a separate destination, policy and logical-byte preflight. |

Lean explicitly skips these two tests by full name:

- `scalar_driver_streams_and_pages_more_than_65536_matches_completely`
- `text_and_spatial_drivers_page_more_than_65536_matches_without_a_result_cap`

The family-fault, quantized-fault and current-reader unit groups use name
filters. The runner requires each filtered command to report at least one
executed test. A test rename therefore produces `zero-tests-executed` rather
than a false pass; `status.tsv` still records Cargo's exact exit code separately
from that runner-level failure.

It also does not select `tests/pagewal_transaction_capacity.rs` or the larger
kernel shape/heap ladders such as `kernel/tests/law2_shape.rs`,
`graph_law2_reads.rs`, `vector_laws.rs`, `props_laws.rs`, `bulk.rs`, and
`pack_shape.rs`. Their absence is recorded by the profile definition; a lean
pass must not be described as pagination, scale, memory-shape, or performance
qualification.

## Full and scale profiles

`--profile full` runs `cargo test --release --locked --offline --workspace
--no-fail-fast` for the selected build configuration. It includes the complete
65,537-match pagination tests, 100,000-transaction-capacity fixture, kernel
law/shape tests, and all ordinary regressions currently registered with Cargo.
It does not create the preserved binaries and fixtures needed for compatibility
or process-kill evidence, and it does not collect benchmark RSS, peak disk,
SQLite comparisons, or 10K/100K/1M timing repetitions.

Scale is a separately provisioned Linux profile defined by
`PHASE2_WORKLOAD.md`: fixed `phase2-synthetic-v1` inputs at 10K, 100K and a
representative 1M, plus the separate 1536-dimensional sample. It requires raw
input/query-seed digests, bounded independent oracles, three alternating
trials, durability/cache/batch settings, RSS, logical and allocated peak disk,
and E4/SQLite results. Neither lean nor full substitutes for this profile.

## Workload acceptance map

| `PHASE2_WORKLOAD.md` acceptance row | Lean evidence | Evidence still required outside lean |
|---|---|---|
| Keyspace layout and index catalog | Family admission, lifecycle and malformed-value tests. | Full workspace plus preserved compatibility fixtures and accepted format declarations. |
| Atomic index maintenance | Family fault modules, rollback/reopen and snapshot tests. | Process-kill matrix and full workspace; device durability evidence remains separate. |
| Scalar index | Independent repeated-CRUD oracle and scalar query semantics. | Complete 65,537 pagination test in full; scale costs separately. |
| Graph storage and traversal | Directed/context/property/cycle/cascade/bound tests and graph fault boundaries. | Large degree/store shape and measured reverse-index costs. |
| Exact and quantized vector | Independent exact metrics, approximate shortlist/exact rerank, lifecycle, snapshots, corruption and write faults. | Recall and effort measurements, 32/1536-dimensional scale, disk/write cost. |
| Spatial point index | Bbox/radius/nearest exact refinement, WGS84/dateline validation, lifecycle and faults. | Complete 65,537 pagination in full and scale candidate/refinement costs. |
| Full-text index | Independent BM25/analyzer expectations, presence states, lifecycle, statistics corruption and faults. | Complete 65,537 pagination in full; multilingual and scale cost evidence. |
| Query interface and combined query | Small scalar and multimodel drivers, filtering-before-ranking, stable cursors, retries, diagnostics and snapshot consistency. | Full pagination, runnable consumer example, and scale measurements. |
| Index compatibility | Admission/refusal paths only. | Preserved old/current binaries and immutable fixtures across checkpointed and pending-WAL boundaries. |
| Index recovery | Verified current reader, verifier and source-preserving derived rebuild; authoritative loss refuses publication. | Wider damaged-source corpus and operational recovery tooling acceptance. |
| Hybrid storage | No claim. | Complete 10K/100K/1M benchmark matrix and retained artifacts. |
| Phase 2 acceptance | No claim. | Every row above, accepted commits, limitations, artifacts and release decision. |

## Eight-law evidence map

| Law | Relevant tests in lean/full | Limit of the evidence |
|---|---|---|
| 1. Disk-first | Bounded query budgets/cancellation, bounded BFS, current-reader limits; full adds heap/pack shape tests. | Lean does not measure RSS across a growing corpus. Law 1 needs scale ladders and retained memory evidence. |
| 2. Cost proportional to change | Point/range access, resumable bounded build/drop, bounded traversal and query diagnostics; full adds `law2_shape` and graph read-shape tests. | Correctness counters and small fixtures are not latency-flatness proof. |
| 3. Nothing fallible may delete | Lifecycle publication, exhaustive family write faults, snapshot/rollback, separate-destination rebuild. | Linux process-kill evidence is a separate matrix. |
| 4. Name the sacrifice | Tests expose work/result caps, explicit exact versus quantized methods, reverse adjacency and lifecycle states. | Tradeoff accounting lives in the design/results documents and measured scale artifacts, not a pass count. |
| 5. No corruption unrecoverable | Admission, checksummed current traversal, classified verifier issues, authoritative-loss refusal and source-preserving rebuild. | Some authoritative loss is intentionally unrebuildable; recovery must report it. A clean report cannot prove mutually consistent deletion of both graph copies. |
| 6. Writes do not alter snapshots | Family CRUD/lifecycle/fault tests and mixed-query held snapshots; full includes kernel snapshot regressions. | Wall-clock contention and read-count parity require the separate snapshot/device evidence. |
| 7. Usable target-device ingest | Late build/live CRUD/reopen semantics are exercised. | Usability is a measured bulk/late/live/reopen cost on the target device; lean and full Cargo tests cannot pass this law. |
| 8. Permanent compatibility | Unknown-format refusal-before-mutation is exercised. | Only immutable fixtures from an actually released baseline, tested with preserved old and new binaries, can pass Law 8. Candidate same-build round trips do not. |

## External evidence: explicit artifacts only

The entrypoint does not invoke these drivers. Their binaries and immutable
inputs must be built or selected explicitly; guessing a binary from `target/`
would break provenance.

### Candidate scalar/graph compatibility

Required artifacts are default and retained `phase2_format_fixture` binaries
built from the source under test, a new authorized work directory, binary
hashes, source hash/inventory and `REPORT.json`:

```sh
python3 tools/phase2_format_compat.py \
  --default-bin "$DEFAULT_PHASE2_FORMAT_FIXTURE" \
  --retained-bin "$RETAINED_PHASE2_FORMAT_FIXTURE" \
  --work "$NEW_AUTHORIZED_COMPAT_DIR"
```

### Candidate all-family compatibility and older-engine refusal

Required artifacts are current default/retained
`multimodel_format_fixture` binaries and at least one preserved five-family
`typed_admission_probe` built with a recorded `E4_COMPAT_ENGINE_REVISION` and
supported mask 31. The driver creates v2 mask-63 fixtures and preserves source
inventories:

```sh
python3 tools/multimodel_format_compat.py \
  --default-bin "$DEFAULT_MULTIMODEL_FIXTURE" \
  --retained-bin "$RETAINED_MULTIMODEL_FIXTURE" \
  --older-probe query-drivers-r1 "$PRESERVED_FIVE_FAMILY_PROBE" 31 \
  --work "$NEW_AUTHORIZED_MULTIMODEL_COMPAT_DIR"
```

These are candidate compatibility checks. Law 8 additionally requires
immutable fixtures and binaries from the first declared released format.

### Graph-independent lifecycle fixtures

The separate `phase2_lifecycle_fixture` helper captures masks 1/5/9/17/33
without enabling graph storage, including Ready, nonzero-cursor Building,
partially Dropping and post-drop states. Build both codecs with an explicit
`E4_COMPAT_ENGINE_REVISION`; use the preserved mask-31 admission probe:

```sh
python3 tools/phase2_lifecycle_compat.py \
  --default-bin "$DEFAULT_LIFECYCLE_FIXTURE" \
  --retained-bin "$RETAINED_LIFECYCLE_FIXTURE" \
  --older-probe old-five "$PRESERVED_FIVE_FAMILY_PROBE" 31 \
  --work "$NEW_AUTHORIZED_LIFECYCLE_DIR"
```

The planned matrix contains 80 sources, 160 same-revision cross-build
mutation/resume/readback arms and 160 older admission arms. These are test
definitions, not pass counts; inspect the generated `REPORT.json` before
claiming qualification. The frozen Phase 1 corpus must also pass the separate
`tools/format_reference_compat.py` gate with preserved baseline/current binaries
and the pinned INDEX hash from [FORMAT_BASELINE.md](FORMAT_BASELINE.md).

### Future engine against preserved Phase 2 corpora

`phase2_preserved_compat.py` never generates replacement fixtures. Select one
complete, pinned qualification report, its original corpus, both original
executables and a newly built helper with the same harness version:

```sh
python3 tools/phase2_preserved_compat.py \
  --qualification-report "$PRESERVED_REPORT" \
  --report-sha256 "$PRESERVED_REPORT_SHA256" \
  --corpus "$PRESERVED_CORPUS" \
  --baseline-bin default "$PRESERVED_DEFAULT_HELPER" \
  --baseline-bin retained "$PRESERVED_RETAINED_HELPER" \
  --current-bin "$CURRENT_MATCHING_HELPER" \
  --work "$NEW_AUTHORIZED_REPLAY_DIR"
```

The gate verifies originals read-only, then uses independent fresh copies for
current-writer/preserved-reader and preserved-writer/current-reader handoffs at
checkpointed and pending-WAL boundaries. It checks source and binary hashes.
This is not an old writer modifying an already upgraded copy. A v2 multimodel
helper cannot substitute for the archived v1 harness. Candidate corpora remain
labelled candidate until a release baseline is actually declared. Native
qualification of this replay driver is pending.

### Process-kill qualification

Required artifacts are default and retained `phase2_crash_probe` binaries
from the same captured source revision and a new authorized work directory.
The standard four delays produce 42 cases per binary after quantized lifecycle
coverage, 84 total:

```sh
python3 tools/phase2_crash_qualify.py \
  --default-bin "$DEFAULT_CRASH_PROBE" \
  --retained-bin "$RETAINED_CRASH_PROBE" \
  --work-dir "$NEW_AUTHORIZED_CRASH_DIR"
```

Preserve both binaries, SHA-256 hashes, captured source inventory, raw child
logs, fixture inventories and `phase2-crash-report.json`.

### Multimodel scale benchmark

Required artifacts are an explicitly built `phase2_multimodel_bench` binary,
its hash/source inventory, a new authorized root, raw logs and the generated
report. State the build configuration explicitly; comparisons of different
builds must use distinct roots. R7 uses the retained four-feature configuration;
its performance results do not qualify the no-feature build:

```sh
python3 tools/phase2_multimodel_bench.py \
  --bin "$EXPLICIT_MULTIMODEL_BENCH_BINARY" \
  --root "$NEW_AUTHORIZED_BENCH_DIR" \
  --sizes 10000,100000,1000000 \
  --reader-modes none,batch,short,held \
  --trials 3 \
  --dimension 32
```

Reader lifetimes and completed/refused interpretation are specified in
[PHASE2_MULTIMODEL_PROTOCOL.md](PHASE2_MULTIMODEL_PROTOCOL.md). The historical
`short` mode holds a whole round; `batch` holds one update commit per round.

The separate quantized recall/effort runner is a direct binary interface and
must likewise use an explicit binary and new directory:

```sh
"$EXPLICIT_QUANTIZED_BENCH_BINARY" \
  ROWS DIMENSION "$NEW_AUTHORIZED_QUANTIZED_DIR" K EF_CSV
```

For the workload acceptance matrix, retain input and query-seed digests,
exact-oracle output, method/effort/recall, binary and source hashes, stdout and
stderr, RSS samples, peak and final logical/allocated disk, and the SQLite
configuration/results. A Cargo test pass or an unproven binary path cannot
replace those artifacts.

### Read-only query optimization comparison

Build `phase2_query_replay` against captured baseline and candidate sources,
with matching retained features and distinct recorded engine revisions. Select
completed R7 reports with their pinned SHA-256 hashes:

```sh
python3 tools/phase2_query_replay.py \
  --baseline-bin "$BASELINE_REPLAY" \
  --candidate-bin "$CANDIDATE_REPLAY" \
  --report "$R7_10K_REPORT" "$R7_10K_SHA256" \
  --report "$R7_100K_REPORT" "$R7_100K_SHA256" \
  --report "$R7_1536_REPORT" "$R7_1536_SHA256" \
  --report "$R7_1M_REPORT" "$R7_1M_SHA256" \
  --work "$NEW_AUTHORIZED_QUERY_REPLAY_DIR"
```

The runner selects completed no-reader E4/SQLite databases after three CRUD
rounds, checks independent primary-scan oracles and cross-engine answers, and
hashes database files before/after each process. It refuses pending WAL.
Three alternating process trials per engine each run three seeds five times.
Compare paired process medians; repeated samples within one process are not
independent trials. This measures warm post-CRUD text/scalar/vector queries,
not cold reads, ingestion, disk growth or complete CRUD cost. SQLite uses a
bounded heap for top-k here; historical R7 query wrappers repeatedly sorted a
small vector. Preserve that distinction when presenting results. A PASS means
correct capture and answers; it does not itself accept the optimization.

### Full same-copy rollback cycle (native ARM preparation passed)

The pairwise preserved gate above does not cover rollback writing. The new
`phase2_rollback_compat.py` requires helpers advertising `rollback_cycle_version:1`.
It never generates fixtures. Use a complete pinned report from those helpers,
the original corpus, and explicit default/retained executables:

```sh
python3 tools/phase2_rollback_compat.py \
  --qualification-report "$PRESERVED_REPORT" \
  --report-sha256 "$PRESERVED_REPORT_SHA256" \
  --corpus "$PRESERVED_CORPUS" \
  --baseline-bin default "$PRESERVED_DEFAULT_HELPER" \
  --baseline-bin retained "$PRESERVED_RETAINED_HELPER" \
  --comparison-bin default "$PRESERVED_DEFAULT_HELPER" \
  --comparison-bin retained "$PRESERVED_RETAINED_HELPER" \
  --qualification-kind cross-build \
  --work "$NEW_AUTHORIZED_ROLLBACK_DIR"
```

Cross-build mode uses the exact two pinned binaries and prepares the first
baseline. Each fixture's opposite build writes Original to Updated; the
preserved originating build writes that SAME copy to Roundtrip; the comparison
build reads and reopens it for writing, then the preserved build reads again.
Both transitions cover checkpointed and pending-WAL boundaries (four pairs).
All originals and input executables are rehashed. Existing candidate helpers
without the capability cannot supply this evidence.

For a later engine, use `--qualification-kind cross-revision`, supply the two
new comparison executables and `--comparison-build MANIFEST SHA256`. This pinned
JSON must have format `phase2-rollback-build-v1`, a nonempty `source_files_sha256`
map including `Cargo.lock`, and a `binaries` map with exact `default`/`retained`
SHA-256 and parsed `--version` values. Capture it from the actual build, not from
expected output. Each group must represent one recorded engine revision; the
groups must differ, with matching build/creation profiles. Source provenance
cannot be inferred from a version label alone.

The rollback driver's pure provenance regression tests run without database
files or native binaries:

```sh
python3 -m unittest discover -s tools -p test_phase2_rollback_provenance.py -v
```

They reject mixed/criss-cross revision groups, mismatched helper protocols,
missing cycle capability and mislabelled build profiles. They do not replace
native same-copy rollback execution or source-preservation checks.

The first native ARM capture passes 416 full cycles over 104 sources. Its actual
corpora, executables, source and offline dependencies are archived on scratch;
all 2,029 member hashes verify and a relocated Linux replay passes the same 416
cycles without regenerating fixtures. See [rollback results](PHASE2_ROLLBACK_RESULTS.md)
for exact provenance, archive hash, limits and raw reports. This closes native
cross-build/portability preparation; a newer engine still needs cross-revision
qualification against that preserved baseline.
