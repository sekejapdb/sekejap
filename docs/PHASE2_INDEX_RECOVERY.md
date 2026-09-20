# Phase 2 indexed verification and recovery design

Status: implementation and acceptance plan, not a released recovery format.
This design extends the source-preserving rules in
[RECOVERY_CONTRACT.md](RECOVERY_CONTRACT.md). It does not authorize in-place
repair or automatic publication of a rebuilt database.

The implementation anchors audited for this plan are (paths below are the
layout restructure at `f5e4c7e`'s current locations; the names in the next
sentence are the pre-restructure ones this design was written against)
[`recovery.rs`](../src/store/recovery.rs),
[`pagewal/repair.rs`](../src/store/pagewal/repair.rs), `CollectionRecovery` in
[`collections/mod.rs`](../src/collections/mod.rs), the common catalog in
[`catalog.rs`](../src/collections/catalog.rs), and the graph, vector and point
codecs in [`graph/mod.rs`](../src/index/graph/mod.rs),
[`vector/exact.rs`, `vector/quantized.rs`](../src/index/vector/) and
[`spatial/point.rs`, `spatial/geometry_index.rs`](../src/index/spatial/).
Text keyspaces remain the
candidate specified by [PHASE2_TEXT_DESIGN.md](PHASE2_TEXT_DESIGN.md) until that
family's persisted implementation is stable.

## Existing boundary

The current low-level `pagewal::repair::recover_to` opens the source read-only,
holds its writer lock, overlays an independently readable committed WAL, writes
to a new destination, reopens that destination, compares every recovered
current value with the source, rereads its candidate spool, fingerprints source
data and WAL before and after, and writes its completion marker last. When it
cannot establish the current root, records found by rootless scanning are
candidates only. They are not safe evidence of current membership because an
older leaf can contain a value deleted by a later committed tree.

`recovery::recover_typed_candidates` likewise preserves complete raw row bytes
and discovers replicated layouts without the root, but explicitly labels every
exported row candidate membership. `CollectionRecovery` can decode inline
dense-v3 values and stable entity IDs; it deliberately leaves vector-bearing
rows raw when authoritative `0x60` sidecars cannot be resolved. These tools are
evidence sources. Neither currently verifies or reconstructs Phase 2 indexes.

Indexed recovery must retain that distinction:

1. **Verify** reads a quiescent source and produces a report. It writes no
   database records.
2. **Rebuild derived data** writes a new, non-overlapping destination from a
   verified current source. It never edits, renames, truncates, checkpoints or
   normalizes the source.
3. **Candidate export** remains the only result when current membership or an
   authoritative dependency cannot be established.
4. **Publication** is a later operator action after independent destination
   reopen and verification. Recovery never swaps the destination over source.

## Authority map

The verifier must classify bytes by meaning before deciding that rebuilding is
possible. A checksum-valid derived entry is still checked against its source.

