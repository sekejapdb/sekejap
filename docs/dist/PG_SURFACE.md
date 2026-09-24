# The PostgreSQL catalog surface

What a PostgreSQL client can read out of a sekejap database, and what it
cannot. `docs/lang/QL_CONTRACT.md` §1 row "catalog" is the contract; this
document is the LIST: every relation, every column, every type OID, and the
statements real clients issue against them.

Nothing here is stored and nothing here is a second source of truth. Every
row is COMPUTED at prepare from the catalog readers the engine already has --
`Database::list_collections`, `collection_info`, `list_indexes`, `row_count`,
`graph_names`, `edge_shape` -- into a typed value list the compiled statement
owns (`lang/src/catalog.rs`). A `SELECT` over one of them is an ordinary
`SELECT` whose driver is the bounded in-memory row source named in
`lang/src/compile/rows.rs`; `WHERE`, `ORDER BY`, `LIMIT` and `DISTINCT`
compose over that list. No disk format changed and no bit was spent.

## 1. The bound

| relation | one row per | cost of building it |
|---|---|---|
| `db_tables` | collection | one catalog-name walk, one descriptor read and one LIVE ROW COUNT point read per collection |
| `db_columns` | column of a collection | the same descriptors; no row is read |
| `db_indexes` | index | one index-registry walk per collection |
| `db_contexts` | interned graph context, plus the base graph | one name-dictionary walk, capped at 4,096 names per kind by `create_graph_name` |
| `db_edges` | distinct `(context, edge type, from collection, to collection)` | **the one probe that is not catalog-bounded**: `Database::edge_shape` pays one descent per distinct `(source entity, context, type, destination collection)` and seeks past each run, so it is proportional to the SOURCE ENTITIES that have edges, never to the edges. Capped at `EDGE_SHAPE_SEEKS` = 65,536 descents; past the cap the answer carries a NOTICE saying it stopped, rather than being short and looking complete |

Every `pg_catalog`, `information_schema` and PostGIS relation below is a
fixed projection of those five, so each costs what its source costs.
`EXPLAIN` over any of them prints `driver: rows(<relation>)`, the number of
rows the catalog produced before the filters, and the line "no index is
opened, no row is read".

## 2. Why `version()` starts with `PostgreSQL`

```text
SELECT version()  ->  PostgreSQL 16.0 (sekejap 0.17.0)
```

```sql
SELECT version();
SELECT db_version()
```

Drivers PARSE it. pgjdbc reads the major number out of it to choose which
protocol features and which catalog queries to use; psql prints it and
compares it against its own; the QGIS provider shows it as the provider
description. A string that does not begin `PostgreSQL <major>.<minor>` is
either rejected outright or silently degrades the client to its oldest
behaviour -- so the shape is PostgreSQL's and the honest part, which engine
actually answered, goes in the parenthesis PostgreSQL itself uses for the
build. `SELECT db_version()` answers `sekejap 0.17.0` with no costume, for a
caller that is not a driver.

## 3. The session facts

| statement | answers | column |
|---|---|---|
| `SELECT version()` | `PostgreSQL 16.0 (sekejap 0.17.0)` | `version` |
| `SELECT db_version()` | `sekejap 0.17.0` | `db_version` |
| `SELECT current_schema()` | `public` | `current_schema` |
| `SELECT current_database()` | `sekejap` | `current_database` |
| `SELECT current_user` / `session_user` / `current_role` | `postgres` | `current_user` |
| `SELECT pg_backend_pid()` | this process's id | `pg_backend_pid` |
| `SELECT current_setting('<name>')` | the constant below, or NULL | `current_setting` |
| `SELECT 1` | `1` | `?column?` |

Every one of them, run on [the example fixture](../lang/EXAMPLE_FIXTURE.md):

```sql
SELECT version();
SELECT db_version();
SELECT current_schema();
SELECT current_database();
SELECT current_schema(), session_user;
SELECT current_user;
SELECT pg_backend_pid();
SELECT current_setting('client_encoding');
SELECT 1
```

