# Connectivity — reaching sekejap over a network

sekejap is embedded first: normally you link the library and call it
in-process. When you want a process boundary — dashboards, other machines,
tools that speak a wire protocol — the same engine is reachable through thin
adapters. They add access, not a different database: every path executes the
same SQL against the same file.

## HTTP/JSON — `sekejap serve`

```sh
sekejap serve ./mydb --host 127.0.0.1 --port 5918
```

| endpoint | what it does |
|---|---|
| `POST /query` | run SQL; body `{ "sql": "...", "params": [...] }` with PostgreSQL-style `$1` placeholders |
| `POST /graph` | upsert nodes and edges in one JSON body |
| `DELETE /graph` | remove nodes/edges |
| `GET /health` | `{ "ok": true }` |
| `GET /version` | crate version |

```sh
curl -s localhost:5918/query -d '{
  "sql": "SELECT name FROM places WHERE category = $1 LIMIT 5",
  "params": ["temple"]
}'
# → { "ok": true, "rows": [...], "row_count": 5, "took_ms": 1 }
```

`SELECT GRAPH FROM MATCH …` returns `{ "graph": { "nodes": […], "edges": […] } }`
instead of rows — ready for graph visualisation libraries.

Flags worth knowing: `--read-only` rejects all writes; binding beyond
localhost requires an auth key (safe by default).

## PostgreSQL wire — `sekejap pg`

```sh
sekejap pg ./mydb --port 5433
psql -h 127.0.0.1 -p 5433 -d mydb
```

Any client that speaks the Postgres protocol — `psql`, DBeaver, JDBC/pgjdbc,
BI tools — can connect and issue the full SGQL surface, including
`SELECT … FROM MATCH`. Current implementation is a localhost-trust listener:
convenient for tools and dashboards, not an exposed production server.

## Object storage (read-only scale-out)

With the `s3` feature, a writer can publish compacted database state to
S3-compatible object storage, and many lightweight readers can open it
read-only, fetching payload blocks on demand with range reads and a bounded
cache. This is deliberately **not** a distributed database — no consensus, no
cross-node transactions — just a low-complexity way to serve many readers from
one published snapshot.

## Choosing a path

| you want | use |
|---|---|
| lowest latency, no ops | embedded (default) |
| browser/dashboard/another language over HTTP | `sekejap serve` |
| existing SQL tooling (psql, DBeaver, BI) | `sekejap pg` |
| many readers, one writer, cloud storage | S3 read-only mode |

The adapters are feature-gated (`serve`, `pg`, `s3`) and live outside the
core: the minimal embedded build carries none of them.
