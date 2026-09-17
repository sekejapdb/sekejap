# Phase 2 mixed-query and integrity-verifier qualification

Linux results,2026-09-17. These candidate increments pass their selected
checks; full Phase2 acceptance, release and commit remain pending.

| Qualification | Default | Retained packing |
|---|---:|---:|
| First mixed executor plus scalar/family regressions | 27 passed,0 failed | 27 passed,0 failed |
| Runnable combined-query example | passed | passed |
| Indexed-source verifier and current-reader regression | 17 passed,0 failed | 17 passed,0 failed |

The first mixed fixture originally called graph `link` before explicit graph
enablement; both mixed tests stopped during setup. Only the fixture and example
were corrected to call `enable_graph()`. The engine snapshot remained the same.
The failed mixed-query-r1 trial is preserved beside passing mixed-query-r2.

Mixed tests derive expected results independently from the app-like tiny
fixture: scalar/JSON, graph, bbox/radius and text constraints before exact
vector or BM25 ranking; deterministic IDs/scores; targeted vector projection;
rollback, old snapshot, committed updates/deletion/reinsertion and reopen.
The runnable example returned person2 with cosine distance0.29289321881345254
and projected body/vector, using a graph driver and reporting logical work.
This first slice uses complete entity fallback for text/spatial candidate
selection. Native posting drivers are a separate subsequent qualification.

Verifier qualification comprises10 integrity cases plus7 current-reader cases.
It covers clean source preservation, damaged agreeing replicas, ordinary
collection metadata and external-key mappings, orphan family namespaces,
missing postings across families, graph reverse markers, missing/malformed
authoritative dependencies, text statistics, duplicate unique scalar entries,
resource exhaustion and incomplete index lifecycle reporting. Verification
reads committed current membership and does not write the source.

A complete scan can report issues; `clean` must also be true before treating
its contents as consistent. Interrupted Building/Dropping states are not
silently certified clean. Some damage still causes an explicit error rather
than a complete issue report. No verifier can infer an edge if both its primary
and reverse records were removed consistently; an external manifest/history
would be needed to distinguish that from legitimate deletion. Missing primary
rows/vector lanes/edge properties are not recoverable from secondary postings.

Commands, source hashes, complete logs and final markers:
[phase2-evidence/verifier-mixed-r1](phase2-evidence/verifier-mixed-r1/).
Execution used the existing isolated server Linux job with2CPU/2GiB limits,
release builds and locked offline dependencies. Source snapshots are immutable;
query-driver, rebuild and quantized-vector work underway was excluded.