There is no authentication and no cluster: sekejap is one database per file and a
connection is a process, so `current_user` is a constant and
`pg_backend_pid()` is the operating system's own answer for this process
rather than a fabricated number.

## 4. Client settings

A driver `SET`s these on connect. Each is accepted as a NOTICE that names the
knob and the value, and each answers `SHOW <name>` from the constant below.
Nothing is stored: there is no session settings object, and a silent `SET`
would read as one that took effect. `SHOW extra_float_digits` therefore still
answers `1` after `SET extra_float_digits = 3` -- the truth, not the echo.

| setting | value | spellings also accepted |
|---|---|---|
| `client_encoding` | `UTF8` | |
| `server_encoding` | `UTF8` | |
| `server_version` | `16.0` | |
| `datestyle` | `ISO, MDY` | `DateStyle` |
| `intervalstyle` | `postgres` | |
| `timezone` | `UTC` | `SET TIME ZONE`, `SHOW TIME ZONE` |
| `application_name` | (empty) | |
| `search_path` | `"$user", public` | |
| `extra_float_digits` | `1` | |
| `standard_conforming_strings` | `on` | |
| `integer_datetimes` | `on` | |
| `transaction_isolation` | `read committed` | `SHOW TRANSACTION ISOLATION LEVEL` |
| `default_transaction_isolation` | `read committed` | |
| `transaction_read_only` | `off` | |
| `is_superuser` | `on` | |
| `max_identifier_length` | `63` | |
| `bytea_output` | `hex` | |
| `session_authorization` | `postgres` | |

The settings a driver sends on connect, and the truth `SHOW` answers after
them:

```sql
SET client_encoding = 'UTF8';
SET DateStyle TO 'ISO';
SET application_name = 'DBeaver 24.0';
SET extra_float_digits = 3;
SET TIME ZONE 'UTC';
SHOW client_encoding;
SHOW server_version;
SHOW TRANSACTION ISOLATION LEVEL;
-- Still 1, not 3: the notice above named the knob, it did not store it.
SHOW extra_float_digits
```

`SET <anything else> = <value>` is also a notice, and says the knob is not one
this engine has. The two knobs that DO change something keep their own route
(`SET LOCAL ef_search`, `SET LOCAL diskann.query_search_list_size`:
`docs/lang/QL_CONTRACT.md` §4.5).

## 5. The `SHOW` family

Each is fixed sugar over one `db_*` relation and adds no atomic
(`docs/lang/QL_CONTRACT.md` §2).

| statement | relation | columns |
|---|---|---|
| `SHOW TABLES` | `db_tables` | `name, id, rows, fields` |
| `SHOW <collection>` | `db_columns`, filtered to it | `name, kind, declared_type, position, not_null, has_default` |
| `SHOW INDEXES [ON t]` | `db_indexes` | `name, family, field, expression, state` |
| `SHOW EDGES` | `db_edges` | `edge_type, from_table, to_table, context` |
| `SHOW CREATE TABLE t` | the descriptor | `create_table` -- the `CREATE TABLE` and one `CREATE INDEX` per index, built from the descriptor so it cannot disagree with what is there |
| `SHOW <setting>` | §4 | the setting's name |

```sql
SHOW TABLES;
SHOW posts;
SHOW INDEXES;
SHOW INDEXES ON posts;
SHOW EDGES;
SHOW CREATE TABLE posts
```

`SHOW EDGES FROM t` is the one form of the family that is NOT accepted, and
the refusal says the filter is a `WHERE` over `db_edges`:

```sql
SELECT edge_type, from_table, to_table FROM db_edges WHERE from_table = 'people'
```

