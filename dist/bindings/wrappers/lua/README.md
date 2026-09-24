# sekejap for Lua

A Lua 5.4 **C module** over the sekejap 0.17 C ABI
([`docs/dist/C_ABI.md`](../../../../docs/dist/C_ABI.md),
[`dist/ffi/include/sekejap.h`](../../../ffi/include/sekejap.h)) --
[sekejap](https://sekejap.life) is a graph-first, multi-model embedded
database (SQL + graph traversal + spatial + vector + full-text), addressed
by **collection and key**. No build step for callers beyond compiling the
module once; it links a prebuilt `libsekejap`, and this directory builds no
Rust.

Made for **game scripting** (engines that embed Lua) and embedded hosts
(Neovim, Redis, OpenResty).

This is a full rewrite for 0.17: e1's wrapper here bound the 0.16 API (one
`"collection/key"` slug, `execute`/`execute_params` pairs instead of an
optional parameter, `contains`/`remove`/`node_count`/`edge_count`, no scan,
no prepared statements, no transactions, no catalog). None of that glue was
Rust (e1's Lua binding was already a plain C module over the header, unlike
several other language wrappers), so nothing was removed beyond the old
`sekejap.c`, `test.lua` and this README -- rewritten below to match 0.17's
own shape (collection + key, JSON documents, `$n` parameters, rows as
arrays of objects keyed by column, scans, prepared statements,
transactions, links, the catalog).

## Build

```bash
make            # -> sekejap.so, linking libsekejap
make test       # runs test.lua against it
```

`make` uses `pkg-config` for the Lua headers (`lua5.4`, falling back to
`lua`) and links a PREBUILT `libsekejap` -- **no `cargo` command runs in
this directory**. It reads `libsekejap.{dylib,so,a}` and
`include/sekejap.h` from `$SEKEJAP_PREFIX` (default
`/usr/local`);
point it elsewhere with e.g. `make SEKEJAP_PREFIX=/path/to/libsekejap`, or
build that prefix yourself with `cargo build --release -p sekejap-capi`
from the repository root and pass its `target/release` output directory.
The module embeds an absolute rpath to `$SEKEJAP_PREFIX`, so
`require("sekejap")` finds `libsekejap` at run time with no
`LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`.

`sekejap.pc` in the prebuilt prefix predates the current flat layout (it
sets `libdir=${prefix}/lib`, a directory that does not exist -- the
`.dylib`/`.a` sit directly in the prefix); this Makefile does not use
`pkg-config` for sekejap itself for that reason. Worth fixing at the
source (`dist/ffi/sekejap.pc.in` / the packaging step that stages
`libsekejap/`), which is outside this directory.

## Use

```lua
local sekejap = require("sekejap")
print(sekejap.version())                    -- "0.17.0"

local db = sekejap.open("./data")            -- a directory on disk

db:create_collection("places",
  '[{"name":"name","kind":"text"},{"name":"area","kind":"text"}]')
db:execute("CREATE INDEX places_area ON places USING btree (area)")
db:put("places", "ubud", '{"_key":"ubud","name":"Ubud","area":"central"}')

local rows = db:query("SELECT name FROM places WHERE area = $1", '["central"]') -- JSON string
print(rows)                                  -- [{"name":"Ubud"}]

-- a prepared statement, rebound with a different parameter
local stmt = db:prepare("SELECT name FROM places WHERE area = $1")
print(stmt:query('["central"]'))

-- transactions: many writes, one commit
local tx = db:tx_begin()
tx:put("places", "kuta", '{"_key":"kuta","name":"Kuta","area":"south"}')
tx:commit()                                  -- or tx:rollback()

-- graph: link two rows, then walk the edge in SQL/PGQ
db:link("tourists", "chloe", "visited", "places", "ubud")
db:query([[
  SELECT place FROM GRAPH_TABLE (base
    MATCH (t IS tourists WHERE t._key = 'chloe')-[e IS visited]->(p IS places)
    COLUMNS (p.name AS place))
]])
-- `base` is the reserved name of the base graph context that db:link()
-- (no context argument) writes into (docs/core/GRAPH_CONTRACT.md §2.1);
-- `CREATE PROPERTY GRAPH` (naming a graph) is Tier 2 and refused in this
-- build (see "Gaps" below).

db:close()                                   -- also closes automatically on GC
```

Documents, parameters and rows cross the boundary as **JSON text**, exactly
as the C ABI carries them -- decode with any Lua JSON library (`dkjson`,
`cjson`, ...) if you need Lua tables rather than strings.

## API

One Lua method per C function in `docs/dist/C_ABI.md` §4, on the matching
handle:

Module (`local sekejap = require("sekejap")`): `open(path)`,
`open_with_config(path, config_json?)`, `open_service(path)`,
`open_memory()` (always refused: sekejap is disk-first), `version()`,
`format_version()`, and the nine `sekejap.STATUS_*` constants of
`SekejapStatus`.