| Records | Role | Recovery rule |
|---|---|---|
| `00` COLL/layout headers, `01` collection descriptors, layout replicas | Authoritative interpretation and allocator metadata | Use agreeing valid replicas. Conflicting valid replicas are corruption, not a vote. All-copy loss blocks a certified typed replacement. |
| `02` entity sequence allocators | Authoritative stable-identity history | Preserve a verified allocator. `max(live ID)+1` can reuse an ID deleted above that maximum, so it is not an equivalent reconstruction. Without the allocator, keep the result read-only/candidate unless an external identity ledger proves a safe value. |
| `03` index descriptors | Authoritative declaration: index ID, collection, field, family, encoding and lifecycle | A READY derived index can be rebuilt only from a verified descriptor. Missing definitions or valid conflicts cannot be inferred from postings. |
| `04/05/11` registry, collection and name mappings | Derived catalog cross-references | Rebuild from verified descriptors, while checking header count/allocator and exact name uniqueness. Missing descriptors cannot be repaired from these mappings alone. |
| `10` collection-name mapping | Derived from verified collection descriptors | Rebuild only after checking exact UTF-8 names and uniqueness. |
| `20` external-key mapping | Derived from the hidden external key in a verified `0x40` row | Verify both directions by point lookup; rebuild without changing the entity ID or row bytes. |
| `40` dense-v3 entity row | Authoritative primary document, stable ID, layout reference and vector-presence state | Preserve exact key/value bytes. A missing/damaged row is data loss. An old leaf candidate must not resurrect it. |
| `60` vector sidecar | Authoritative vector lanes for the row's immutable physical ordinal | Preserve exact bytes after dimension/finiteness checks. A locator has no vector values and cannot reconstruct a lost sidecar. |
| `70` scalar posting | Derived | Recompute canonical scalar key from each verified primary row. Null and missing intentionally share the `00` posting bucket. |
| `71` graph primary edge | Authoritative tuple and binary-JSON object properties | Preserve exact valid properties and identity. A reverse marker cannot reconstruct properties or prove a primary edge once both records are absent. |
| `72` graph reverse marker | Derived | Rebuild as an empty marker for each verified `0x71` edge. |
| `73` exact-vector locator | Derived | Recompute from the row's immutable layout/ordinal and require the authoritative `0x60` sidecar. |
| `74` spatial point posting | Derived | Recompute Hilbert-16 key and exact two-`f64` value from the inline Point. |
| proposed `75/76/77/78` text posting/norm/DF/corpus | Derived | Recompute with the descriptor-pinned analyzer and ranking versions. This row remains conditional until the text family implementation and descriptor bytes stabilize. |
| `06/07/12` graph header, name descriptors and lookups | Authoritative graph type/context meaning and allocation history; lookup is derived | Preserve agreeing descriptors and IDs. Numeric edge IDs do not reveal lost names. Rebuild lookups from descriptors. |

The common logical feature mask says which parsers are required; it is not a
backup inventory. A set feature with no surviving descriptor does not reveal
the missing index name, field or options. Conversely, an intact descriptor or
family entry without its required feature is corruption and cannot be hidden by
clearing the flag.

## Strict streaming verifier

The verifier operates on a committed read-only PageWalStore-format view built
from the recovery source handles when its root, page structure and WAL overlay
are trustworthy. It must not call the ordinary raw writer opener, because WAL
normalization or coordination-file creation would violate source preservation.
It also does not call typed `Database::open`, because catalog damage is one of
the inputs it must diagnose. It uses the same canonical parsers as ordinary
admission, exposed as read-only family codecs rather than copied ad hoc.

The run has four passes.

### 1. Format and metadata admission

- Acquire the source writer lock and fingerprint `data`, `wal` and persistent
  metadata before reading. Refuse concurrent writers.
- Verify page/WAL CRCs, page identities, committed extent and current root.
- Resolve replicated COLL headers, collection/layout descriptors, common index
  descriptors and graph dictionaries. One damaged or missing copy may fall
  back to another valid copy. Any two differing valid copies are a conflict.
- Validate allocators, counts, feature dependencies, family versions, current
  layouts, mapping identities, name uniqueness and lifecycle states.
- Discover every descriptor replica and family namespace from the current tree,
  including orphans; do not rely solely on a possibly damaged registry.

Any checksum-valid unknown COLL/index/graph envelope, required feature, index
family, encoding version, descriptor option, analyzer, Unicode table, CRS,
grid or metric returns **Unsupported**. Typed verification and rebuild stop
before database writes. Bad CRC on an alleged future magic is damage and may
fall back to another replica; it is not proof of a future format. An unknown
current key tag is preserved by raw salvage but blocks a certified typed copy
unless a supported feature definition proves it opaque.

### 2. Authoritative membership and dependency verification

Stream `0x40` rows in stable ID order. For one row at a time:

- parse its canonical entity key, resolve an agreeing immutable layout and
  validate the complete dense-v3 value;
- point-check its `0x20` external-key mapping, then scan `0x20` separately and
  point-check every mapping back to exactly one row;
- for each present vector, validate the exact `0x60` key, byte length and finite
  lanes against the historical layout; separately scan `0x60` to reject orphan,
  wrong-ordinal and absent/null sidecars;
