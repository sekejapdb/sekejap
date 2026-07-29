# sekejap for Node.js

**Status: working** (tested). Native Node.js binding via [napi-rs](https://napi.rs/) —
`#[napi]` over `sekejap::CoreDB`, compiled to a native N-API addon.

## Run it

```bash
cd wrappers/node
cargo build --release                        # builds the addon
cp target/release/libsekejap_node.dylib sekejap.node   # (.so on Linux, .dll on Windows)
node test.cjs                                # → OK — sekejap-node 0.13.0 …
```

For real packaging use `npx @napi-rs/cli build --release`, which names the addon
per-platform and generates `index.js` + `index.d.ts` (TypeScript types). JS API:
`Db.open(path)`, `execute`, `query`/`queryParams` (→ JSON string, `JSON.parse` it),
`put`, `link`, `nodeCount`/`edgeCount`, `compact`, `version()`.

## Approach

napi-rs is Node's equivalent of PyO3: you annotate Rust with `#[napi]` and it
builds a native Node addon (`.node`) that speaks Node's stable **N-API** directly —
idiomatic JS types, no manual C ABI. (An alternative is `koffi`/`ffi-napi` calling
the C ABI in `../c`, but napi-rs gives better perf and TypeScript types.)

```
wrappers/node/
├── Cargo.toml        # crate-type = ["cdylib"], deps: napi, napi-derive; depends on the sekejap crate
├── package.json      # name "sekejap", @napi-rs/cli build/publish scripts
├── src/lib.rs        # #[napi] wrappers over sekejap::CoreDB / Engine
├── index.d.ts        # generated TypeScript types
└── examples/         # .js / .ts usage
```

Minimal `Cargo.toml` to add (then add `wrappers/node` to the workspace members):

```toml
[package]
name = "sekejap-node"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib"]

[dependencies]
sekejap = { path = "../.." }
napi = { version = "2", features = ["napi8"] }
napi-derive = "2"

[build-dependencies]
napi-build = "2"
```

## Distribution

- **Registry:** [npm](https://npmjs.com) → `npm install sekejap`
- **Publish:** `npm publish`. Build prebuilt binaries per platform in CI with
  `@napi-rs/cli` (`napi build --release`) and ship them as platform packages under
  `optionalDependencies`, so users don't need a Rust toolchain.
- Reserve the name `sekejap` on npm now.
