# `sekejap serve` — HTTP/JSON server (design)

> Default port **5918**.

## Goals
- One command turns the embedded DB into a networked service (the Ollama model).
- Universal access over plain HTTP — no driver, any language.
- Double as a single-binary **search engine** (BM25 + vector + spatial + graph) —
  the "DuckDB of search+graph," not a distributed Elasticsearch.
- Safe by default: localhost bind, auth required before public exposure.

## Non-goals (v1)
- Distributed sharding / replication (single-node).
- Postgres wire protocol (separate later track → unlocks DBeaver + PG tools).
- Linguistic vagueness (NL time / semantic query parsing) — resolved to concrete
  SQL by the caller or a higher `serve` layer, not the query engine.

## Process model & CLI
```
sekejap serve <db-path> [--host 127.0.0.1] [--port 5918] \
              [--key <master-key>] [--read-only] [--tls-cert … --tls-key …]
```
- Opens the DB through the existing Engine concurrency layer (concurrent reads;
  writes serialize through the WAL).
- Binds 127.0.0.1 by default. Public bind (`0.0.0.0`) REQUIRES `--key` — refuse to
  start public without auth.
- Graceful shutdown flushes the WAL.

## Security (Meilisearch-style)
- Master key (`--key` / `SEKEJAP_KEY`): full access.
- Scoped keys (admin API): `read` / `write` / per-collection, optional expiry;
  sent as `Authorization: Bearer <key>`.
- No key + localhost → open (dev). No key + public bind → hard error.
- Optional TLS (rustls); otherwise front with a reverse proxy.

## HTTP API

### Core — SQL (full power)
```
POST /query            { "sql": "SELECT GRAPH FROM MATCH …", "params": [...] }
POST /query/prepared   { "sql": "...$1...", "params": [...] }   # reuses prepare cache
```
Envelope:
```json
{ "ok": true, "rows": [ … ], "row_count": 12, "took_ms": 3 }
// SELECT GRAPH → { "ok": true, "graph": { "nodes":[…], "edges":[…] }, "took_ms": 5 }
```

### Search — ergonomic Meili/ES-shaped layer (no SQL)
```
POST /collections/{c}/search
{
  "q": "grilled chicken",
  "vector": [0.7,0.3,0.0,0.0],
  "hybrid": { "text": 0.6, "vector": 0.4 },
  "filter": "protein_g >= 25 AND open_now = true",
  "near":   { "lat": -8.69, "lon": 115.168, "km": 5 },
  "limit": 10, "offset": 0
}
→ { "hits": [ { "score": 0.87, "document": {…} } ], "estimated_total": 42, "took_ms": 6 }
```
Compiles to the existing hybrid `ORDER BY BM25(...)*w1 + VECTOR_COSINE(...)*w2` +
`ST_DWithin` + `WHERE`. (Typo tolerance is a later opt-in via the term FST —
`fst` levenshtein — no new index, zero impact on other queries.)

### Ingest (bulk)
```
POST   /collections/{c}/documents   [ {doc}, … ]     # bulk upsert
DELETE /collections/{c}/documents/{key}
POST   /collections/{c}/edges       [ {from,to,type,attrs}, … ]
```

### Admin / schema
```
POST /collections           { "name": …, "schema": …, "indexes": ["bm25(description)","hnsw(embedding)"] }
GET  /collections           # list + counts
GET  /collections/{c}/stats
POST /keys · GET /keys · DELETE /keys/{id}
```

### Ops / migration
```
GET  /health   → { "ok": true }
GET  /version  → { "version": "0.14.0" }
POST /admin/compact
GET  /admin/dump     → NDJSON stream (backup / move between servers)
POST /admin/restore  ← NDJSON stream
```

## Errors
```json
{ "ok": false, "error": { "code": "sql_parse", "message": "…", "hint": "use length(p) not p.length" } }
```
400 bad query · 401/403 auth · 404 · 409 write conflict · 500.

## Dependency strategy
- Whole server behind a **`serve` cargo feature**, compiled only into `sekejap-cli`.
  The embedded `sekejap` library stays dep-light.
- HTTP stack: `axum` + `tokio` (recommended) vs `tiny_http` (minimal deps) — DECISION.

## Positioning (honest)
Win on the **combination**: search + vector + spatial + **graph** + SQL, one binary,
run like Ollama. Do not claim Meilisearch's typo/analyzer depth or Elasticsearch's
cluster scale. Nobody else has graph + search together.

## Phasing
1. **M1 — DONE** — `serve` + `/query` + `/health` + `/version` + master-key auth +
   localhost-default (public bind refused without a key), read-only flag, `SELECT
   GRAPH` surfaced as `graph`. `Arc<RwLock<CoreDB>>` (concurrent reads, serial writes),
   feature-gated + default-on in the CLI, axum + tokio.
2. **M2** — `/collections/{c}/search` hybrid + bulk `/documents` ingest.
3. **M3** — scoped keys, TLS, `dump`/`restore` migration.
4. **M4** (separate track) — Postgres wire → DBeaver + PG tool ecosystem.

## Open decisions
1. HTTP lib: `axum`+tokio vs `tiny_http`.
2. Search endpoint in M1 or M2.
