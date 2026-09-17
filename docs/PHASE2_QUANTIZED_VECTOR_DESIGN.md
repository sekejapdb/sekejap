# Phase 2 explicit approximate vector search

Candidate design,2026-09-17. Exact search remains the default. This optional
family scans compact per-vector approximations, selects a bounded shortlist,
and reranks that shortlist from authoritative f32 sidecars. It is a linear
quantized scan, not a navigation graph, HNSW, or a sublinear-search claim.
Acceptance requires measured recall, query cost and extra storage/write cost.

## Storage and compatibility

Proposed `IndexFamily::QuantizedVector`, family5/encoding1, feature0x20,
namespace0x79. Creation is explicit. Existing scalar, graph, exact-vector,
spatial and text meanings do not change. Older engines must refuse the new
feature before mutation. Raw f32 sidecars retain their existing bytes.

Descriptor payload follows the shared index envelope:
`id:u64be | collection:u32be | family:u8=5 | encoding:u16be=1 |
dimension:u32be | quantizer:u8=1 | options:u8=0 | state:u8 | cursor:u64be |
name_len:u16be | name | field_len:u16be | field`.
Unknown quantizer/options/version refuse admission. Shared replicated
catalog identity and Building/Ready/Dropping lifecycle remain in use.

Entry key: `79 | ordered(index_id) | ordered(entity_sequence)`.
Entry value: immutable `layout:u32be | ordinal:u16be`, then quantized codec.
Historical layout/ordinal must still name the declared vector field/dimension.
These are derived bytes; a missing authoritative f32 sidecar cannot be repaired
from quantized values.

Quantizer1 is independently scaled symmetric int8:

- Dimension1..16384, matching the declared vector.
- `scale = max(abs(f32 lane))/127` evaluated and stored as f64 little-endian.
- Follow scale with exactly dimension signed two's-complement i8 lanes.
- Code is round(original/scale), halfway away from zero, clamped[-127,127].
- Zero vector: positive-zero scale and all-zero codes. Negative-zero source
  lanes normalize to this same derived representation.
- Nonzero vectors must have a positive finite scale within the f32 source
  range, no code-128, and at least one code of magnitude127.
- Non-finite original/query lanes and noncanonical payloads are refused.

No corpus training, centroid, global calibration, or interpretation depending
on insertion order exists. Subnormal f32 inputs retain a nonzero f64 scale.
The pure codec is `src/vector_quant.rs`; six standalone, non-I/O tests pass:
golden bytes/rounding, extreme magnitudes, metric goldens/zeros, quantization
error bound, malformed inputs, and chunked cancellation. The family is now integrated as a candidate. Linux quant-family-r1 passes
100 checks per ordinary/retained build; see PHASE2_QUANTIZED_RESULTS.md.
This does not freeze the format or establish measured recall/performance.

## Query contract

Proposed convenience API:
`query_quantized_vector(index, query, metric, k, ef, candidates, max_examined,
cancel) -> ApproxVectorResult`.
Require1<=k<=ef<=65536 and finite/dimension-valid query. Candidates are either
the complete compact index or explicit sorted unique entity IDs. Apply every
filter before shortlist selection; a filtered global top-k is incorrect.

Approximate distances are those of reconstructed `scale*code` lanes:
squared-L2, negative dot, or cosine. Use f64 lane-order accumulation and
canonical positive zero, as in exact search. Cosine refuses zero queries and
excludes stored zero vectors. Keep the best ef approximate `(distance,ID)`
entries, fetch their authoritative f32 values, then return exact reranked top-k
with stable ID ties. Returned distances are exact for returned entities;
selection is approximate and can miss better entities.

The result type and combined-query diagnostics must explicitly identify method
`SymmetricInt8ScanV1`, ef, examined and reranked counts. Never label this an
exact full-population answer. Work/cancellation applies during scan, scoring,
sidecar reads and reranking. Existing exact methods and defaults are unchanged.
Combined pagination must define its bounded shortlist population explicitly;
it cannot silently claim completeness over all matching entities.

## Qualification and product decision

Use independent exact ground truth and report recall@k alongside wall time,
scanned entries, exact sidecars read, memory and total disk. Include32- and
1536-lane inputs, unit/nonunit/clustered vectors, zeros, exact ties and all three
metrics. Report multiple ef values; do not infer recall from storage size or
from reranked distances. The existing periodic synthetic vector generator is
useful for deterministic ties but cannot alone establish realistic recall.

Live updates must regenerate codes even if layout/ordinal is unchanged;
unchanged vectors may skip redundant derived writes. Verify bounded late build
and drop, rollback/snapshots/reopen, write faults, old-engine refusal, source
verification and derived rebuild. Filtered queries must use the same snapshot.
Preserve fixtures and full failure evidence. Retain the family only if measured
tradeoffs justify its cost; a failed candidate is not Phase2 approximate-search
acceptance and requires a better implementation, not silent scope reduction.
