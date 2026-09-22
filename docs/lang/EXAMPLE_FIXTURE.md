# The documentation example fixture

Every `sql` block in sekejap's documentation is RUN, by
`dist/rust/tests/doc_examples.rs`, against the one database this document
describes. A doc example may assume everything below and nothing else.

That database holds TWO SHAPES, because two sets of documents were written
against two different ones and both sets are kept:

| shape | collections | edge types | who writes against it |
|---|---|---|---|
| the small one | `posts` (12 rows), `people` (4), `readings` (3) | `knows`, `wrote` | `README.md`, `docs/dist/*`, `docs/core/*`, `docs/TIMESTAMPS.md` |
| the wide one | `place` (200 rows) | `near` | `docs/lang/QL_CONTRACT.md`, whose §0 declares it |

They share one database and touch none of each other's names, so a block may
use either and a document may use both. Every collection is in the base graph
context, which `GRAPH_TABLE` spells `base`.

Run them with:

```sh
cargo test --release --features compact-cells,sqlite-balance,keyspace-append,slotref-split \
  -p sekejap --test doc_examples -- --test-threads=1
```

---

## 1. How a block is read

The harness acts on the fence's info string and on nothing else.

| fence | what happens |
|---|---|
| `sql` | a **`;`-separated script**. Its statements run in order on a copy of the fixture, and every one of them must answer without error. |
| `sql refused` | ONE statement that must be REFUSED. See §5. |
| `rust` | a RUNNABLE example. See §6. |
| `rust,signatures` | declarations only — a type, a struct, a trait, a module list. Ignored. |
| everything else (`sh`, `text`, `json`, `toml`, `rust,ignore`, no info string) | ignored. |

**One statement or a script — the convention, stated once.** A `sql` block is
a script: several statements separated by `;`, run in order. One statement per
block is the same rule with one statement in it. `--` line comments are the
language's own (`lang/src/lexer.rs`), so a block can explain itself.

**Parameters.** A statement that takes `$n` values is preceded by a
`-- params:` line holding a JSON array. It applies to the statement after it
and to no other:

```sql
-- params: ["p03"]
SELECT _key, title, views FROM posts WHERE _key = $1;

-- params: [40]
SELECT _key FROM posts WHERE views >= $1 ORDER BY views DESC LIMIT 3
```

**Isolation.** Each documentation FILE gets its own copy of the fixture, so a
block that writes cannot reach the file after it. Blocks within one file share
that copy, in document order, so a doc can INSERT and then SELECT what it
inserted.

**A statement's door.** The harness picks the `Db` call by the statement's
first word, which is the three doors `docs/dist/RUST_API.md` §3 names:
`SELECT`, `SHOW` and `TABLE` go to `Db::query`, `EXPLAIN` to `Db::explain`,
and everything else to `Db::execute`. The door is part of what an example
asserts, and one family shows it: `Db::explain` is `explain_sql`, which RUNS
the statement to explain it, so `EXPLAIN DROP TABLE` and `EXPLAIN` of a
predicated write are refused through all three doors and are written as
`sql refused` blocks (`docs/lang/QL_CONTRACT.md` §2, the
`EXPLAIN <statement>` row).

---

## 2. `posts` — one column of every `Kind`

12 rows, keys `p01` … `p12`. Declared by this `CREATE TABLE`, so the
`TIMESTAMPTZ` and `DATE` spellings are in the catalog and not only the
`Kind::Int` underneath them:

`WITH (index: none)` on both shapes is deliberate. Every eligible column of a
table is indexed when the table is created
(`docs/lang/INDEX_CONTRACT.md`), so a fixture that said nothing would hold
indexes this document does not list, and `place.note` -- whose whole job is to
be the column a refusal can name -- would have one. The fixture therefore opts
out once per shape and declares each index by hand, which is what makes the
index tables below exactly what the database holds.

```sql
CREATE TABLE posts_example (
    title TEXT,
    body TEXT,
    views INT,
    score REAL,
    live BOOLEAN,
    meta JSONB,
    at TIMESTAMPTZ,
    day DATE,
    loc GEOMETRY(Point, 4326),
    area GEOMETRY(Polygon, 4326),
    emb VECTOR(4)
) WITH (index: none);
DROP TABLE posts_example
```

| column | `Kind` | row `n` (1-based) holds |
|---|---|---|
| `_key` | `Text` | `p01` … `p12`. A declared field of every layout. |
| `title` | `Text` | one of `kebun raya`, `pasar pagi`, `warung kopi`, `sawah luas`, `danau biru`, `hutan kota`, `kantor pos`, `sekolah dasar`, `jembatan tua`, `bengkel motor`, `desa wisata`, `pasar malam`, in that order |
| `body` | `Text` | `<title> di kota <city>`, the city cycling `jakarta`, `bandung`, `surabaya` |
| `views` | `Int` | `10 × n`, so 10 … 120 |
| `score` | `Real` | `n / 2`, so 0.5 … 6.0 |
| `live` | `Bool` | true for even `n`, so 6 rows are live |
| `meta` | `Json` | `{"author": "alice" or "bob", "pinned": <n = 1>}` |
| `at` | `Int`, declared `TIMESTAMPTZ` | `2026-01-<n>T09:00:00Z` |
| `day` | `Int`, declared `DATE` | `2026-01-<n>` |
| `loc` | `Point` | lon `106.80 + 0.01n`, lat `-6.20 - 0.01n` — a line of 12 points in west Java |
| `area` | `Geo` | a 0.008° square polygon centred on `loc` |
| `emb` | `Vector(4)` | a unit vector with lane `(n-1) mod 4` at 1 and lane `n mod 4` at 0.25 |