`SHOW <name>` is ambiguous by construction and is resolved against the
catalog: a collection of that name wins, then a setting of that name, and
neither is an error that names only one of the two. `SHOW EDGES FROM t` is
NOT accepted -- the filter is a `WHERE` over `db_edges`, and the refusal says
so. `SHOW STATUS` and `SHOW STORAGE` stay Tier 2 (`docs/dist/OPS_CONTRACT.md`
§6).

## 6. Type OIDs

The numbers are PostgreSQL's own and frozen in its catalog. The two EXTENSION
types have no fixed number anywhere -- PostGIS and pgvector are assigned
theirs at `CREATE EXTENSION` time and every client detects them by type NAME
-- so these two are ours, and are written down here so a wire layer and a
test agree on them.

| sekejap `Kind` (and declared spelling) | `typname` | OID | `information_schema.columns.data_type` |
|---|---|---|---|
| `Text` | `text` | 25 | `text` |
| `Int` | `int8` | 20 | `bigint` |
| `Int` declared `TIMESTAMPTZ` | `timestamptz` | 1184 | `timestamp with time zone` |
| `Int` declared `DATE` | `date` | 1082 | `date` |
| `Real` | `float8` | 701 | `double precision` |
| `Bool` | `bool` | 16 | `boolean` |
| `Json` | `jsonb` | 3802 | `jsonb` |
| `Geo`, `Point` | `geometry` | 18000 | `USER-DEFINED` |
| `Vector(n)` | `vector` | 18001 | `USER-DEFINED` |
| (no `Kind`; listed for a wire layer) | `bytea` | 17 | `bytea` |

`TIMESTAMPTZ` and `DATE` are both `Kind::Int` (`docs/lang/QL_CONTRACT.md` §5
deviation 8: UTC microseconds), so only the DECLARED spelling the catalog
recorded tells them apart -- which is why `CollectionInfo::declared` is what
decides the OID and not the `Kind`.

Relation columns also use `oid` (26), `name` (19), `char` (18), `int2` (21),
`int4` (23) and `float4` (700), which are the types PostgreSQL's own catalog
declares those columns with.

## 7. Object OIDs

PostgreSQL hands out OIDs from a counter. sekejap has no such counter and no place
to keep one without a format change, so an object's OID is FNV-1a over its
name, taken into the user-object range (`16,385 + h mod 2e9`) that a client
reads as "not a system object". It is the one number on this surface that is
derived rather than read: two names that collide would report one OID for two
objects, which at 10,000 collections is under 1e-5, and a client that follows
an OID back reaches the relation by name anyway. The same function is what the prior engine
used, so the two surfaces agree on what a collection's OID is.

## 8. The relations

`db_tables` — 4 columns

| column | type | OID |
|---|---|---|
| `name` | text | 25 |
| `id` | int8 | 20 |
| `rows` | int8 | 20 |
| `fields` | int8 | 20 |

`db_columns` — 7 columns

| column | type | OID |
|---|---|---|
| `table` | text | 25 |
| `name` | text | 25 |
| `kind` | text | 25 |
| `declared_type` | text | 25 |
| `position` | int8 | 20 |
| `not_null` | bool | 16 |
| `has_default` | bool | 16 |

`db_indexes` — 6 columns

| column | type | OID |
|---|---|---|
| `table` | text | 25 |
| `name` | text | 25 |
| `family` | text | 25 |
| `field` | text | 25 |
| `expression` | text | 25 |
| `state` | text | 25 |

`db_edges` — 4 columns

| column | type | OID |
|---|---|---|
| `edge_type` | text | 25 |
| `from_table` | text | 25 |
| `to_table` | text | 25 |
| `context` | text | 25 |

`db_contexts` — 2 columns

| column | type | OID |
|---|---|---|
| `name` | text | 25 |
| `id` | int8 | 20 |

`information_schema.schemata` — 7 columns

| column | type | OID |
|---|---|---|
| `catalog_name` | text | 25 |
| `schema_name` | text | 25 |
| `schema_owner` | text | 25 |
| `default_character_set_catalog` | text | 25 |
| `default_character_set_schema` | text | 25 |
| `default_character_set_name` | text | 25 |
| `sql_path` | text | 25 |

