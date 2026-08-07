# One flow, idiomatic in every language

sekejap's typed API is designed to feel like each language's own people wrote
it — Dart the way Dart is written, Kotlin the way Kotlin is written, and so on —
while following the **same conceptual flow** underneath. You learn the flow once;
in each language you get the surface that community already reaches for.

## The shared flow

Every wrapper walks the same path:

1. **Describe a model** — a typed entity with a key, indexes, and the fields
   (scalar, geo, vector, text).
2. **Get a typed collection** for it (`db.<collection>`).
3. **Build a query** by composing the models: filter, follow relationships,
   constrain by distance, rank by text and vector, order, page.
4. **Run it** — read once, or **watch** it and react to changes.
5. **Write** — put, update, delete.

The concepts and their order are identical across languages. Only the spelling
changes, on purpose.

## Idiomatic on purpose

The same query — "open mains under 90k, cheapest first" — in three languages,
each in that language's grain:

```dart
// Dart — build_runner codegen, cascades, Stream
db.dishes.where((d) => d.category.eq('main') & d.price.lt(90000))
         .sortBy((d) => d.price).limitTo(20).find();
```
```kotlin
// Kotlin — KSP, infix operators, Flow
db.dishes.where { it.category eq "main" and (it.price lt 90000) }
         .sortBy { it.price }.limitTo(20).find()
```
```ts
// TypeScript — schema-as-code type inference, no build step
db.dishes.where(d => d.category.eq('main').and(d.price.lt(90000)))
         .sortBy(d => d.price).limit(20).find();
```

`limitTo` in Dart/Kotlin, `limit` in TypeScript; `&` vs `and` vs `.and()`;
`Stream` vs `Flow` vs async iteration for `watch`. These differences are the
point — each reads naturally to that community. Languages still to come lean on
their own strengths the same way: types in Haskell, data/Datalog in Clojure,
comptime in Zig.

## The stability promise

- Whatever surface a language ships, it is meant to **last**. Names and meanings
  follow semantic versioning: removing or repurposing one would be a major
  version, and the intent is to never need to.
- New capability arrives **additively** — new verbs or options — never by
  changing what an existing one does. Code written today keeps working.
- The typed layer is a **front-end** over the query surface; the engine's
  internals change between releases, the surface does not.

Same flow everywhere, written for each language's people, stable once shipped.