- derive the expected scalar, vector-locator, spatial and text records for each
  verified descriptor applying to the row.

Stream graph dictionaries and primary `0x71` edges separately. Validate
canonical entity/type/context IDs, both existing endpoints, descriptor identity,
property version, binary JSON object form and bounds. Point-check the exact
empty reverse marker. A second `0x72` scan point-checks an authoritative primary
for every reverse marker. This detects a missing reverse and a reverse without
primary. It cannot detect an edge for which both primary and reverse were
removed with valid tree updates; only an external manifest or operation log can
distinguish that state from a legitimate unlink.

If a page or structural extent containing possible authoritative records is
unreadable, the verifier reports an unknown affected extent and does not certify
primary completeness. Keyspace locality alone is insufficient unless
authenticated parent/sibling bounds prove the damaged range. The current
rootless reader does not provide that proof.

### 3. Derived comparison

Verification does not need a corpus-sized expected-index store. It checks every
derived family in both directions:

- While streaming each authoritative primary row, derive each exact posting,
  locator, norm and mapping that row requires and point-read that key from the
  current source. Absence is a missing derived record; unequal canonical bytes
  are a mismatched derived record.
- Separately stream every actual derived prefix. Decode its descriptor and
  entity identity, point-read the authoritative primary row (and vector
  sidecar where applicable), derive that one expected record again and compare
  it with the actual key/value. A missing primary is an orphan; a primary that
  does not derive the record makes it extra; malformed/noncanonical bytes are
  derived corruption.

This deliberately trades I/O and CPU for bounded space. The reverse scans can
repeat bounded primary reads and, for text, repeat tokenization once per actual
posting. A fixed-size cache may reduce that cost, but correctness and memory
must not depend on retaining all documents or expected postings.

Scalar verification also checks adjacent unique-index values for duplicate
entities, excluding the shared null/missing bucket. Vector locator verification
checks the layout ID and ordinal and then the authoritative sidecar. Spatial
verification checks finite WGS84 ranges, exact `f64` bytes and Hilbert-key
agreement.

For text v1, a per-document term map is the largest in-memory object in the
primary pass; expected `0x75` term frequencies and the `0x76` document length
are point-checked immediately. In the actual term-ordered posting scan, each
posting TF is independently checked against its primary document and one
checked counter accumulates DF until the term changes. At each boundary the
exact `0x77` statistic is point-checked. The norm scan independently checks
each norm against its primary and accumulates document count and total length
with O(1) counters, then point-checks `0x78`. Separate `0x77/78` scans detect
orphan statistics: each DF key must have a nonempty posting prefix, and each
corpus key must name a live READY text descriptor. Empty text contributes a
zero norm and one corpus document; null/missing contributes nothing. Stored
terms are validated under the descriptor-pinned analyzer and must not be
re-tokenized as analyzer input.

### 4. Report and source recheck

The report streams every issue with key/index/entity identity when known, page
and slot evidence, class, dependency and disposition. Counters and a bounded
preview stay in RAM. Before marking the report complete, reread its frames and
checksums, compare source fingerprints, sync report files/directories and write
the completion marker last.

Memory is bounded by the configured page cache, metadata caps, one decoded
document and its bounded token map, one vector/edge, O(1) streaming statistic
counters, an optional fixed-size primary cache, and at most 256 entity IDs for
a rebuild batch. No vector of entities, postings, terms, edges or damage
records may grow with the corpus. Report, destination and total work/byte
budgets are explicit; exhaustion returns an incomplete error.

## Source-preserving derived rebuild

Rebuild is allowed only after the verifier proves current authoritative
membership and all interpretation dependencies. The destination must not exist,
alias, contain, be contained by, hardlink to or symlink into the source. It is
created under an incomplete marker and uses a separate PageWalStore.

