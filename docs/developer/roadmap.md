# Roadmap

Where sekejap is going. This is direction, not a dated commitment — priorities
shift with what users need. Shipped work lives in the rest of the docs; this
page is the intent behind the next stretch.

## The shape of the goal

sekejap already unifies relational, graph, spatial, vector, and text retrieval
in one embedded engine with one query surface (SGQL). The next chapter is about
making that power **pleasant to use in every language**, not just reachable.

Today most wrappers speak SGQL as strings and hand back JSON. That works, but
the databases developers reach for on mobile and the backend — Isar, ObjectBox,
Realm, Drift, Room, GRDB, Prisma, Drizzle, SQLModel — win on ergonomics as much
as on speed: typed models, a fluent query builder, results that update
themselves, and idiomatic async. The roadmap adopts that recipe, adapted to
each ecosystem's idioms rather than copied verbatim.

## A typed, fluent, reactive surface per language

The aim is one conceptual API expressed idiomatically:

```text
db.dishes
  .match(Restaurant.serves)                 // graph traversal
  .where(d => d.openNow == true)            // scalar filter
  .near(d => d.geometry, here, metres: 5000)// spatial
  .bm25(d => d.description, "grilled")      // text relevance
  .rankBy(d => d.embedding.cosine(taste))  // vector
  .watch()                                  // reactive stream
```

The builder compiles to the same SGQL plan the engine already runs — it is a
typed front-end, not a second engine. What differs per language is only the
surface:

| ecosystem | model form | generation | reactive type |
|---|---|---|---|
| Dart / Flutter | annotated class | build_runner | `Stream` |
| Kotlin / Android | annotated data class | KSP | `Flow` |
| Swift / iOS | `@Model`-style macro | Swift macros | `AsyncSequence` |
| TypeScript / Node | schema or decorators | generated `.d.ts` | async iterator |
| Python | Pydantic model | runtime type hints | async iterator |
| Go | struct tags | `go generate` | channels |

A single embedded engine means the TypeScript surface is the same on a device
(React Native) and on a server (Express, Next.js) — the query you write in a
route handler is the query you write in a screen. Python meets its two
audiences at once: Pydantic models with async queries for API developers, and
DataFrames (pandas today, Arrow-based analytics later) for data work.

## What the engine needs to support it

Two primitives underpin the ergonomic layer:

1. **Plan-equivalent builders.** Every fluent query lowers to the same SGQL
   plan, so there is one execution path and one place to optimize.
2. **Change notification.** Reactive queries need to know when to re-emit. Each
   committed transaction publishes which collections, keys, and edge types
   changed, so a watcher can refresh precisely instead of polling.

## Other directions on the horizon

- **Mobile operating profile** — a relaxed-durability, deferred-compaction mode
  suited to phone flash storage, alongside the default power-loss-safe mode.
- **Incremental index maintenance** — segment-based text and vector index
  updates so large text/vector collections stay fast under frequent writes.
- **Faster reopen** — memory-mapped index snapshots so opening a large database
  is close to instant.
- **Schema migrations** — a versioned, transactional migration framework across
  the wrappers.
- **Local-first sync** — an optional replication layer that keeps the
  local-only path unchanged.

These are goals, not guarantees. If you need one of them, an issue or a pull
request is the fastest way to move it up the list.
