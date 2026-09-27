# Edge tables

An EDGE TABLE is how SQL writes edges. It is written the way PostgreSQL 19,
Oracle 23ai and Spanner write a property graph's edges -- a table, a
`CREATE PROPERTY GRAPH ... EDGE TABLES (...)` declaration, and ordinary
`INSERT`, `UPDATE` and `DELETE` -- but it is not stored as a table. In those
systems the edge is a row and the graph is a view over the rows; here the
edge is a native edge (`GRAPH_CONTRACT.md` §2) and the edge table is a view
over the edges. Nothing about an edge is stored twice.

```sql
CREATE TABLE artist (_key TEXT PRIMARY KEY, name TEXT);
CREATE TABLE song   (_key TEXT PRIMARY KEY, title TEXT);
CREATE TABLE performed (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song,
                        performed_on DATE, venue TEXT,
                        PRIMARY KEY (artist_id, song_id, performed_on, venue));
CREATE PROPERTY GRAPH music
  VERTEX TABLES (artist, song)
  EDGE TABLES (performed SOURCE KEY (artist_id) REFERENCES artist (_key)
                         DESTINATION KEY (song_id) REFERENCES song (_key));
INSERT INTO performed VALUES ('dewa', 'kirana', '1998-05-01', 'Jakarta');
```

## 1. What is stored

1.1 An edge table is a collection whose catalog record carries an EDGE tail
    (flag bit `CATALOG_EDGE`) behind the additive feature bit
    `EDGE_TABLE_FEATURE = 0x80000`. A file that never declares one does not
    carry the bit and is unchanged byte for byte; a binary that predates the
    bit refuses a file that carries it, whole, as `Unsupported` (Law 8).
1.2 The tail records: the columns declared `REFERENCES` and the collection
    each references; the `PRIMARY KEY` columns (possibly none); and, once a
    property graph declares the table, the source column, the destination
    column and the edge type the edges are written under.
1.3 The collection's layout types the edge's properties: every column that is
    not an endpoint is a property, checked and encoded exactly as a row's
    column is (types, `NOT NULL`, `DEFAULT`). The edge stores them in its
    inline property bag. The collection itself never holds a row.
1.4 The edges are ordinary edges in the base context under the table's edge
    type: `GRAPH_TABLE`, the endpoint sets, `RESTRICT`/`CASCADE` on a row
    delete and verification all see them as they see any edge.

## 2. Declaring one

2.1 `CREATE TABLE` with a `REFERENCES` column records an edge table that no
    property graph has declared yet. It takes no write: an `INSERT` into it is
    refused, naming `CREATE PROPERTY GRAPH`, because an edge needs to know
    which end is which.
2.2 `CREATE PROPERTY GRAPH g ... EDGE TABLES (t SOURCE KEY (a) REFERENCES x
    (_key) DESTINATION KEY (b) REFERENCES y (_key) [LABEL l])` binds `t`: `a`
    and `b` must be two distinct `REFERENCES` columns of `t` naming `x` and
    `y`, the edge type is `l` (default: the table's name), and the edge type
    must have no edges yet and belong to no other edge table.
2.3 A binding is permanent. `DROP PROPERTY GRAPH` removes the graph's
    definition and never an edge; the edge table stays an edge table.
2.4 An edge type bound to an edge table takes no write from the untyped calls
    (`link`, `put_edge`, `create_edge`): its edges carry typed properties and
    a key, and only the edge table checks them.
2.5 An edge table's name is a table name: it shares the table namespace of
    its schema.

## 3. The primary key

The key decides how many edges a pair may have. Every accepted key names the
source, the destination, or both, so the uniqueness check is one read of
edges stored together (§2.2 of `GRAPH_CONTRACT.md`).

| `PRIMARY KEY` | Edges | The check on `INSERT` |
|---|---|---|
| `(source, destination)` | one per pair: the tuple's own edge (id 0) | does the pair have an edge |
| `(source, destination, c...)` | several per pair, one per value of `c...` | the pair's edges, comparing `c...` |
| `(source[, c...])` | at most one per source (per value of `c...`) | the source's edges of the type |
| `(destination[, c...])` | at most one per destination | the reverse edges of the type |
| none | every insert a new edge, as a table without a key | nothing |

A key that names neither endpoint is refused when the table is declared: it
would need an index over edge properties, which does not exist.

## 4. Writing

4.1 `INSERT` resolves each endpoint's key in the referenced collection: a
    missing row is `23503` (foreign_key_violation). A duplicate key is `23505`
    (unique_violation). A plain `INSERT` never overwrites.
4.2 `INSERT ... ON CONFLICT (key columns) DO UPDATE SET c = EXCLUDED.c` and
    `DO NOTHING` are the upsert.
4.3 `UPDATE t SET c = ... WHERE <every key column> = ...` rewrites the
    properties of that edge. A key or endpoint column is not assignable: a new
    identity is a `DELETE` and an `INSERT`.
4.4 `DELETE FROM t WHERE <source and/or destination> = ... [AND c = ...]`
    removes every edge that matches.
4.5 An `UPDATE` or `DELETE` that names no endpoint is refused: it would read
    every edge of the type.

## 5. Reading

5.1 `SELECT ... FROM t WHERE source = ... [AND ...]` (or `destination =`) reads
    that node's edges of the type. A `WHERE` that names no endpoint is refused,
    as a `WHERE` on an unindexed column is.
5.2 `GRAPH_TABLE (g MATCH ...)` walks the edges under their label.

## 6. Refused, by name

`ALTER TABLE`, `TRUNCATE`, triggers and `CREATE INDEX` on an edge table; an
`UPDATE` of a key or endpoint column; a property graph over a table that is
not an edge table; several labels on one table, `DEFAULT LABEL`,
`PROPERTIES ARE ALL COLUMNS EXCEPT`; a key that names no endpoint.
