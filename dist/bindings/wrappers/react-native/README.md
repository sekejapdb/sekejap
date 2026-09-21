# sekejap for React Native (JSI, New Architecture)

The same typed, reactive API as everywhere else — but **synchronous on device**
via JSI, so `db.dishes.where(...).find()` returns rows directly (no Promise),
exactly like the Node backend.

```ts
import { openSekejap, entity, key, index, text, int, useQuery } from '@sekejap/react-native';

const Dish = entity('dishes', { id: key(text()), category: index(text(), 'hash'), price: int() });
const db = openSekejap('app.db', { schema: { dishes: Dish } });

db.dishes.put({ id: 'd1', category: 'main', price: 45000 });
const mains = db.dishes.where(d => d.category.eq('main')).find();   // Dish[], sync

function List() {
  const dishes = useQuery(db.dishes.where(d => d.category.eq('main')));
  return <FlatList data={dishes ?? []} ... />;
}
```

## Architecture

- The **typed layer is shared** with Node (`sekejap/orm`) — it's platform-agnostic
  and takes a pluggable backend. This package supplies a **JSI backend**:
  - `src/native.ts` — `SekejapJsi` implements the `RawDb` contract by calling the
    sync `global.SekejapJSI` HostObject.
  - `cpp/sekejap-jsi.cpp` — the C++ `HostObject` that wraps the **C ABI**
    (`../c/include/sekejap.h`): `open → DbHostObject{ execute, queryParams, put,
    get, putMany, compact, … }`.
  - `src/NativeSekejapJsi.ts` — a tiny TurboModule whose `install()` runs
    `sekejap::install(runtime)` once at startup.

## Status — scaffold, pre-validation

Done and verifiable now:
- Shared typed layer is backend-agnostic (Node stays green).
- The **C ABI cross-compiles** for Android ABIs (cargo-ndk) and the iOS Rust
  targets are installed.
- The JSI `HostObject` (C++) and the TS glue are written against the C ABI.

Remaining native wiring (needs a New-Architecture RN app to build + validate):
1. **iOS**: a `.podspec` that compiles `cpp/` + links the C ABI static lib
   (build for `aarch64-apple-ios` + `*-sim`, lipo into an xcframework), and a
   TurboModule whose `install()` calls `sekejap::install(runtime)`.
2. **Android**: `android/build.gradle` + `CMakeLists.txt` compiling `cpp/` with
   `../c/include` on the header path and the C ABI `.so`/`.a` per ABI in
   `jniLibs`, and a TurboModule `install()` → `sekejap::install(runtime)`.
   (Reuse `wrappers/kotlin/orm/build-native.sh` to produce the ABIs.)
3. **Codegen**: `codegenConfig` is set; RN codegen generates the `Spec` bindings.
4. **Validate**: a New-Arch RN example app, run on the Android device (and iOS
   sim) exercising put/get/where/find/count/update.

Not yet on RN: reactive `.watch()` — it needs a change-feed export added to the
C ABI (`sekejap_subscribe`); until then, `find()`/`useQuery` (poll-free reads)
work, and `.watch()` is added once the C ABI exposes the feed.
