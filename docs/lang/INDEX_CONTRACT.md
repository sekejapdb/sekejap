# Index contract — what every column type gets, and what you choose

Companion to `docs/lang/QL_CONTRACT.md`. That document fixes what the language
accepts. This one fixes **which indexes exist without being asked for, which
ones you declare, and why the line falls where it does.**

## The principle

> An index is declared when there is a decision. It is automatic when there
> is not.

A decision means two or more defensible answers with different costs. Choosing
between an exact vector index and a quantized one is a decision: recall against
hundreds of gigabytes. Choosing how to index a `SMALLINT` is not a decision.
There is one implementation, it costs a few bytes per row, and no one would
ever pick differently.

Asking a caller to opt into the second kind is ceremony with nothing behind it.
It also breaks the first five minutes: a table with a name, a number and a flag
is the first thing anyone writes, and every predicate over it would be refused.

The engine already works this way in one place. Every collection is given its
key mapping without being asked, which is why `WHERE _key = $1` answers with no
`CREATE INDEX` anywhere. This contract extends the line that already exists
rather than inventing a new one.

## What each type gets

**Automatic** means the index is created with the column and maintained on
every write, with no declaration. **Declared** means the statement refuses
until you create it, and the refusal names what it needs.

| declared type | stored as | automatic | you may declare | why the line is here |
|---|---|---|---|---|
| `TEXT`, `VARCHAR` | text | scalar (`btree`) | `gin` over `to_tsvector` | equality, ranges and prefix `LIKE` are choiceless. Full-text is a decision: an analyzer, and a large index |
| `INT`, `INTEGER`, `INT4`, `SMALLINT` | 64-bit int | scalar (`btree`) | — | one implementation, a few bytes per row |
| `BIGINT`, `INT8` | 64-bit int | scalar (`btree`) | — | as above |
| `REAL`, `FLOAT4` | 64-bit float | scalar (`btree`) | — | as above |
| `DOUBLE PRECISION` | 64-bit float | scalar (`btree`) | — | as above |
| `BOOLEAN`, `BOOL` | bool | scalar (`btree`) | — | two values; the index is nearly free |
| `TIMESTAMPTZ`, `TIMESTAMP` | int microseconds | scalar (`btree`) | — | ranges, `EXTRACT(YEAR …)` and `date_trunc` are all one range over this index |
| `DATE` | int microseconds | scalar (`btree`) | — | as above |
| `GEOMETRY(Point,4326)` | point | spatial point | — | one family applies to a point; nothing to weigh |
| `GEOMETRY(<other>,4326)` | geometry cells | geometry | — | the family follows from the declared shape |
| `VECTOR(n)` | f32 row | **nothing** | `exact`, `quantized` | the only genuine trade in the system: exactness against size. See `docs/core/VECTOR_CAPACITY.md` |
| `JSONB`, `JSON` | binary JSON | **nothing** | **nothing exists** | see the gap below |

## The two that get nothing, and why they differ

**`VECTOR(n)` is deliberate.** At fifty million rows a vector column is
hundreds of gigabytes before any index, the two families differ by an order of
magnitude in size, and neither is right for everyone. This is the one place
where the caller must state an intent, and the refusal that names both families
is doing its job.

**`JSONB` is a gap, not a decision.** A JSON column can be stored, returned and
projected, but there is no index family that covers it, so **no predicate over
a JSON field can ever be answered index-side.** The scalar family refuses the
kind outright (`scalar index requires bool/int/real/text`), the path operators
`->`, `->>`, `#>`, `#>>` and `json_array_length` are all refused by name, and a
JSON equality inside a boolean has no membership set. A caller can therefore
declare a column they can never filter on.

That is stated here so it is a known hole rather than a surprise. Closing it
means either a scalar index over an extracted path, which is the expression
index that already exists applied to a JSON path, or a general inverted index
over the document. Neither is built. Until one is, treat `JSONB` as storage,
not as queryable data, and lift any field you intend to filter into a column of
its own.

## Rows this table will grow

Two entries are named here so the table above is understood as a shape that
extends, not a closed list.

**A graph vector family.** The `quantized` family is a bounded linear scan over
int8 codes: it reads every entry, which is correct and fast while a collection
is small and stops being either as it grows. A graph family, Vamana over
quantised codes, is the disk-first answer and would join `exact` and
`quantized` as a third **declared** vector option. It stays declared, because it
is the same decision the other two are, only with another point on the curve.
It is the one planned family that needs a new keyspace and a new feature bit.

**A JSON path, as an expression index.** The gap below is closable without a
new family. The scalar family already supports EXPRESSION indexes, behind
`EXPRESSION_FEATURE = 0x400`, which is how `CREATE INDEX i ON t (lower(col))`
stores the fold rather than the column. `IndexExpr` has exactly one variant
today, `Lower`. A second variant that extracts a JSON path would give:

```sql,proposed
CREATE INDEX people_status ON people ((payload->>'status'))
```

and a predicate over that same expression would be answered from it, which is
precisely how PostgreSQL indexes JSON. It reuses the scalar family, the scalar
keyspace and an already-shipped feature bit. What it needs is a descriptor that
can carry the path beside the variant byte, and a decision about whether an
older reader should refuse such a descriptor under its own additive bit. It
does not need a new index family, and it does not need a new keyspace.

A general inverted index over whole documents, the equivalent of PostgreSQL's
`jsonb_path_ops`, remains the larger and separate option. Path extraction
should come first, because it is cheaper and covers the common case: one field
inside a document that you filter on often.

## What automatic costs, and how to refuse it

Every automatic index is maintained on every write. A table of twenty scalar
columns maintains twenty postings per inserted row, including for columns
nobody filters. That is the only real argument against this rule, and it is
answered by making the default overridable rather than by making every caller
opt in:

```sql,proposed
CREATE TABLE events (
    event_id TEXT PRIMARY KEY,
    payload  TEXT,
    seen_at  TIMESTAMPTZ
) WITH (index: none)              -- nothing automatic; declare what you need

CREATE TABLE events (...) WITH (index: [seen_at])   -- only this column
```

The escape is explicit and the default is helpful. Today it is the other way
around: the default is unhelpful and there is no escape.

## Status

The principle and the table above are the target this contract adopts. Of it,
what exists today is the automatic key mapping and every **declared** family in
the right-hand column. Automatic scalar and spatial indexing, and the
`WITH (index: …)` override, are not yet built; until they are, every column in
the automatic column must be declared by hand with `CREATE INDEX`.

The refusal for an unindexed predicate stays in every case. Nothing here
permits a silent scan; it changes only which indexes exist without being asked.