`information_schema.tables` — 12 columns

| column | type | OID |
|---|---|---|
| `table_catalog` | text | 25 |
| `table_schema` | text | 25 |
| `table_name` | text | 25 |
| `table_type` | text | 25 |
| `self_referencing_column_name` | text | 25 |
| `reference_generation` | text | 25 |
| `user_defined_type_catalog` | text | 25 |
| `user_defined_type_schema` | text | 25 |
| `user_defined_type_name` | text | 25 |
| `is_insertable_into` | text | 25 |
| `is_typed` | text | 25 |
| `commit_action` | text | 25 |

`information_schema.columns` — 15 columns

| column | type | OID |
|---|---|---|
| `table_catalog` | text | 25 |
| `table_schema` | text | 25 |
| `table_name` | text | 25 |
| `column_name` | text | 25 |
| `ordinal_position` | int8 | 20 |
| `column_default` | text | 25 |
| `is_nullable` | text | 25 |
| `data_type` | text | 25 |
| `character_maximum_length` | int8 | 20 |
| `numeric_precision` | int8 | 20 |
| `numeric_scale` | int8 | 20 |
| `datetime_precision` | int8 | 20 |
| `udt_catalog` | text | 25 |
| `udt_schema` | text | 25 |
| `udt_name` | text | 25 |

`information_schema.table_constraints` — 10 columns

| column | type | OID |
|---|---|---|
| `constraint_catalog` | text | 25 |
| `constraint_schema` | text | 25 |
| `constraint_name` | text | 25 |
| `table_catalog` | text | 25 |
| `table_schema` | text | 25 |
| `table_name` | text | 25 |
| `constraint_type` | text | 25 |
| `is_deferrable` | text | 25 |
| `initially_deferred` | text | 25 |
| `enforced` | text | 25 |

`information_schema.key_column_usage` — 9 columns

| column | type | OID |
|---|---|---|
| `constraint_catalog` | text | 25 |
| `constraint_schema` | text | 25 |
| `constraint_name` | text | 25 |
| `table_catalog` | text | 25 |
| `table_schema` | text | 25 |
| `table_name` | text | 25 |
| `column_name` | text | 25 |
| `ordinal_position` | int8 | 20 |
| `position_in_unique_constraint` | int8 | 20 |

`pg_catalog.pg_namespace` — 4 columns

| column | type | OID |
|---|---|---|
| `oid` | oid | 26 |
| `nspname` | name | 19 |
| `nspowner` | oid | 26 |
| `nspacl` | text | 25 |

`pg_catalog.pg_class` — 25 columns

| column | type | OID |
|---|---|---|
| `oid` | oid | 26 |
| `relname` | name | 19 |
| `relnamespace` | oid | 26 |
| `reltype` | oid | 26 |
| `relowner` | oid | 26 |
| `relam` | oid | 26 |
| `relpages` | int4 | 23 |
| `reltuples` | float4 | 700 |
| `reltoastrelid` | oid | 26 |
| `relhasindex` | bool | 16 |
| `relisshared` | bool | 16 |
| `relpersistence` | char | 18 |
| `relkind` | char | 18 |
| `relnatts` | int2 | 21 |
| `relchecks` | int2 | 21 |
| `relhasrules` | bool | 16 |
| `relhastriggers` | bool | 16 |
| `relhassubclass` | bool | 16 |
| `relrowsecurity` | bool | 16 |
| `relispopulated` | bool | 16 |
| `relreplident` | char | 18 |
| `relispartition` | bool | 16 |
| `reltablespace` | oid | 26 |
| `relacl` | text | 25 |
| `reloptions` | text | 25 |

`pg_catalog.pg_attribute` — 19 columns

