# Edge tables and property graphs

How SQL writes edges, and how a property graph names them. The statements are
ISO SQL/PGQ (ISO/IEC 9075-16) as Oracle 23ai and Spanner write them. The
storage underneath is sekejap's own: an edge is a native edge
(`GRAPH_CONTRACT.md` §2), stored once.

PostgreSQL 19 was to ship the same statements and reverted them before its
release (7 September 2026); sekejap follows the standard and Oracle's reading
of it.

## 0. Names and defaults

| Name | What it is | Created | Dropped |
|---|---|---|---|
| `public` (schema) | Where a table goes when a statement names no schema. | with the database | never |
| `base` (graph) | Every table and every edge, in every schema. ISO GQL calls such a graph the *home graph*; SQL/PGQ has none. | with the database | never |

Three layers, and only the first holds data:

```text
named graphs   music: [artist, song, performed]      charts: [artist, song, performed]
(views)                    │ a definition: names and labels over chosen tables
                           ▼
edge tables    performed = source artist_id, destination song_id, edge type `performed`
(the doors)                │ INSERT / DELETE go through here
                           ▼
base           dewa ──performed──▶ kirana        once ──performed──▶ kirana
(the data)
```

```sql
CREATE TABLE artist (_key TEXT PRIMARY KEY, name TEXT);
CREATE TABLE song   (_key TEXT PRIMARY KEY, title TEXT);
CREATE TABLE performed (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song,
                        role TEXT, PRIMARY KEY (artist_id, song_id, role));

-- The direction, once. `base` is enough; a named graph could do it too.
ALTER PROPERTY GRAPH base ADD EDGE TABLES (
  performed SOURCE KEY (artist_id) REFERENCES artist (_key)
            DESTINATION KEY (song_id) REFERENCES song (_key));

INSERT INTO performed VALUES ('dewa', 'kirana', 'band');

SELECT * FROM GRAPH_TABLE (base
  MATCH (a:artist)-[p:performed]->(s:song) RETURN a.name, p.role, s.title);
```

## 1. What is stored

1.1 An edge table is a collection whose catalog record carries an EDGE tail
    (flag bit `CATALOG_EDGE`) behind the additive feature bit
    `EDGE_TABLE_FEATURE = 0x80000`. A file that never declares one does not
    carry the bit and is unchanged byte for byte; a binary that predates the
    bit refuses a file that carries it, whole, as `Unsupported` (Law 8).
1.2 The tail records the columns declared `REFERENCES` and the collection
    each references, the `PRIMARY KEY` columns (possibly none) and, once its
    direction is fixed, its BINDING: the source column, the destination
    column and the edge type its edges are written under.
1.3 The collection's layout types the edge's properties: every column that is
    not an end is a property, checked and encoded exactly as a row's column
    is (types, `NOT NULL`, `DEFAULT`). The edge stores them in its inline
    property bag. The collection itself never holds a row.
1.4 The edges are ordinary edges in the base context under the table's edge
    type: `GRAPH_TABLE`, the endpoint sets, `RESTRICT`/`CASCADE` on a row
    delete and verification see them as they see any edge.
1.5 A property graph's definition is stored on each member table's own
    catalog record, as a MEMBERSHIPS tail (flag bit `CATALOG_GRAPHS`) behind
    `PROPERTY_GRAPH_FEATURE = 0x400000`: the graphs the table belongs to,
    its element name there, vertex or edge, and its labels. A graph is the
    set of tables that name it; no edge and no row is part of it. The base
    graph's own extra labels are memberships of `base`.
1.6 A file written before 0.18.3 recorded a graph only as a name on each edge
    table it declared. Such a graph opens as it was: each of those edge
    tables an edge element, each table at one of their ends a vertex element,
    each labelled with its own name. Its first definition write stores it the
    0.18.3 way.

## 2. An edge table's direction

2.1 `CREATE TABLE` with a `REFERENCES` column records an edge table with no
    direction yet. It takes no write: an `INSERT` is refused, naming
    `PROPERTY GRAPH`, because an edge needs to know which end is which.
2.2 The first `CREATE PROPERTY GRAPH` or `ALTER PROPERTY GRAPH` (`base`
    included) that declares the table with `SOURCE KEY (a) REFERENCES x
    (_key) DESTINATION KEY (b) REFERENCES y (_key)` fixes its direction: `a`
    and `b` are two distinct `REFERENCES` columns naming `x` and `y`. Its
    edges are written under a new edge type named by its first `LABEL`, else
    its table name -- with the schema in front for a table outside `public`
    (`geo.road`), so two schemas' same-named edge tables never share a type.
2.3 A direction is fixed once. Any later graph names the table as it is, with
    or without `SOURCE KEY ... DESTINATION KEY ...`; ends that differ are
    refused, naming the fixed ones.
2.4 `ALTER PROPERTY GRAPH base DROP EDGE TABLES (t)` takes the direction back.
    It is refused while `t` has an edge (`DELETE FROM t` first) and while a
    named graph shows `t` (remove it there first): an edge must always have
    the table it is read and deleted through.
