# E4 recovery contract — R1, 2026-09-10

This specifies the foundation gate; individual implementation claims require
passing evidence in RECOVERY_MATRIX.json and the tracker task notes. P1/P2
size results do not imply this gate passed. No legacy E3 documents are inputs.

## Fault model and honest outcomes

Supported target: accidental byte/page damage, truncation, crossed pages,
malformed records, interrupted writes/publication and explicit I/O failures.
Validate CRC, page identity, kind, bounds, record structure and dependencies
before serving values. CRC32C is accidental-damage detection, not proof against
an adversary who rewrites bytes and checksums. The source is quiescent during
repair; concurrent source writers are outside the repair protocol.

Four outcomes must remain distinct:

1. **Verified current data**: integrity and membership in a trustworthy
   committed state are established, including applicable committed WAL.
2. **Recovered candidate**: bytes are valid but current membership/version is
   uncertain (for example, rootless scanning after deletes). Never silently
   label this as current or automatically replace a database with it.
3. **Known affected key**: a verified cell names a key whose value/dependency
   cannot be verified. Omit the whole value, retain source evidence and report
   the key and reason. Never serve a partial JSON/vector value as complete.
4. **Unknown affected extent**: page/slot/schema/generation damage prevents an
   exact key/count claim. Report physical extent and any independently proven
   bounds; do not invent row counts or trust damaged kind bytes.

Every independently recoverable unaffected record must remain accessible in
its correct outcome class. Derived trees/indexes can be reconstructed. A lost
source value cannot be reconstructed without valid redundancy. Positional
values cannot reveal their original field names or exact layout by inference.
Loss of every source/replica must be explicitly reported, never fabricated.

## Blast radius and required evidence

| Unit | Required containment / independent recovery evidence |
|---|---|
| Compact cell / typed record | Validate lengths, tags, states and schema; unknown extent when boundaries cannot be trusted. Per-page CRC presently makes a failed page the atomic trusted unit, not one field. |
| Leaf payload/slot/header | Quarantine failed page; preserve verified unrelated leaves. A changed kind byte cannot erase a loss report. Bounds from authenticated structure only. |
| Interior/root/meta | Rebuild derived structure from trustworthy current-membership evidence. Rootless candidates alone are not proof against stale resurrection. |
| Layout descriptor | Use independently discoverable, checksum/identity-checked redundant descriptors. Damage to ancestors cannot prevent discovery. All-copy loss is a named schema dependency loss; preserve raw records. |
| Overflow chain | Verify every page, identity, length, chain bounds and whole-value CRC. Lose/quarantine only dependent record(s); continue unrelated salvage. Never fall back silently to an older intact version. |
| WAL | Preserve full original; replay independently valid committed regions only. Separate torn tail, corrupt/unknown region and committed loss. Do not infer current membership from obsolete pages after a delete. |
| Freelist | Reconstruct/ignore damaged allocator hints conservatively; never allocate a currently reachable or snapshot-pinned page. |
| Whole-file tail / unreadable extent | Name byte/page extent and uncertainty. No claim of zero lost rows from untrusted header contents. |
| Interrupted repair / destination failure | Source remains byte-identical; incomplete destination is never advertised as verified. Retry preserves previous evidence. |

## Repair protocol and report

The public safe path is source -> new destination. It never modifies the
source data, WAL, freelist or layouts. Destination must not alias/contain the
source; a pre-existing destination is refused. An explicit later publication
step requires an independently reopened/verified complete result and durable
barriers. A failed write/sync/verification/rename must not destroy the only
copy. In-place low-level recovery is not the safe user-facing salvage API.

The report has a version, source/destination identity, physical format,
completion state, current-vs-candidate classification, rows recovered,
known affected records, unknown damaged extents, WAL recovery summary,
missing layouts, verification evidence, and artifact paths. Per-loss details
stream to an append-only report file; only bounded previews/counters reside
in memory. Hex/base64 keys preserve arbitrary binary identity. Zero reported
known keys must never mean zero loss when unknown extents exist.

Phases: inspect -> preserve source evidence -> stream verified candidates ->
resolve identity/schema/version dependencies -> build replacement -> reopen
independently -> verify data/counts/structure -> durable completion marker.
Expose progress without requiring the damaged root. Retrying an interrupted
attempt must either resume a verified stage or start a new destination; never
silently trust a partial output. Publication remains separate from salvage.

## Resource and performance contract

- Source scans may cost O(source bytes); repair is an administrative operation.
  Normal writes/schema changes remain proportional to changed data.
- Buffers, sort arena, page cache, schema handling and loss previews have
  declared caps. Many losses cannot allocate a collection proportional to the
  database; spill loss/candidate catalogs to disk. Reject budgets below a
  stated minimum instead of secretly exceeding them.
- Size costs of redundant schema/current-membership evidence are measured.
  Checkpoint is not VACUUM; file shrinking is a separate verified replacement.
- Benchmark data/fault fixtures/results live on scratch. Existing 20M artifacts
  are read-only evidence; destructive tests operate on independent small copies.

## Seven-law executable acceptance matrix

RECOVERY_MATRIX.json maps each fault class to a deterministic test or a named
pending test. A pending test is not passing evidence. Tests compare against
independent clean fixtures/operation logs, including known deleted keys.

1. **RAM**: scaling stores and all-pages-damaged inputs under a hard budget;
   allocation peaks and disk-spilled report checked, not just final RSS.
2. **Work**: read/write counters vs changes and source size; no repair design
   may add a table scan to ordinary writes or schema version creation.
3. **Preservation**: hash source before/after every repair outcome; inject
   errors at every source/destination I/O and publication phase.
4. **Costs**: record time, bytes, peak disk/RAM, redundancy and uncertainty.
5. **Corruption**: seeded bit flips/truncations/crossed valid pages and malformed
   checksummed cells; exact survivors, known losses, unknown extents; no panic.
   Mutation checks bypass CRC/identity/whole-value validation and must fail.
6. **Snapshots**: unchanged pinned-reader answers during write/checkpoint churn;
   repair runs against an explicitly quiescent source and does not mutate it.
7. **Usability**: reproducible inspect/repair/verify commands, bounded progress
   reporting, retry test, and device-scaled load/reopen/repair measurements.

## Audit at loop start (open defects, not accepted sacrifices)

This is the original R1 audit. Scoped implementation progress and remaining
limits are recorded in RECOVERY_R1.md, RECOVERY_R2.md and the live tracker
journey; the historical list below is not a current status board.

- Damaged overflow aborts the low-level rebuild of unrelated healthy rows.
- Failed page CRC is classified using its untrusted kind byte; flipping Leaf
  to another kind can under-report losses.
- CRC-valid malformed leaf cell aborts the entire recovery iterator.
- Rootless generation/page-number dedup lacks deletion evidence and can
  resurrect old rows. A surviving older version is not a safe substitute.
- lost_pages/lost_ranges/hash maps grow with damage; worst case is O(database).
- Recovery sort arena floors at 4MiB while pool reserves ~2/3 budget; combined
  memory can exceed a small configured total. This needs a real resource gate.
- The in-place rebuild replaces source data before damaged-WAL handling can
  fail; it preserves no original data file on successful lossy recovery.
- Existing padded schema replicas are discoverable through lookup paths only;
  losing all descriptors cannot be solved by inferring names from rows.

tracker: recovery-contract -> fault-corpus -> overflow-salvage/layout-recovery/
structural-salvage -> safe-repair. Other law gates remain explicit tasks.