`Db` methods (§4.1-§4.9): `close`, `last_error`, `last_error_code`, `put`,
`put_many`, `get` (`nil` on a clean miss), `exists`, `delete`,
`scan_open(collection, page_rows?)` -> `Scan`, `execute(sql, params_json?)`,
`query`, `explain`, `prepare(sql)` -> `Stmt`, `query_open(sql, params_json?,
page_rows?)` -> `Scan`, `link`, `link_with`, `unlink`,
`neighbours(collection, key, edge_type?, direction?, limit?)` (direction:
`"outgoing"` default, `"incoming"`, `"both"`), `create_collection(name,
fields_json?)`, `drop_collection`, `collections`, `describe` (`nil` for an
unknown collection), `count_rows`, `scan_count_rows`, `scan_count_edges`,
`tx_begin()` -> `Tx`, `checkpoint`, `publish`, `storage`, `trim_memory`
(always refused), `compact` (always refused), `show` (always refused --
use `collections`/`describe`), `statement_timeout_ms`, `cancel`,
`clear_interrupt`, `subscribe`, `next_change`, `unsubscribe`.

`Stmt` methods: `query(params_json?)`, `execute(params_json?)`,
`rebindable()` (`true`/`false`/`nil` for "not bound yet"), `free()`.

`Scan` methods (from `scan_open` or `query_open` -- the C ABI names both
the same operation): `next()` (`nil` at the end of the walk), `close()`.

`Tx` methods: `put`, `delete`, `link`, `execute`, `commit()`, `rollback()`
(either one frees the handle whether it succeeds or not, per the C ABI).

A `Stmt`/`Scan`/`Tx` keeps its `Db` referenced (in its first uservalue) so
the `Db` is not garbage-collected while it is alive; the C ABI still
requires every `Stmt`/`Scan`/`Tx` to be freed **before** `db:close()`.
Every handle closes/frees itself on garbage collection and on Lua 5.4
to-be-closed scope exit (`__gc`/`__close`), and every explicit
close/free/commit/rollback is idempotent.

Hard failures raise a Lua error (catch with `pcall`) carrying
`sekejap_last_error()`, prefixed `"sekejap <call>: "`; `db:last_error()` /
`db:last_error_code()` are also exposed directly for a caller that wants
to inspect the thread-local slot rather than catch. A construct sekejap has
no atomic for (`db:compact()`, `db:trim_memory()`, `db:show()`,
`sekejap.open_memory()`) is REFUSED BY NAME the same way, never emulated.

## Test

`test.lua` (`make test`) runs the checklist end to end against a real
temporary directory from `os.tmpname()` (override with
`SEKEJAP_TEST_DIR`): open, create a collection (plus a scalar index and a
duplicate-create check), put, get (hit and clean miss), a `$n`-parameter
query, a paged scan to its end, prepare + rebind with two different
parameter sets, link + neighbours, a `GRAPH_TABLE` traversal over the same
edge, a committed and a rolled-back transaction, `count_rows`, `collections`
/ `describe` / `storage`, and two error paths (`pcall`-caught malformed SQL,
and a by-name refusal) that both surface `sekejap_last_error()`.

## Gaps in the C ABI / build found while wrapping

None in the C ABI itself: all 59 functions in `dist/ffi/include/sekejap.h`
bind cleanly and the full checklist above runs against the real library.

- `CREATE PROPERTY GRAPH` (naming a graph context for `GRAPH_TABLE`) is
  Tier 2 and refused in this build ("Not in this slice"), so
  `GRAPH_TABLE (base MATCH ...)` -- the reserved name of the context
  `db:link()` writes into with no context argument -- is the only graph
  reference this wrapper's test can use today. Not a C ABI gap (the C ABI
  has no `CREATE PROPERTY GRAPH` call of its own to gap; `sekejap_link`
  always writes the base graph, and every traversal in the test runs
  through `db:query`), just a note for whichever wrapper or doc assumes a
  named graph works today.
- `sekejap.pc`, shipped in the prebuilt prefix this wrapper links against,
  sets `libdir=${prefix}/lib`, which does not exist in the flat layout the
  prefix actually has (`libsekejap.dylib`/`.a` sit in the prefix root). A
  packaging issue in `dist/ffi`, outside this directory; this wrapper's
  Makefile works around it by not using `pkg-config` for sekejap.

## Publish

No CI release job exists for Lua: `.github/workflows/release.yml` has no
`lua`/`luarocks` job (checked; the workflow's `publish-*` jobs cover Rust,
Python, Dart, native libs, Swift, Node and Kotlin only). Publish by hand
once the rock builds against a real `libsekejap`:

```bash
# from this directory, with a LuaRocks API key configured:
luarocks upload sekejap-0.17.0-1.rockspec --api-key=<your-api-key>

# or build + install locally, then push the same rockspec:
luarocks make sekejap-0.17.0-1.rockspec
luarocks upload sekejap-0.17.0-1.rockspec --api-key=<your-api-key>
```

The rockspec's `source.url` is the sekejap repository
(`git+https://github.com/sekejapdb/sekejap.git`) at the release tag.