Indexes on `posts`:

| name | family | on |
|---|---|---|
| `posts_views` | btree | `views` |
| `posts_at` | btree | `at` |
| `posts_title` | btree | `title` |
| `posts_title_lower` | btree (expression) | `lower(title)` |
| `posts_body` | gin | `to_tsvector('simple', body)` |
| `posts_loc` | gist | `loc` |
| `posts_area` | gist | `area` |
| `posts_emb` | exact | `emb` |
| `posts_emb_ann` | diskann (the vamana graph family) | `emb`, `vector_cosine_ops` |

## 3. `people` and the graph

4 rows, keys `alice`, `bob`, `carol`, `dave`, with `name` (`Alice` …) and
`city` (`Jakarta`, `Bandung`, `Surabaya`, `Jakarta`). One btree index,
`people_city`, on `city`.

Edges, all in the BASE graph context, which `GRAPH_TABLE` spells `base`:

| from | type | to | properties |
|---|---|---|---|
| `people/alice` | `knows` | `people/bob` | — |
| `people/bob` | `knows` | `people/carol` | — |
| `people/carol` | `knows` | `people/dave` | — |
| `people/alice` | `wrote` | `posts/p01` | `{"year": 2026}` |
| `people/bob` | `wrote` | `posts/p02` | `{"year": 2026}` |
| `people/carol` | `wrote` | `posts/p03` | `{"year": 2026}` |
| `people/dave` | `wrote` | `posts/p04` | `{"year": 2026}` |

So `knows` is a chain: one hop from `alice` reaches `bob`, three hops reach
`dave`, and the walk is acyclic by construction.

## 4. `readings` — automatic timestamps ON

3 rows, keys `r01` … `r03`, with `sensor` (`s1` … `s3`) and `celsius`
(21.0, 22.0, 23.0). This is the one collection created with
`CollectionOptions { timestamps: true }` (`docs/TIMESTAMPS.md`), so every row
also carries the engine-managed `_created_unix` and `_updated_unix`.

## 5. `place` and the `near` chain — the shape `QL_CONTRACT` §0 declares

200 rows, keys `p000` … `p199`. This is the shape every block of
`docs/lang/QL_CONTRACT.md` §8 is written against, and §0 of that document
declares it column for column; what follows is the same declaration with the
values filled in. Row `n` below is 0-based, so `p000` is `n = 0`.

```sql
CREATE TABLE place_example (
    name TEXT,
    body TEXT,
    kind TEXT,
    born INT,
    rating DOUBLE PRECISION,
    active BOOLEAN,
    at TIMESTAMPTZ,
    day DATE,
    loc GEOMETRY(Point, 4326),
    area GEOMETRY(Polygon, 4326),
    emb VECTOR(4),
    tag TEXT,
    note TEXT
) WITH (index: none);
DROP TABLE place_example
```

| column | `Kind` | row `n` (0-based) holds |
|---|---|---|
| `_key` | `Text` | `p000` … `p199`, a fixed width, so a key range is written without surprise |
| `name` | `Text` | `<word> <nnn>`, the word cycling `kebun`, `sawah`, `pasar`, `kopi`, `danau`, `hutan`, `desa`, `taman` — two tokens, so `split_part(name, ' ', 1)` and `left(name, 3)` have something to cut |
| `body` | `Text` | `<word> <next word> di kota <city>`, the same vocabulary and the city cycling `jakarta`, `bandung`, `surabaya`. Every eighth row (`n mod 8 = 0`) reads `kebun sawah …`, so `kebun & sawah` and the phrase `"kebun sawah"` both have rows |
| `kind` | `Text` | `depot`, `farm`, `home`, `mill`, `park`, `port`, `school`, `shop` by `n mod 8`, so each value has 25 rows |
| `born` | `Int` | `1900 + n`, so the 200 rows cover 1900 … 2099 and 1990, 1991 and 1993 are one row each |
| `rating` | `Real`, declared `DOUBLE PRECISION` | `(n mod 50) / 10`, except every seventh row (`n mod 7 = 0`, 29 rows), which is WRITTEN as null — so `rating IS NULL` finds rows, and is a different question from `tag IS MISSING` |
| `active` | `Bool` | true for even `n`, so 100 rows are active |
| `at` | `Int`, declared `TIMESTAMPTZ` | `<born>-06-15T09:00:00Z` — one year per row, and mid-year so a `date_trunc('month', …)` boundary is visible |
| `day` | `Int`, declared `DATE` | `<born>-06-15` |
| `loc` | `Point` | lon `106.82 + 0.002(n - 100)`, lat `-6.17 + 0.001(n - 100)` — a line of 200 points THROUGH `(106.82, -6.17)`, the point every spatial example probes with, so `p100` sits on it. The line spans 0.4° of longitude and 0.2° of latitude, which is why a 20 km radius admits 161 of the 200 and the envelope `(106, -7, 108, -6)` admits all of them |
| `area` | `Geo` | a 0.008° square polygon centred on `loc`, so three of them CONTAIN `(106.82, -6.17)` and the rest do not |
| `emb` | `Vector(4)` | a unit vector with lane `n mod 4` at 1 and lane `(n+1) mod 4` at 0.25 |
| `tag` | `Text` | `t<n mod 4>`, and ABSENT from every fifth row (`n mod 5 = 0`, 40 rows) — absent, not null, which is the question `IS MISSING` asks |
| `note` | `Text` | `note <nnn>`. The one column with NO index, so a predicate over it is refused and has something to name, and an `UPDATE … SET note` never writes the column its own driver is walking |

