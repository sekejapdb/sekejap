# sekejap for TypeScript — typed, reactive, isomorphic

The **same `sekejap` package** on the server (Node/Express/Next) and on device
(React Native), with a typed API and reactive queries. **Schema-as-code** — no
build step, no codegen; your row types are inferred from the schema.

## 1. Define a schema (types are inferred)

```ts
import { entity, key, index, text, int, bool, geo, vector, bm25 } from 'sekejap/orm';

export const Dish = entity('dishes', {
  id:       key(text()),
  category: index(text(), 'hash'),
  price:    index(int()),        // btree
  openNow:  bool(),
  location: geo(),
  taste:    vector(384),
  about:    bm25(text()),
});
// type Dish = { id: string; category: string; price: number; openNow: boolean; … }  ← inferred
```

## 2. Open + typed CRUD

```ts
import { Sekejap } from 'sekejap/orm';

const db = Sekejap.open('app.db', { schema: { dishes: Dish } });

db.dishes.put({ id: 'd1', category: 'main', price: 45000, openNow: true, /* … */ });
const one = db.dishes.get('d1');                         // Dish | null

const cheapMains = db.dishes
  .where(d => d.category.eq('main').and(d.price.lt(90000)))
  .sortBy(d => d.price)
  .find();                                               // Dish[]

db.dishes.where(d => d.price.between(10000, 50000)).count();
db.dishes.where(d => d.id.eq('d1')).update({ price: 40000 });
db.dishes.where(d => d.id.eq('d1')).deleteAll();
```

`d.price.lt('cheap')` is a **compile error** — `price` is a `number`.

## 3. Multi-model in one query

```ts
db.dishes
  .where(d => d.openNow.eq(true))
  .near(d => d.location, here, { metres: 5000 })         // spatial
  .matchText(d => d.about, 'grilled')                    // text (BM25)
  .rankByVector(d => d.taste, myTaste)                   // vector
  .limit(10)
  .find();
```

## 4. Reactive

```ts
// Node / server — callback form: current list now, then after every relevant commit
const stop = db.dishes.where(d => d.category.eq('main')).subscribe(rows => broadcast(rows));
// later: stop();

// or async-iterable form:
for await (const dishes of db.dishes.where(d => d.category.eq('main')).watch()) { … }
```

```tsx
// React Native — the useQuery hook re-renders on change
import { useQuery } from 'sekejap/orm/react';

function DishList() {
  const dishes = useQuery(db.dishes.where(d => d.category.eq('main')));
  return <FlatList data={dishes ?? []} renderItem={({ item }) => <Text>{item.id}</Text>} />;
}
```

The query you write in an Express route is the query you write in a screen.

## Run the tests

```bash
cargo build --release                                  # build the native addon
cp target/release/libsekejap_node.dylib sekejap.darwin-arm64.node
npx tsx orm/orm.test.ts                                 # typed CRUD/query/subscribe, real engine
```

## Status

Typed CRUD, multi-model queries, `update`/`deleteAll`, and reactive `subscribe`
are implemented and tested on Node against the real binding. The `useQuery`
React hook and the React Native on-device native module are in progress; the
`.watch()` change feed and the typed builder are shared across both.