| column | type | OID |
|---|---|---|
| `attrelid` | oid | 26 |
| `attname` | name | 19 |
| `atttypid` | oid | 26 |
| `attstattarget` | int4 | 23 |
| `attlen` | int2 | 21 |
| `attnum` | int2 | 21 |
| `attndims` | int4 | 23 |
| `atttypmod` | int4 | 23 |
| `attbyval` | bool | 16 |
| `attstorage` | char | 18 |
| `attalign` | char | 18 |
| `attnotnull` | bool | 16 |
| `atthasdef` | bool | 16 |
| `attidentity` | char | 18 |
| `attgenerated` | char | 18 |
| `attisdropped` | bool | 16 |
| `attislocal` | bool | 16 |
| `attinhcount` | int4 | 23 |
| `attcollation` | oid | 26 |

`pg_catalog.pg_type` — 18 columns

| column | type | OID |
|---|---|---|
| `oid` | oid | 26 |
| `typname` | name | 19 |
| `typnamespace` | oid | 26 |
| `typowner` | oid | 26 |
| `typlen` | int2 | 21 |
| `typbyval` | bool | 16 |
| `typtype` | char | 18 |
| `typcategory` | char | 18 |
| `typispreferred` | bool | 16 |
| `typisdefined` | bool | 16 |
| `typdelim` | char | 18 |
| `typrelid` | oid | 26 |
| `typelem` | oid | 26 |
| `typarray` | oid | 26 |
| `typnotnull` | bool | 16 |
| `typbasetype` | oid | 26 |
| `typtypmod` | int4 | 23 |
| `typndims` | int4 | 23 |

`pg_catalog.pg_index` — 13 columns

| column | type | OID |
|---|---|---|
| `indexrelid` | oid | 26 |
| `indrelid` | oid | 26 |
| `indnatts` | int2 | 21 |
| `indnkeyatts` | int2 | 21 |
| `indisunique` | bool | 16 |
| `indisprimary` | bool | 16 |
| `indisexclusion` | bool | 16 |
| `indimmediate` | bool | 16 |
| `indisclustered` | bool | 16 |
| `indisvalid` | bool | 16 |
| `indisready` | bool | 16 |
| `indislive` | bool | 16 |
| `indkey` | text | 25 |

`pg_catalog.pg_indexes` — 5 columns

| column | type | OID |
|---|---|---|
| `schemaname` | name | 19 |
| `tablename` | name | 19 |
| `indexname` | name | 19 |
| `tablespace` | name | 19 |
| `indexdef` | text | 25 |

`pg_catalog.pg_description` — 4 columns

| column | type | OID |
|---|---|---|
| `objoid` | oid | 26 |
| `classoid` | oid | 26 |
| `objsubid` | int4 | 23 |
| `description` | text | 25 |

`pg_catalog.pg_constraint` — 11 columns

| column | type | OID |
|---|---|---|
| `oid` | oid | 26 |
| `conname` | name | 19 |
| `connamespace` | oid | 26 |
| `contype` | char | 18 |
| `condeferrable` | bool | 16 |
| `condeferred` | bool | 16 |
| `convalidated` | bool | 16 |
| `conrelid` | oid | 26 |
| `contypid` | oid | 26 |
| `conindid` | oid | 26 |
| `conkey` | text | 25 |

`pg_catalog.pg_tables` — 8 columns

| column | type | OID |
|---|---|---|
| `schemaname` | name | 19 |
| `tablename` | name | 19 |
| `tableowner` | name | 19 |
| `tablespace` | name | 19 |
| `hasindexes` | bool | 16 |
| `hasrules` | bool | 16 |
| `hastriggers` | bool | 16 |
| `rowsecurity` | bool | 16 |

`public.geometry_columns` — 7 columns

| column | type | OID |
|---|---|---|
| `f_table_catalog` | text | 25 |
| `f_table_schema` | text | 25 |
| `f_table_name` | text | 25 |
| `f_geometry_column` | text | 25 |
| `coord_dimension` | int8 | 20 |
| `srid` | int8 | 20 |
| `type` | text | 25 |

