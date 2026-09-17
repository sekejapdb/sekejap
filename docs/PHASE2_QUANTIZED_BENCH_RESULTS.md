# Explicit approximate-vector cost and recall probe

Candidate evidence, 2026-09-17. These are **E4 versus E4 exact-search** results,
not a SQLite comparison. The quantized index remains optional; original f32
vectors and exact search are preserved.

On server Linux, the retained build ran two controlled synthetic corpora:10,000
vectors of32 floats and2,000 vectors of1,536 floats. Each contains zeros, exact
ties, unit/nonunit and clustered vectors. Four query cases × three distance
metrics × four effort values × three repetitions gives144 paired records per
corpus. Exact answers and every approximate returned distance are checked
against independent f64 math. Query order reverses in the middle repetition.

At `k=10, ef=40` (scan compact values, then read/rerank at most40 originals):

| Corpus | Exact cosine, unit query | Approximate cosine, same query | Minimum recall@10 across tested cases/metrics | Paired speedup range across cases/metrics |
|---|---:|---:|---:|---:|
|10K ×32 floats|12.70 ms|3.92 ms|100%|2.89–3.66×|
|2K ×1,536 floats|36.66 ms|14.24 ms|100%|2.27–2.84×|

Times are medians of three repetitions of the named query. Speedup ranges use
per-case median exact time divided by median approximate time; different
queries are not pooled into a single latency. Full recall here means matching
all ten independent expected IDs in this synthetic sample. It is **not a
production recall guarantee**. Returned distances are exact for selected IDs;
selection remains approximate.

| Effort `ef` | Worst recall,32-float corpus | Worst recall,1,536-float corpus |
|---:|---:|---:|
|10|90%|90%|
|40|100%|100%|
|160|100%|100%|
|640|100%|100%|

Larger effort has a cost: at ef640 one1,536-float case is approximately4.7%
slower than the exact reference. This is a linear compact scan, not sublinear
navigation or HNSW.

Checkpointed total logical file sizes, including the exact index already
present before adding the optional quantized index:

| Corpus | With exact index | With both indexes | Added quantized bytes |
|---|---:|---:|---:|
|10K ×32|2,330,720 B|2,941,024 B|610,304 B (+26.2%)|
|2K ×1,536|16,670,816 B|20,811,872 B|4,141,056 B (+24.8%)|

Quantized builds took0.324 s and0.274 s respectively, using256-row durable
build steps. Build order was fixed: exact first, quantized second. This is one
process per corpus with repeated warm queries; process-level repetition,
large-scale coverage and final-source acceptance remain outstanding. Memory
figures include the benchmark's independent source vectors/oracle, so they do
not prove engine-only memory scaling. Peak disk sampling is a lower bound.

Source/binary hashes, all288 raw query records and exact expected/returned IDs:
[quant-bench-r1 evidence](phase2-evidence/quant-bench-r1/).
The result supports continuing qualification of the optional family; it does
not complete Phase2 or its SQLite workload comparison.
