# Phase 2 text cost review

This is a bounded cost review of the 10K/32-dimensional R4 workload. It does
not authorize an optimization, change the persisted format, or establish how
either cost scales at 100K. Preserve the 100K baseline before implementing
either candidate below.

Update 2026-09-17: the three-trial **100K no-reader baseline** is now preserved
in `phase2-evidence/multimodel-r7-100k-partial-11/`. All nine no-reader arms
completed; the full reader-mode matrix is still running on the unchanged
captured binary. E4 atomic / SQLite medians are 139.557 / 108.666 seconds for
three CRUD rounds and 228.606 / 67.823 milliseconds for text+active+vector.
The complete-case baseline requirement is satisfied for a bounded query-only
experiment. The full 100K capture and all larger comparisons remain required.
Only the strict scalar-equality membership candidate below is being attempted;
metadata microbatching remains unimplemented. Keep or revert it from native
correctness and measured query benefit, with no persisted-format changes.

## Captured measurements

The captured R4 medians report:

- atomic E4 text late-build: 1.865 seconds;
- atomic SQLite FTS5 population: 0.029 seconds;
- E4 exact-vector cosine, k=10: 11.77 milliseconds;
- E4 text + active + exact-vector, k=10: 12.77 milliseconds;
- SQLite text + active + exact-vector: 3.14 milliseconds.

These values and their trial ranges are recorded in
`docs/PHASE2_MULTIMODEL_R4_RESULTS.md:25-49`. SQLite has different storage and
FTS machinery, so the ratio identifies a cost problem; it does not identify a
single equivalent operation or predict an attainable speedup.

The preserved R4 report records this E4 work for text + active + vector:

```text
candidates=3750 primary_reads=3750 text_postings=3751
vector_locators=2499 vector_sidecars=2499 vector_lanes=79968
```

See
`docs/phase2-evidence/quant-compat-crash-r1/multimodel-r4-10k/report.json:170-174`.
Those counters are measured facts from the captured R4 binary. Current query
source also performs an authoritative primary point-get for every emitted
winner (`src/query.rs:2715-2728`); a new run can therefore add up to ten
winner reads to this k=10 case.

## Text late-build cost

The current build scan retains only an entity ID for a text row, discards the
row value, and later point-gets the same primary row in `build_document`
(`src/indexes.rs:762-821`, `src/text_indexes.rs:346-365`). It then calls the
general live-transition path. For each document that has not contributed yet,
that path:

1. reads its norm and the shared corpus statistic;
2. for every distinct term, reads and writes the posting and reads and writes
   the shared document frequency;
3. writes the norm and rewrites the shared corpus statistic.

The relevant operations are in `src/text_indexes.rs:197-323`. This is source
inspection, not a profiler result.

The R4 generator emits six distinct terms per document and only repeats the
first term (`src/bin/phase2_multimodel_bench.rs:171-178`). Therefore the 10K
fixture is estimated to perform 60K posting transitions, 60K document-frequency
read-modify-writes, 10K corpus-stat read-modify-writes, 10K norm transitions,
and 10K duplicate primary point-gets. Counting each get/put separately gives
about 290K tree operations in addition to the primary range scan and analyzer
work. This count is derived from the fixture and source; R4 did not measure
individual storage operations or attribute elapsed time among them.

### Unimplemented candidate: bounded metadata microbatching

Keep the existing posting, norm, term-stat and corpus-stat keys and values.
During late-build only, process rows sequentially while accumulating signed
document-frequency and corpus deltas. Read and write each touched term statistic
and the corpus statistic once per microbatch rather than once per document.
Continue validating each norm and posting, and retain the existing transition
path for a row that already contributed while the index was BUILDING.

The unique-term accumulator must have an explicit configured cap independent
of corpus size. Flush a capped sub-batch inside the enclosing build transaction
when the cap is reached; do not retain the entire corpus vocabulary or a whole
collection of token maps. On this fixture, an ideal 256-row step touches only
16 vocabulary terms, reducing the estimated DF updates from 60K to at most
640 term-stat updates across 40 steps and corpus updates from 10K to 40.
Posting and norm writes remain required.

Publication semantics must remain unchanged. Every posting, norm and aggregate
stat update for a step must succeed in the same transaction before advancing
the BUILDING cursor. The descriptor must become READY only after the final
complete step; resumable commits must never publish READY with deferred
metadata outstanding (`src/indexes.rs:794-830`). Fault injection and live-write
interleavings are prerequisites for accepting this path.

## Text + active + vector query cost

The benchmark explicitly selects the text filter as driver, applies
`active = true`, and ranks with the exact vector index
(`src/bin/phase2_multimodel_bench.rs:1015-1039`). The text cursor streams the
3,750 `flood` postings (`src/query.rs:1636-1721`). Because the scalar predicate
did not drive the query, current code evaluates it by fetching the primary row
and running the targeted full-row-validating field reader
(`src/query.rs:1925-1951`, `src/query.rs:2320-2325`). The 2,499 survivors each
require a locator lookup, authoritative vector-sidecar lookup and 32-lane f64
score (`src/query.rs:2152-2195`).

The counters show that the mixed query scores roughly one quarter of the 10K
vectors scanned by the standalone exact-vector query, yet its median is one
millisecond slower. This strongly identifies text traversal plus scalar
refinement reads as material excess work. It does not separate primary lookup,
dense-row validation, locator lookup and vector math into measured time shares.

### Unimplemented candidate: strict scalar-equality posting membership

For a READY scalar equality predicate whose encoded key fully determines the
answer, point-test the existing scalar posting
`(index, encoded value, entity sequence)` instead of fetching and walking the
primary row. The existing format already has this key shape
(`src/indexes.rs:71-75`). Validate the empty posting value, charge the scalar
posting probe, and leave the text stream and exact-vector ranking unchanged.
For the R4 query this would replace 3,750 full primary-row reads with 3,750
small scalar-index membership probes. The current winner existence point-get
must remain, including for ID-only projection.

Admission must use the existing strict scalar encoder. Integer equality must
remain exact without f64 conversion, Real must accept only finite F64 input,
and Bool/Text must not coerce. Missing and null currently share the scalar
nullish key, while combined-query predicates distinguish them; therefore this
fast path cannot decide `IsMissing` versus `IsNull` and those predicates must
keep authoritative row evaluation. Any other predicate whose persisted key
does not prove the requested semantics also stays on the row path.

This optimization relies on the same typed atomic-write invariant as a scalar
index used as the candidate driver. It is not full integrity verification;
the verifier/rebuild workflow remains responsible for missing or stale index
entries. Measure it only after preserving the 100K baseline, with answer,
pagination, work-counter, snapshot and corruption tests unchanged.