`public.spatial_ref_sys` — 5 columns

| column | type | OID |
|---|---|---|
| `srid` | int8 | 20 |
| `auth_name` | text | 25 |
| `auth_srid` | int8 | 20 |
| `srtext` | text | 25 |
| `proj4text` | text | 25 |

<!-- 21 relations, 195 columns -->

All 21, each answering over the example fixture:

```sql
SELECT name, id, rows, fields FROM db_tables ORDER BY name ASC;
SELECT * FROM db_columns WHERE table = 'posts';
SELECT * FROM db_indexes;
SELECT * FROM db_edges;
SELECT * FROM db_contexts;
SELECT * FROM information_schema.schemata;
SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = 'public';
SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name = 'posts';
SELECT constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'posts';
SELECT column_name FROM information_schema.key_column_usage WHERE table_name = 'posts';
SELECT oid, nspname FROM pg_namespace ORDER BY nspname ASC;
SELECT oid, relname, relkind, relnatts FROM pg_class WHERE relnamespace = 2200;
SELECT attrelid, attname, atttypid, attnum, attnotnull FROM pg_attribute WHERE attnum > 0;
SELECT oid, typname, typtype, typelem, typlen FROM pg_type;
SELECT indexrelid, indisprimary, indisunique FROM pg_index;
SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'posts';
SELECT objoid, description FROM pg_description;
SELECT conname, contype FROM pg_constraint;
SELECT schemaname, tablename FROM pg_tables WHERE schemaname = 'public';
SELECT upper(type), srid, coord_dimension FROM geometry_columns WHERE f_table_name = 'posts';
SELECT auth_name, auth_srid, srtext, proj4text FROM spatial_ref_sys WHERE srid = 4326
```

## 9. What is NOT provided, and why

Listed rather than answered empty. An empty `pg_settings` reads as "this
server has no settings", which is false, and the eighth law of
`docs/core/FOUNDATION_TEST_STANDARD.md` refuses a construct with no atomic by
a named reason instead. Each of these is a Tier-3 row in `lang/src/refuse.rs`
and a `SELECT` naming one is refused BY NAME.

| relation | why not |
|---|---|
| `pg_proc` | sekejap has no function catalog: the §4.1/§4.2 functions are compiled by `lang`, not registered rows |
| `pg_settings` | sekejap has no GUC table; the client settings a driver sends are accepted as notices and each answers `SHOW <name>` from a constant |
| `pg_roles`, `pg_authid` | no authentication and no role catalog: the process that opened the file is the only user |
| `pg_database` | one database per file; there is no cluster to list. `SELECT current_database()` names this one |
| `pg_enum` | a column's `Kind` is one of eight and none of them is user-defined |
| `pg_operator` | operators are compiled by `lang` against the index families a predicate names |
| `pg_am` | an index family is an `IndexFamily`, a closed set in the collection catalog, not an access-method row |
| `pg_trigger` | triggers are Tier 3: no atomic |
| `pg_rewrite` | a user `CREATE VIEW` is Tier 3: a query rewrite at prepare is a second planner path |
| `pg_stat_activity` | there is no connection table: a connection is a process |
| `postgis_version()` | withheld until the geometry I/O it advertises exists (p3-geometry-io). A client reads the string as a PROMISE that `ST_AsBinary`, `ST_GeomFromWKB` and `&&` answer, and they do not |

Each of them, refused:

```sql refused
-- refused 0A000: pg_settings
SELECT name, setting FROM pg_settings
```

```sql refused
-- refused 0A000: pg_proc
SELECT oid, proname FROM pg_proc
```

```sql refused
-- refused 0A000: pg_database
SELECT datname FROM pg_database
```

```sql refused
-- refused 0A000: postgis_version()
SELECT postgis_version()
```

`pg_description` IS provided and is EMPTY, which is different: sekejap records no
comment on any object, so empty is the true answer rather than a stand-in.