1. Copy supported authoritative metadata and exact authoritative logical bytes:
   feature headers, collection/layout identities, verified common index
   descriptors, `0x40` rows, verified `0x60` sidecars, graph dictionaries and
   `0x71` primary edges. Preserve stable entity/index/name IDs, exact dense rows,
   vector bytes, edge properties, timestamp values and known safe allocators.
   Do not decode and reinsert entities through ordinary CRUD.
2. Rebuild `04/05/10/11/12/20/72` mappings and all READY `70/73/74` and
   finalized text records from authoritative bytes. Do not copy source derived
   records. Existing family maintenance accumulates text DF/corpus statistics
   in the destination as records are inserted; verification does not need an
   expected-index scratch database.
3. Preserve lifecycle meaning. READY descriptors are published READY only in
   the destination transaction containing the last verified batch. BUILDING
   descriptors are retained BUILDING and reset to cursor zero with no derived
   entries, so normal bounded build can restart. DROPPING descriptors remain
   DROPPING with no family entries, ready for bounded catalog finalization.
   Recovery must not silently publish an unfinished index or resurrect one
   being dropped.
4. Reopen the destination independently through typed admission and structural
   verification. Compare every authoritative key/value byte with the verified
   source, regenerate and merge-check every derived family, verify counts and
   allocators, and run fixed independent query oracles.
5. Sync the destination, report and parent directory; fingerprint the source
   again; write `COMPLETE` last. A failed write, sync, verification or process
   leaves a new incomplete destination and the unchanged source.

Derived damage alone therefore has a repair path. These conditions do not:

- a missing/damaged authoritative entity or vector sidecar;
- a missing/malformed graph primary property's bytes;
- graph type/context name loss or conflicting valid metadata;
- unproved current membership after root/interior/WAL damage;
- an unknown intact format;
- a lost stable-ID allocator without independent no-reuse evidence.

For those cases, preserve verified records and candidates in separate evidence
artifacts, name the limitation, and do not emit a certified writable database.

## Damage classes and blast radius

| Class | Example | Result |
|---|---|---|
| Recoverable metadata replica damage | One/two bad descriptor copies, intact agreeing copy | Use the valid copy; rebuild destination replicas; report damaged copies. |
| Metadata dependency loss | All index/layout/graph-name copies absent, valid conflict, allocator loss | Preserve raw data; block affected typed interpretation or writable certification. |
| Derived-only semantic damage | Missing/extra scalar posting, bad vector locator, wrong point Hilbert, text counter mismatch, missing reverse | Exact mismatch report; rebuild from authoritative data in a new destination. |
| Authoritative key loss | Missing entity, vector sidecar, graph primary with surviving reverse | Known loss when identity is independently visible; no fabrication. |
| Authoritative value loss | Truncated vector, malformed edge properties, undecodable dense row | Omit/quarantine the whole dependent value and block certified replacement. |
| Unknown physical extent | Bad page/slot bounds or unusable root/WAL region | Report physical extent; current completeness unproved, so candidate export only. |
| Undetectable logical omission without oracle | Both graph directions deleted; primary row and all references deleted consistently | Indistinguishable from legitimate deletion. State this limit; compare retained acceptance fixtures/backups when available. |
| Unsupported intact encoding | Future family/version/option/feature | Refuse typed interpretation without source mutation; permit format-agnostic raw preservation only. |

A failed page remains at least page-wide uncertainty. If it can contain mixed
primary and derived keys, rebuilding the derived families does not make the
destination complete. Zero named lost keys never means zero loss when the
report contains unknown extents.

## When ordinary typed admission is blocked

A low-level read-only recovery view can sometimes still establish the committed
PageWalStore tree and stream current key/value bytes even when `Database::open`
refuses a supported catalog inconsistency. The verifier may then parse known
metadata replicas, primary rows and families directly and rebuild only after
all invariants above pass. This covers, for example, a missing scalar posting,
corrupt registry mapping, or missing graph reverse marker.

If raw PageWalStore open or committed-root traversal fails, run the existing
source-preserving `recover_to`/candidate tools first. Only records that
`recover_to` independently classifies as verified current may seed a certified
rebuild. Rootless `candidates.bin`, `records.raw` and decoded JSONL remain
evidence; they are never promoted by newest-generation, highest-page or
first-copy heuristics. A future-format refusal remains a refusal even when raw
bytes are readable.