2.5 An edge type an edge table owns takes no write from the untyped calls
    (`link`, `put_edge`, `create_edge`): its edges carry typed properties and
    a key, and only the edge table checks them.
2.6 An edge table's name is a table name: it shares its schema's table
    namespace.

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

A key that names neither end is refused when the table is declared: it would
need an index over edge properties, which does not exist.

## 4. Writing

4.1 `INSERT` resolves each end's key in the referenced collection: a missing
    row is `23503` (foreign_key_violation). A duplicate key is `23505`
    (unique_violation). A plain `INSERT` never overwrites.
4.2 `INSERT ... ON CONFLICT (key columns) DO UPDATE SET c = EXCLUDED.c` and
    `DO NOTHING` are the upsert.
4.3 `UPDATE t SET c = ... WHERE <every key column> = ...` rewrites the
    properties of that edge. A key or end column is not assignable: a new
    identity is a `DELETE` and an `INSERT`.
4.4 `DELETE FROM t WHERE <source and/or destination> = ... [AND c = ...]`
    removes every edge that matches.
4.5 An `UPDATE` or `DELETE` that names no end is refused: it would read every
    edge of the type.

## 5. Property graphs: named views

5.1 `CREATE PROPERTY GRAPH g VERTEX|NODE TABLES (...) EDGE TABLES (...)`
    records a definition. Each element table is written
    `t [AS alias] [SOURCE KEY ... DESTINATION KEY ...] [LABEL l | DEFAULT
    LABEL] ... [PROPERTIES ARE ALL COLUMNS]`:
    - the ELEMENT NAME is the alias, else the table name without its schema
      (`usa.city` is `city`); element names are unique within the graph,
      across vertex and edge tables, so `usa.city` and `china.city` in one
      graph need `AS`;
    - with no label clause an element has one label, its element name; a
      `LABEL` clause replaces that default, and `DEFAULT LABEL` keeps it
      beside the others (`usa.city AS usa_city DEFAULT LABEL LABEL city`);
    - a label shared by several tables shows the same properties on each,
      or is refused naming the column that differs;
    - an edge table's two end tables are vertex tables of the same graph.
5.2 Any number of graphs may show the same tables. A graph stores no edge:
    `GRAPH_TABLE (g MATCH ...)` walks the base graph's edges through g's
    names, as fast as the base graph.
5.3 In a named graph a label is one its definition gives, and an unlabeled
    `(n)` or `-[e]->` covers the graph's own vertex tables and edge tables.
5.4 `ALTER PROPERTY GRAPH g` takes, in any number and order:
    `ADD VERTEX|EDGE TABLES (...)`, `DROP VERTEX|EDGE TABLES (element, ...)`,
    and `ALTER VERTEX|EDGE TABLE element ADD|DROP LABEL l`. Removing a table
    from a named graph deletes nothing. A vertex table an edge table of the
    graph reaches cannot leave it first; an element keeps at least one label;
    a graph keeps at least one element (`DROP PROPERTY GRAPH` removes it).
5.5 `CREATE OR REPLACE PROPERTY GRAPH g ...` rewrites g's definition whole.
    `DROP PROPERTY GRAPH [IF EXISTS] g` removes it. Neither touches an edge.
5.6 A property graph's name may not be one a graph context (the partitions
    edges are written into through the API) already uses.

## 6. The base graph

6.1 `base` always exists, holds every table and every edge, and is neither
    created, replaced nor dropped.
6.2 A label there is a table's own name, in any schema, or a label `base` was
    given (`ALTER PROPERTY GRAPH base ALTER VERTEX TABLE china.city ADD LABEL
    china_city`). A name that reaches two tables (`city` for `usa.city` and
    `china.city`) is refused, naming both; the quoted `"china.city"` picks one,
    in the base graph only.
6.3 `ALTER PROPERTY GRAPH base ADD EDGE TABLES (t SOURCE KEY ... DESTINATION
    KEY ...)` fixes `t`'s direction (§2.2); `ADD VERTEX TABLES` changes
    nothing (every table is there) and says so; `DROP VERTEX TABLES` is
    refused; `DROP EDGE TABLES` takes a direction back (§2.4).
6.4 A label is never schema-qualified outside that quoted form:
    `-[r:usa.lives_in]->` is refused, saying the schema belongs in a graph's
    definition.

## 7. Reading an edge table

7.1 `SELECT ... FROM t WHERE source = ... [AND ...]` (or `destination =`)
    reads that node's edges of the type. A `WHERE` that names no end is
    refused, as a `WHERE` on an unindexed column is.

## 8. Refused, by name

`ALTER TABLE`, `TRUNCATE`, triggers and `CREATE INDEX` on an edge table; an
`UPDATE` of a key or end column; `KEY (...)` on an element table (the table's
own key is the element key); a label's `PROPERTIES (...)`, `NO PROPERTIES`
and `PROPERTIES ARE ALL COLUMNS EXCEPT` (a label shows every column); `DROP
... CASCADE` on a graph's tables (removing one deletes nothing); a key that
names no end.