## 10. The statements a client issues

The acceptance list is `lang/tests/sql_catalog.rs::CLIENT_STATEMENTS`, and
the test is that every one of them ANSWERS -- with rows or with no rows, but
never with an error, because a client that meets an error here stops before
it has listed anything. They were collected from the prior engine's
`src/pg.rs` shim (the keys it pattern-matched on), its
`tests/catalogue_and_ddl.rs`, and
the QGIS PostGIS provider's own SQL.

**On connect (pgjdbc / DBeaver 24):** `SELECT version()`,
`SELECT current_schema()`, `SELECT current_database()`,
`SELECT current_schema(), session_user`, `SELECT pg_backend_pid()`,
`SELECT 1`, `SET client_encoding = 'UTF8'`, `SET DateStyle TO 'ISO'`,
`SET application_name = '...'`, `SET extra_float_digits = 3`,
`SHOW TRANSACTION ISOLATION LEVEL`, `SHOW client_encoding`,
`SHOW server_version`.

**Type resolution and the column probe:**
`SELECT oid, typname, typtype, typelem, typlen FROM pg_type`,
`SELECT oid, typname FROM pg_type WHERE oid = 18000`,
`SELECT * FROM <t> WHERE 1 <> 1 LIMIT 1`.

**Expanding a schema tree:** `SELECT oid, nspname FROM pg_namespace`,
`SELECT oid, relname, relkind, relnatts FROM pg_class WHERE relnamespace = 2200`,
`SELECT attrelid, attname, atttypid, attnum, attnotnull FROM pg_attribute WHERE attnum > 0`,
`SELECT schemaname, tablename FROM pg_tables WHERE schemaname = 'public'`,
`SELECT objoid, description FROM pg_description`.

**psql `\dt` / `\d <table>`, in the single-relation form:**
`SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename ASC`,
`SELECT indexname, indexdef FROM pg_indexes WHERE tablename = '<t>'`,
`SELECT indexrelid, indisprimary, indisunique FROM pg_index WHERE indrelid = <oid>`,
`SELECT conname, contype FROM pg_constraint WHERE conrelid = <oid>`.

**`information_schema`, which every ORM and reporting tool reads:** the five
relations, filtered by `table_schema` / `table_name`.

**QGIS, the two relations a spatial layer cannot load without:**
`SELECT upper(type), srid, coord_dimension FROM geometry_columns WHERE f_table_name = '<t>'`,
`SELECT auth_name, auth_srid, srtext, proj4text FROM spatial_ref_sys WHERE srid = 4326`.

Every statement above, run in order on the example fixture — the connect
sequence, the type probe, the schema tree, `\dt` / `\d`,
`information_schema`, and QGIS's two relations:

```sql
SELECT version();
SELECT current_schema();
SELECT current_database();
SELECT current_schema(), session_user;
SELECT pg_backend_pid();
SELECT 1;
SET client_encoding = 'UTF8';
SET DateStyle TO 'ISO';
SET application_name = 'DBeaver 24.0';
SET extra_float_digits = 3;
SHOW TRANSACTION ISOLATION LEVEL;
SHOW client_encoding;
SHOW server_version;

SELECT oid, typname, typtype, typelem, typlen FROM pg_type;
SELECT oid, typname FROM pg_type WHERE oid = 18000;
SELECT * FROM posts WHERE 1 <> 1 LIMIT 1;

SELECT oid, nspname FROM pg_namespace ORDER BY nspname ASC;
SELECT oid, relname, relkind, relnatts FROM pg_class WHERE relnamespace = 2200;
SELECT attrelid, attname, atttypid, attnum, attnotnull FROM pg_attribute WHERE attnum > 0;
SELECT schemaname, tablename FROM pg_tables WHERE schemaname = 'public';
SELECT objoid, description FROM pg_description;

SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename ASC;
SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'posts';
SELECT indexrelid, indisprimary, indisunique FROM pg_index;
SELECT conname, contype FROM pg_constraint;

SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = 'public';
SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name = 'posts';
SELECT schema_name FROM information_schema.schemata;
SELECT constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'posts';
SELECT column_name FROM information_schema.key_column_usage WHERE table_name = 'posts';

SELECT upper(type), srid, coord_dimension FROM geometry_columns WHERE f_table_name = 'posts';
SELECT f_table_schema, f_table_name, f_geometry_column FROM geometry_columns;
SELECT auth_name, auth_srid, srtext, proj4text FROM spatial_ref_sys WHERE srid = 4326
```

