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

| language | shaped for | interface form | reactive |
|---|---|---|---|
| **Dart / Flutter** | Flutter app & game (Flame) developers | annotated `@Entity` classes, build_runner codegen, widget-ready results | `Stream` |
| **Kotlin** | Android app developers | annotated data classes, KSP codegen, coroutine-first | `Flow` (Compose) |
| **Swift** | iOS / macOS app developers | `@Model`-style macro, SwiftUI `@Query`-style property wrapper | `AsyncSequence` |
| **TypeScript** | full-stack web **and** React Native | typed client from a schema (Prisma/Drizzle feel), same code on device and server | async iterator / hooks |
| **Python** | FastAPI developers **and** data scientists | two doors: Pydantic models + async queries; DataFrame in/out | async iterator |
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
