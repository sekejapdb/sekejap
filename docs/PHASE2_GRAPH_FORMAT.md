# Phase 2 graph format — implementation candidate

Not released or frozen. `enable_graph()` explicitly sets logical feature bit1
(mask3 with the catalog bit) and writes replicated graph metadata in the same
transaction. Ordinary scalar or entity writes preserve their existing feature
mask. Rollback restores it. Phase1 entity/page/WAL bytes are unchanged.

All tags below are hexadecimal. Integer identities use the existing minimal
ordered integer codec. An entity identity contains collection then sequence.

| Tag | Key after tag | Value |
|---|---|---|
| 06 | copy 0..2 | 2081-byte E4GRF01 packet |
| 07 | kind:u8, copy:u8, ordered ID | 2081-byte E4GNM01 packet |
| 12 | kind:u8, exact UTF8 name | ordered ID |
| 71 | source entity, context, type, destination entity | version:u8=1 + Phase1 binary-JSON object |
| 72 | destination entity, context, type, source entity | empty reverse marker |

Graph header payload: encoding:u16be=1, flags:u16be=1 (mandatory reverse),
next_type:u64be, next_context:u64be, type_count:u32be, context_count:u32be.
Name payload: kind:u8 (type0/context1), ID:u64be, length:u16be, UTF8 bytes.
Each dictionary allows 4096 entries and names of 1..128 bytes. Names are exact,
including embedded NUL and Unicode; no hashes or normalization. IDs start1,
remain contiguous, and are never deleted/reused after commit. Context0 is BASE.
A type/context dictionary currently cannot be garbage-collected.

“Phase1 binary JSON” above is the existing `binary_json`/`read_binary_json`
codec in `src/lib.rs`, not JSON text. Each value begins with a one-byte type:
null0, false1, true2, signed integer3, unsigned integer4, finite f64 number5,
UTF8 string6, array7, object8. Unsigned integers, byte lengths and element counts
use canonical unsigned base-128 varints with least-significant seven-bit groups
first. A signed integer is a width byte 1..8 followed by two's-complement
big-endian bytes; the encoder writes the shortest sign-preserving width, while
the version1 reader accepts any width in that range. A number is f64
little-endian. A string is its varint byte length and UTF8 bytes. Arrays contain
a varint count then values.
Objects contain a varint count then repeated untagged string keys and values;
decoded keys must be strictly increasing and unique. Nesting is at most64 and
trailing bytes are invalid. The graph property payload must decode to an object,
and its total encoded edge value, including version, is at most64KiB.

An edge is uniquely identified by (source, context, type, destination).
Replacing that tuple replaces its properties. Both endpoints must exist;
cross-collection edges are allowed. Properties are objects with at most64KiB
encoded bytes, also subject to the persisted record limit. Primary rows hold
user data; a reverse marker cannot reconstruct lost primary properties.

Neighbor calls return the complete matching result or an explicit bound error:
max256 edges and1MiB encoded property bytes per call. BFS is deterministic
shortest-hop traversal with distinct entities, depth then entity-ID ordering,
maximum depth64, visited65536, edge work1M and result65536. A caller can select
smaller bounds. Budget exhaustion returns an error, not an incomplete success.
Cooperative cancellation is available through neighbors_with_cancel and
traverse_bfs_with_cancel, polled before work and throughout traversal. Cancellation
returns an explicit error and leaves subsequent reads usable. Pagination remains
interface work; current neighbor calls require the complete result to fit.

Entity deletion preflights incoming/outgoing edges across all contexts and
types, deduplicates self-edges, then deletes at most256 incident tuples in the
same transaction as scalar/vector/primary removal. Larger degree refuses before
graph mutation; explicit batched edge deletion is required first. An error may
poison the writer; rollback restores the last committed transaction. No hidden
commit occurs. Published snapshots retain their earlier rows and relationships.

Admission validates bounded name catalogs and all replica versions before any
source modification. Missing feature flags with graph metadata or edge entries
are refused. Whole-edge consistency is checked when accessed, not by an O(edges)
open scan. The explicit read-only verifier checks primary/reverse agreement, and
the source-preserving rebuild recreates reverse markers from surviving primary
edges in a separate destination. A reverse marker never recovers primary edge
properties. If both members of an edge pair are deleted consistently, no
remaining byte proves that the edge existed; verification cannot report that
loss and rebuild must not resurrect it.

Costs: three metadata packets per dictionary entry, one primary and one reverse
B-tree key per edge, repeated metadata/pair reads, property decoding during
traversal, and bounded result buffers. Candidate Linux graph, compatibility,
crash and recovery evidence is recorded in `PHASE2_SCALAR_R6_RESULTS.md`,
`PHASE2_MULTIMODEL_COMPAT_RESULTS.md`, `PHASE2_CRASH_RESULTS.md`,
`PHASE2_REBUILD_RESULTS.md` and `PHASE2_WORKSPACE_RESULTS.md`. This evidence does
not freeze or release the format. Preserved cross-version fixtures still do not
cover graph-independent masks or an index BUILDING/DROPPING lifecycle state;
candidate keys can still change before release, while shipped Phase1 entity
bytes cannot.