Indexes on `place`:

| name | family | on |
|---|---|---|
| `place_name` | btree | `name` |
| `place_body` | gin | `to_tsvector('simple', body)` |
| `place_kind` | btree | `kind` |
| `place_kind_lower` | btree (expression) | `lower(kind)` |
| `place_born` | btree | `born` |
| `place_rating` | btree | `rating` |
| `place_active` | btree | `active` |
| `place_at` | btree | `at` |
| `place_day` | btree | `day` |
| `place_loc` | gist | `loc` |
| `place_area` | gist | `area` |
| `place_emb` | exact | `emb` |
| `place_emb_ann` | quantized | `emb`, `vector_cosine_ops` |
| `place_tag` | btree | `tag` |

`lower(kind)` is the ONE expression index, which is why `lower(kind) = 'port'`
answers and `lower(name) = 'x'` is refused naming the index it would need.
`loc` carries a POINT index and `area` a GEOMETRY one, which is why a point
predicate composes inside an `OR` and a geometry predicate is refused there
(`QL_CONTRACT` §3, the boolean-leaf row).

Edges, all in the BASE graph context: one edge type, `near`, carrying a
`weight` property, in a chain `p000 -> p001 -> … -> p199` — 199 edges. The
weight of the edge LEAVING `p<n>` is the `n mod 9`-th of
`0.7, 0.8, 0.9, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6`: nine values between 0.1 and
0.9, ordered so the first three hops out of `p000` are heavy and the fourth is
light. An inline `WHERE r.weight > 0.2` over `-[:near]->{1,4}` therefore keeps
three hops and prunes the fourth, which is the thing that example is showing.
The chain is acyclic by construction, so `-[:near]->{1,6}` from `p000` reaches
`p001` … `p006` and nothing else.

`docs/lang/QL_CONTRACT.md` §8 WRITES to this collection: one row `p900` is
inserted, renamed and deleted again; one `UPDATE` sets the unindexed `note`
where `born BETWEEN 1990 AND 1991`; one `DELETE … CASCADE` removes
`born = 1993` with its edges. They run in document order on that document's
own copy of the fixture, which is what §1's isolation rule is for.

## 6. A refused example

A construct sekejap has no atomic for is documented by showing it REFUSED, not
by leaving it out. The block's first line names the SQLSTATE a PostgreSQL
client is given (`docs/dist/WIRE_CONTRACT.md` §8) and the construct:

```sql refused
-- refused 0A000: JOIN
SELECT p.title FROM posts p JOIN people ON people._key = p.title
```

The harness asserts the statement was refused AND that the refusal carries
that SQLSTATE, so a construct that quietly starts answering fails the test
just as loudly as one that starts erroring.

## 7. A runnable Rust example

A ```` ```rust ```` block is claimed by an HTML comment on the line above it:

```text
<!-- doc_example: readme_quickstart -->
```

and the block must then equal the body of `#[test] fn doc_readme_quickstart()`
in `dist/rust/tests/doc_examples.rs`, byte for byte once the indentation is
trimmed. `cargo test` compiles and runs the test; the harness proves the doc
and the test have not drifted apart. A block that only DECLARES things — a
struct, a trait, a module list — is tagged `rust,signatures` instead and is
ignored.

Because the block is a function BODY, it ends with `Ok(())` and may use `?`.

## 8. What the fixture deliberately does not have

* No second graph context. `base` is the only one, so a doc example never has
  to explain a context it cannot see.
* No collection with a `PRIMARY KEY` column. The key is `_key`, spelled that
  way everywhere, and a `PRIMARY KEY` column would store it twice
  (`lang/src/compile/ddl.rs`).
* No 50,000-row corpus. The counts above are small on purpose: an example is
  read, and a number a reader can check by hand is worth more than a number
  that only a benchmark can. 200 rows is the largest of them, and it is 200
  rather than 12 only because `place` has to spread eight `kind` values, a
  year per row and a 199-edge chain across distinct values.
