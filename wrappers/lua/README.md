# sekejap for Lua

A small, table-based Lua binding for the [sekejap](https://sekejap.life) embedded
database — a graph-first, multi-model engine (SQL + graph + spatial + vector +
full-text) — as a Lua 5.4 **C module** over the stable C ABI. No build step for
callers beyond compiling the module once; it links `libsekejap`.

Made for **game scripting** (engines that embed Lua) and embedded hosts
(Neovim, Redis, OpenResty).

## Build

```bash
cargo build --release -p sekejap-capi   # build libsekejap once
cd wrappers/lua
make                                     # → sekejap.so (links libsekejap)
make test                                # run test.lua
```

`make` uses `pkg-config` for the Lua headers and builds a loadable module (the
`lua_*` symbols resolve from the host interpreter at load). The module embeds an
rpath to `target/release`, so `require("sekejap")` finds `libsekejap` with no
`LD_LIBRARY_PATH`. For distribution, install `sekejap.so` on your `package.cpath`
(a LuaRocks rockspec can automate this).

## Use

```lua
local sekejap = require("sekejap")
print(sekejap.version())                 -- "0.16.2"

local db = sekejap.open("./data")        -- directory on disk

db:execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)")
db:execute_params("INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)",
                  '["ubud","Ubud","central"]')

local rows = db:query("SELECT name FROM places WHERE area = 'central'")  -- JSON string
print(rows)                              -- [{"name":"Ubud"}]

-- graph traversal
db:link("tourists/chloe", "places/ubud", "visited")
db:query("SELECT p.name FROM MATCH (t:tourists)-[:visited]->(p:places) WHERE t._key = 'chloe'")

db:close()                               -- also closes automatically on GC
```

Results come back as **JSON strings** — decode with any Lua JSON library
(e.g. `dkjson`, `cjson`).

## API

Module: `sekejap.open(path)` → db, `sekejap.version()`.

Database methods: `execute`, `execute_params`, `query`, `query_params`,
`prepare` / `query_prepared`, `put`, `put_many`, `get` (returns `nil` on a
missing node), `remove`, `link`, `contains`, `node_count`, `edge_count`,
`compact`, `close`.

Hard failures (malformed SQL, closed handle) raise Lua errors — catch with
`pcall`. A clean `get` miss returns `nil`, not an error.
