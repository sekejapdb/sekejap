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
surface.

## One umbrella, native idioms

Every language surface is a thin front-end over the same three things: the C
ABI, the SGQL plan it lowers to, and the change feed it subscribes to. The
engine never changes to add a language — a binding just (1) calls the C ABI,
(2) compiles its idiomatic query form to the SGQL plan, and (3) maps the change
feed to the language's reactive primitive. That contract is what lets each
ecosystem feel native without forking the core.

So each binding is shaped for the community that actually uses that language,
in the idiom they already reach for — the goal is to feel *native*, not ported.
Each surface is designed for that language's **primary audience first**, rather
than trying to suit every possible user. Focusing on the people most likely to
reach for the language keeps the surface small, coherent, and stable; a binding
that tries to serve everyone tends to grow more interfaces than any one user
needs. The `shaped for` column names that primary audience.

**Wrapper philosophy.** Each wrapper is written the way a fan of that language
would write it, showcasing that language's strongest trait — while all wrappers
follow the *same conceptual flow* (model → typed collection → fluent multi-model
query → run / watch → write). The surface is idiomatic and deliberately differs
between languages (`limitTo` in Dart, `limit` in TypeScript; `Stream` vs `Flow`
vs async iteration for `watch`); only the flow is shared. And once a language's
surface ships, it is meant to last — additive-only, so code written today keeps
working. See [the vocabulary guide](../usage/vocabulary.md).

| language | shaped for | interface form | reactive |
|---|---|---|---|
| **Dart / Flutter** | Flutter app & game (Flame) developers | annotated `@Entity` classes, build_runner codegen, widget-ready results | `Stream` |
| **Kotlin** | Android app developers | annotated data classes, KSP codegen, coroutine-first | `Flow` (Compose) |
| **Swift** | iOS / macOS app developers | `@Model`-style macro, SwiftUI `@Query`-style property wrapper | `AsyncSequence` |
| **TypeScript** | full-stack web **and** React Native | typed client from a schema (Prisma/Drizzle feel), same code on device and server | async iterator / hooks |
| **Python** | data engineers, analysts & scientists, and data-driven backends | SQL as the query surface, DataFrame in/out (`query → DataFrame`, `DataFrame → nodes/edges`), notebook & REPL flow | async iterator |
| **Go** | backend & cloud engineers | struct tags, `go generate`, context-aware | channels |
| **C / C++** | game engines, robotics, systems | the stable C ABI itself; C++ gets RAII typed wrappers | callbacks |
| **C# / .NET** | **Unity game developers** (also .NET backends) | a **LINQ** provider so queries feel built into the language; MonoBehaviour-friendly lifecycle | `IAsyncEnumerable` / Rx |
| **Lua** | **game scripting** (Roblox, LÖVE, engines) & embedded (Neovim, Redis) | a **tiny table-based API**, no build step, coroutine-friendly | coroutines |
| **Elixir** | realtime & distributed backends, IoT (Nerves) | an **Ecto-style** macro query DSL, GenServer-owned handle | `Phoenix.PubSub` / LiveView |
| **Clojure** | data engineers, functional backends | **Datalog** — query as data, a natural fit for the graph model | `core.async` / atoms |
| **Julia** | scientific computing & ML | **DataFrame + broadcasting**, vector-native for embeddings | observers |
| **Haskell** | type-safety-first, research & finance | a **typed EDSL** (Esqueleto-style), compile-time-checked queries | `STM` |
| **Zig** | systems & game-engine developers | a **comptime** zero-cost typed API, explicit allocators | explicit callbacks |

A single embedded engine also means the TypeScript surface is the same on a
device (React Native) and on a server (Express, Next.js) — the query you write
in a route handler is the query you write in a screen.

The first six are in progress; the rest are community-driven targets, not
scheduled deliverables — each is a self-contained binding an interested
contributor can own end to end, because the umbrella contract keeps the engine
out of the picture.

## What the engine needs to support it

Two primitives underpin the ergonomic layer:

1. **Plan-equivalent builders.** Every fluent query lowers to the same SGQL
   plan, so there is one execution path and one place to optimize.
2. **Change notification.** Reactive queries need to know when to re-emit. Each
   committed transaction publishes which collections, keys, and edge types
   changed, so a watcher can refresh precisely instead of polling.

## Depth before breadth in each binding

Once a binding ships, strengthening and stabilizing its existing surface takes
priority over adding new ways to do the same thing. Each language keeps a small,
coherent set of entry points shaped for its primary audience, and effort goes
into making those more reliable, more predictable, and more pleasant — not into
multiplying alternatives. A stable, well-worn core is easier to learn, document,
and depend on than a wide one.

Near-term core polish includes:

- **Python** — return query results as parsed, mapping-like rows rather than
  JSON strings, so the common path needs no manual decode step. SQL and the
  DataFrame accessor stay the two complementary modes; no additional query
  interface is added.
- **General** — consistent result shapes, error types, and parameter handling
  across bindings, and steady, additive-only evolution of each surface so code
  written today keeps working.

## Other directions on the horizon

- **Relational integrity** — optional constraints declared in the schema and
  enforced by the engine, so a typed relation like *a tourist must originate
  from an existing country* holds everywhere:
  - **NOT NULL** on scalar columns (a value must be present).
  - **Required edges** — a mandatory relationship: an entity of a collection
    must have a given edge (the graph analogue of a NOT NULL foreign key).
  - **Referential integrity** — `REFERENCES EXISTING`: an edge can only be
    created when its destination node exists.
  - **`ON DELETE` policies** — `RESTRICT` (block), `CASCADE` (remove dependents),
    `SET NULL`/`DETACH` (drop the edge), or `NONE` (no action).

  Required and referential checks would be **deferred to COMMIT**, so a
  transaction can stage a node and its edges together (the same commit boundary
  the change feed already uses). Each typed language surface would expose this
  as a non-null, existence-checked reference (`ref(...)` / `Ref<T>`).

- **Format & language stability** — the compatibility contract is defined in
  [invariants.md](invariants.md#pillar-4--format--language-stability) (three
  rings: SGQL language, source-of-truth files, derived accelerators). Two pieces
  remain to build:
  - **Whole-database format version** — a single store-level version stamped in
    the manifest and checked on open, so a Ring-2 physical change is a detectable,
    migratable step. Per-file `[magic][version]` headers already exist; this
    unifies them into one checked contract with migration readers.
  - **SGQL logical dump/restore** — `dump` emits a database as portable SGQL
    text (`CREATE TABLE` + `INSERT` + edge `link`); `load` replays it. Version-
    independent by construction (the `sqlite .dump` / `pg_dump` escape hatch), so
    a database can always move between any two sekejap versions regardless of the
    binary layout. This is the strongest safety net and does not exist yet.

- **Disk-first spatial at scale** — serving spatial queries (not just counts)
  from the on-disk spatial index with bounded memory, for large map/geo datasets
  that exceed RAM.

- **Security** — for shared/served deployments: authentication and per-role
  access control on the serve/Postgres surfaces, optional encryption at rest,
  and hardened input validation. The embedded, in-process path stays
  dependency-light and unchanged.

- **Diagnostics & repair tooling** — the command-line tool as an operational
  companion: inspect schema and counts, monitor size/memory/WAL, verify
  integrity, and rebuild indexes or compact to fix.

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
