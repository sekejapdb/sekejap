# Candidate full-text format and behavior

Status: implemented Phase 2 candidate, not released or frozen. Analyzer-v1,
persisted postings, catalog admission, bounded lifecycle, queries and explicit
verification/rebuild are integrated. This extends the common catalog without
changing `e4-format-v1` primary records.

## Stable analysis and ranking

Analyzer version 1 splits input on non-alphanumeric Unicode characters, then
lowercases each character of each token. It has no stemming, stopwords, Unicode
normalization or accent folding. This follows the E3 algorithm but freezes its
Unicode data: `src/text_unicode_v1.rs` contains Unicode 17.0.0 property and
lowercase tables generated with `rustc 1.96.0 (ac68faa20 2026-05-25)`.
`tools/generate_text_unicode_v1.rs` is a one-time generator, never a build step.
Future compiler Unicode updates must not change version 1. A future analyzer
requires explicit creation with a distinct supported version.

The source APIs are Rust's documented `char::is_alphanumeric`, `to_lowercase`
and `std::char::UNICODE_VERSION`. The runtime uses checked-in tables, with
stable ASCII shortcuts. Pure tests contain fixed multilingual expectations and
compare all Unicode scalars to the generator APIs when their Unicode versions
match. Lowercase expansion can introduce combining marks (for example dotted
capital I); stored tokens must not be re-tokenized during validation.

Per document: at most 64 KiB input, 128 UTF-8 bytes per normalized term,
16,384 tokens and 4,096 distinct terms. Exceeding a bound rejects the whole
operation; truncation must not produce an apparently complete index.
Missing/null text is absent from the corpus. An explicit empty or punctuation-only
string is a present document with zero tokens.

BM25 version 1 uses K1=1.2 and B=.75, positive
`ln(1 + (N-df+.5)/(df+.5))`, and the `(K1+1)` numerator. Sum each distinct query
term once. Corpus statistics are scoped to one collection and field. Results
sort by decreasing score, then increasing EntityId. Exact candidate filtering
happens before top-k. An empty query returns no matches.

Initial query surface explicitly offers any-term and all-term matching with
BM25 ranking. It accepts literal text, not a query-language expression. Phrase,
proximity, stemming and parser operators are unsupported; do not imply that
punctuation in literal input invokes them. No positions are persisted in this
first representation. Consumer evidence currently requires BM25 term ranking.

## Persisted candidate representation

Family4/encoding1 uses feature bit `0x10`; the bit is monotone after explicit
creation. Its `E4IDX01` descriptor payload is:

```text
index_id:u64be | collection_id:u32be | family:u8=4 | encoding:u16be=1 |
analyzer:u16be=1 | Unicode-major:u8=17 | minor:u8=0 | patch:u8=0 |
BM25:u16be=1 | options:u8=0 | state:u8 | cursor:u64be |
name_len:u16be | name:utf8 | field_len:u16be | field:utf8
```

The field must be a non-unique declared `Kind::Text`. State and cursor use the
common catalog encoding: BUILDING0 with last scanned entity sequence,
READY1/zero, DROPPING2/zero. Unknown family, encoding, analyzer, Unicode, BM25,
options or state combinations are refused. Descriptor replicas, the global
index ID allocator and collection/name mappings are shared with other families.
Family3 and tag `0x74` belong to spatial.

| Tag | Key suffix after tag and ordered index ID | Value |
|---|---|---|
| `0x75` | UTF-8 term, NUL, ordered entity sequence | term frequency, u32 BE |
| `0x76` | ordered entity sequence | document length, u32 BE |
| `0x77` | UTF-8 term, NUL | document frequency, u64 BE |
| `0x78` | none | document count and total token count, two u64 BE |

Analyzer terms contain no NUL. Validate UTF-8, nonempty bounded length,
termination, nonzero IDs/frequencies, counter bounds and exact value length.
The pager checksums these records. The document-length entry distinguishes an
indexed empty document from a missing/unbuilt document. Primary text remains
authoritative; all four text namespaces are derived and can be explicitly rebuilt.
Never use a rebuild as an automatic version-update requirement.

## Transaction and memory rules

Live mutation computes old/new token maps and changes only differing term
postings/statistics, norm and corpus counts in the entity transaction. For a
Building index, a missing norm means the old row has not yet contributed to
statistics: do not subtract its old text. A present norm must agree with the
old primary text before applying deltas. Count underflow/overflow is corruption
or an explicit refusal, not saturation.

A build step captures at most 256 entity IDs, then reads/analyzes/writes one
document at a time. Do not buffer 256 full token maps. Already indexed rows
must not contribute twice when the build cursor reaches them after live writes.
Publish Ready only with the complete cursor walk. Drop removes bounded batches
across all four keyspaces before removing its descriptor and registries.

Search merges at most the bounded number of query-term posting streams,
maintaining a heap proportional to requested k. Explicit candidate mode probes
only candidate IDs. Both paths enforce examined-work and cancellation budgets,
returning an error rather than silently incomplete exact answers. Query memory
must not hold the matching corpus. Queries allow at most64 distinct analyzer-v1
terms; this is separate from the document's 4,096-distinct-term allowance.

## Costs and required evidence

One posting per term/document is simple to update and recover but costs more
keyspace overhead than packed posting segments. Document/term/corpus counters
add writes; a common corpus counter is a write hotspot. Measure those costs
before freezing this family. Whole-index search reads postings, while candidate
search trades sequential work for bounded point probes. Index build and repair
need explicit disk accounting alongside primary data and WAL.

Acceptance requires independent BM25/token oracles; empty/null/missing and
Unicode cases; corpus/field isolation; repeated update/delete/reinsert;
live writes during Building; snapshot/reopen; exhaustive injected write failures;
malformed and future-format refusal; preserved source during recovery; compatible
fixtures; and fair Linux query/CRUD/memory/disk measurements. Passing analyzer
unit tests proves none of those persisted database properties.

Candidate Linux behavior, admission, fault, compatibility and recovery evidence
is recorded in `PHASE2_TEXT_READER_RESULTS.md`,
`PHASE2_MULTIMODEL_COMPAT_RESULTS.md`, `PHASE2_CRASH_RESULTS.md`,
`PHASE2_REBUILD_RESULTS.md` and `PHASE2_WORKSPACE_RESULTS.md`. This evidence does
not freeze or release the format. Preserved ARM fixtures now cover READY text
catalogs and graph-independent mask 17 through BUILDING with a nonzero cursor,
DROPPING and post-drop retained features. Same-copy cross-build cycles and
archive restoration pass; see [rollback results](PHASE2_ROLLBACK_RESULTS.md).
These prepare a candidate baseline, not historical released-version evidence.