## Executable acceptance plan

The implementation should expose commands equivalent to:

```text
recover indexes verify SOURCE REPORT_DIR --memory BYTES --work UNITS --report-bytes BYTES
recover indexes rebuild SOURCE NEW_DEST REPORT_DIR --memory BYTES --work UNITS
recover indexes verify NEW_DEST NEW_REPORT --strict
```

Each test starts from a deterministic two-collection fixture with independently
recorded source hashes, collection/entity/index IDs, layout bytes, external
keys, vector bytes, graph dictionaries and primary edges, and complete expected
derived records/query answers. Mutations operate only on copied fixtures.

1. **Clean oracle:** verify zero mismatches; rebuild; compare every authoritative
   logical byte and all scalar/vector/spatial/text/graph answers; reopen writer
   and snapshot. Run with checkpointed and committed-WAL-pending sources.
2. **Catalog matrix:** damage each of three replicas singly and in pairs; remove
   all copies; create two checksum-valid conflicting copies; corrupt registry,
   name and collection mappings; alter header counts/allocators; inject intact
   future family/version/options/features. Refusals preserve byte-for-byte file
   inventories and an uncommitted WAL tail.
3. **Scalar:** remove one expected posting, add an orphan, alter canonical key,
   make a nonempty value, and create a READY unique contradiction. Verification
   identifies exact index/entity where possible; derived rebuild restores the
   independent posting set.
4. **Vector:** remove/add/change `0x73` locators and wrong layout/ordinal values;
   separately remove, truncate or inject a non-finite `0x60` sidecar. Locator
   cases rebuild. Sidecar cases are authoritative loss and never use locator
   bytes as a replacement.
5. **Spatial:** remove/add/move `0x74` postings; inject short, NaN, out-of-range
   and Hilbert-mismatched values; exercise `-180/+180`. Rebuild output equals
   the independent 16-bit-Hilbert and exact-`f64` oracle.
6. **Text:** independently alter every `0x75/76/77/78` class: missing/extra
   posting, wrong TF, missing/wrong norm, wrong DF, document/total-token count
   underflow and overflow. Include empty, punctuation-only, null, missing,
   Unicode expansion and repeated-term documents. Rebuild statistics and BM25
   answers must equal a separate analyzer oracle.
7. **Graph:** remove/duplicate/nonempty reverse markers and prove rebuild from
   primaries. Remove a primary while leaving reverse and report authoritative
   loss. Corrupt properties, endpoints and dictionaries and refuse fabrication.
   Delete both directions in a copied fixture and assert the production verifier
   does not claim it can infer the edge; the external fixture oracle records the
   otherwise-undetectable loss.
8. **Primary membership:** damage entity leaf, layout, vector sidecar, root,
   interior page and committed WAL regions. Assert exact known-key versus unknown
   extent classification and that no candidate deleted row is resurrected.
9. **Failure boundaries:** inject failure and process exit at every report,
   destination put/delete, commit, sync, reopen, verification and
   completion-marker boundary. Source hashes never change; no failed destination
   has `COMPLETE`; retry uses a fresh destination or verified resumable stage.
10. **Bounds:** scale clean and all-pages-damaged inputs under a hard memory
    limit. Measure peak RSS, report/final/peak allocated bytes and I/O. Prove
    memory depends on configured caches, one document/vector/edge and 256 IDs,
    not entity, posting, term, edge or loss count.
11. **Mutation checks:** disable independently the reverse scan, extra-entry
    scan, source fingerprint recheck, destination authoritative-byte compare,
    text-stat recomputation and completion-last rule. Each mutation must make a
    named acceptance arm fail.

Reports must retain commands, binary/source revision, feature set, fixture
manifest, before/after hashes, exact damage recipe, counts by damage class,
unknown extents, resource peaks and destination verification result. Passing a
derived-family test is not evidence that authoritative graph/vector loss is
recoverable.
