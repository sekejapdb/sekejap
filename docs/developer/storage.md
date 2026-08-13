# Storage — what's on disk and why

The disk-first contract: bulk bytes live on disk; RAM holds identity, offsets,
and hot index structures. Every file below is either the source of truth
(payloads, WAL) or rebuildable from it (everything else).

## Design philosophy: fast open, RAM as accelerator

> sekejap opens at lightspeed and treats RAM as a clever accelerator, not a
> requirement.

In paged mode every index *maps* its bytes from disk rather than *rebuilding*
them, so **open time is flat in dataset size** — 10 K rows and 40 M rows open in
the same milliseconds (measured: BM25 6.9 ms mmap vs 103.7 ms rebuild at 50 K).
Data lives on disk and the OS page cache holds the hot working set. RAM is a
small, bounded, deliberate boost — the target is a sub-linear resident footprint
(≈10 B/doc; 40 M records → a few hundred MB), never an O(N) blow-up and never a
rebuild-on-open tax. Three rules follow for every index:

1. **Never rebuild on open** — persist it, mmap it, skip the rebuild.
2. **No O(N) resident structure** — anything that grows with row count goes on
   the mmap (sorted array + binary search, or a flat typed array). Only
   sub-linear things (O(vocab), O(N/M)) may stay resident, as a chosen accelerator.
3. **Resident mode unchanged** — the in-RAM path keeps its direct `Vec`/`HashMap`
   structures; the mmap is paged-mode only, so there is no regression.

## File inventory

| group | files | role |
|---|---|---|
| payloads | `payloads.bin`, `field_table.bin` | append-only record bytes (SKBIN or raw JSON) + shared field-name table |
| topology | `nodes.bin`, `idx.bin`, `adj_fwd.bin`, `adj_rev.bin`, `slugs.bin`, `dict.bin`, `collections.bin`, `edgemeta.bin` | dense-id node records, hash→id lookup, forward/reverse CSR adjacency, names and dictionaries |
| vectors | `vectors_{field}.bin` | append-only per-field f32 vectors |
| indexes | `gin.bin`, `search.bin`, `spatial.bin`, field-index sidecars | rebuildable trigram/positional/spatial/scalar access paths |
| durability | `wal.log`, `snapshot.json` | framed CRC'd mutation log; versioned manifest |

## Payloads: SKBIN

New writes append as raw JSON. During compaction, live records up to 64 KB are
rewritten as **SKBIN** — a per-record binary encoding governed by one
recoverability rule:

> Field *names* may be shared (they come from the schema and the redundant,
> checksummed `field_table.bin`). User *values* never move into a shared
> dictionary. A corrupt byte destroys at most one record.

SKBIN is ~1.6× smaller than JSON, slightly faster to read whole, and ~6×
faster for single-field extraction. Records larger than 64 KB stay raw JSON on
purpose: grouped queries over huge payloads read only a head+tail slice and
extract the few needed fields, instead of parsing megabytes.

Full wire format: [notes/skbin-format.md](notes/skbin-format.md).

## Topology: dense ids + CSR

`compact()` assigns every node a dense integer id and writes adjacency as
**compressed sparse row (CSR)**: one flat array of neighbor ids per direction
(`adj_fwd.bin`, `adj_rev.bin`, StreamVByte-delta encoded), plus per-node
(offset, count) entries. Properties:

- offset-addressable — a node's neighbors are one slice, no pointer chasing;
- mmap-friendly — the OS page cache does the caching; the engine can serve
  traversals from mmap'd slices with only the (offset, count) index resident
  (96 MB of heap adjacency became 21 MB on a 925k-edge graph, with equal or
  better traversal latency);
- paging is a read-path flip, not a format migration — the same layout serves
  both fully-resident and mmap'd reads.

Full format: [notes/topology-format.md](notes/topology-format.md).

## Vectors: f32 on disk, working set in RAM

Per-field vector stores append f32 records to `vectors_{field}.bin` with an
in-memory id→offset map. Reads are zero-copy from the store's mmap window when
possible, positional reads (`pread`) otherwise. The disk-first HNSW keeps only
int8-quantized codes and the graph resident and rescores top candidates from
the f32 file — details in [indexes.md](indexes.md).

## Scalar field indexes: heap overlay + mapped base

Paged mode serves equality/range postings from an mmap'd on-disk field index
(`storage/fieldstore.rs`); resident mode uses the in-heap btree. The query
executor sees one interface (`FieldIndexRef`) over both, so query code never
knows which backing it hit.

## Rules that keep this honest

- Never parse a payload to answer a question metadata can answer: collection
  tags, offsets, and spatial summaries live in the node entry.
- Sidecar indexes must always be rebuildable — deleting every index file and
  reopening must yield a correct (slower) database.
- Compaction streams — it must not materialize the dataset in RAM.

The enforcement checklists live in [invariants.md](invariants.md).