### What a client writes that this surface REFUSES

psql's own `\d` and DBeaver's schema-tree query are not the statements above:
each is a JOIN of `pg_class` with `pg_namespace` (and `CASE`, the regex
operator `!~`, `::regclass`, `pg_get_userbyid()`, `format_type()`,
`obj_description()`, `= ANY(...)`). `JOIN` is Tier 2 BY NAME in
`docs/lang/QL_CONTRACT.md` §4.8 with the atomic stated, and `CASE`, `!~` and
`::regclass` have no Tier-1 form either. They are refused with those reasons
rather than answered by a second planner over the row list -- an in-memory
join over two catalog views would be a join engine that exists only here,
which is exactly the emulation the eighth law forbids. A client that needs
them needs §4.8, and the single-relation statements above are what this
surface serves until then.

```sql refused
-- refused 0A000: JOIN
SELECT c.relname, n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
```

Two shapes the row list DOES serve that a stored collection does not, and both
run:

```sql
SELECT * FROM posts WHERE 1 <> 1 LIMIT 1;
SELECT c.relname, c.relkind FROM pg_catalog.pg_class c LIMIT 5
```


* `WHERE 1 <> 1` -- the parser folds a constant predicate to its truth value,
  so FALSE compiles to `LIMIT 0` and TRUE to no predicate at all. This is
  the probe pgjdbc uses to learn a result's COLUMNS without fetching a row,
  and it works over a collection too, for the same reason: the truth value
  was decided at parse and there is nothing left for an index to answer. A
  constant nested inside an `OR` or a `NOT` is not that shape and is refused
  by name.
* a table ALIAS (`FROM pg_catalog.pg_class c`) -- read and dropped, because
  there is one source and a qualified column has one thing it can mean.

## 11. Schema qualification

`docs/lang/QL_CONTRACT.md` §2 places `CREATE SCHEMA` and a real schema segment
in Tier 2 (p2-schema-segment), so there is exactly one user schema:

* `public.t` IS `t` -- the qualifier is read and dropped.
* `pg_catalog.x` and bare `x` are the same relation; `pg_catalog` is on every
  session's `search_path` implicitly, as in PostgreSQL.
* `information_schema.tables` and `information_schema.columns` resolve ONLY
  when qualified. Their bare spellings are ordinary names a collection may
  have, and a collection named `tables` must keep meaning itself.
* Three dotted segments are the `CREATE SCHEMA` refusal, unchanged.

```sql
SELECT _key, title FROM public.posts LIMIT 2;
SELECT relname FROM pg_catalog.pg_class LIMIT 2;
SELECT relname FROM pg_class LIMIT 2;
SELECT table_name FROM information_schema.tables LIMIT 2
```

## 12. Where the code is

| what | file |
|---|---|
| the relation directory, the OIDs, the row builders | `lang/src/catalog.rs` |
| the `Rows` driver: filter, order, distinct, limit over the list | `lang/src/compile/rows.rs` |
| the FROM-less `SELECT`, `SHOW`, and client `SET` | `lang/src/parser/catalog.rs` |
| the graph name dictionary and the graph shape | `core/engine/src/index/graph/mod.rs` (`Database::graph_names`, `Database::edge_shape`) |
| the acceptance list and the oracle | `lang/tests/sql_catalog.rs` |
