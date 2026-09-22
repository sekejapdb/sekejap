-- Tour + assertions for the sekejap Lua binding, over the 0.17 C ABI.
--   make test
local sekejap = require("sekejap")
print("sekejap " .. sekejap.version() .. " (format " .. sekejap.format_version() .. ")")

local dir = os.getenv("SEKEJAP_TEST_DIR") or ("<scratch>" .. os.time())
local db = sekejap.open(dir)
print("opened " .. dir)

-- Catalog: create a collection
local created = db:create_collection("places", '[{"name":"name","kind":"text"},{"name":"area","kind":"text"}]')
assert(created == true, "expected places to be created")
local again = db:create_collection("places", '[{"name":"name","kind":"text"},{"name":"area","kind":"text"}]')
assert(again == false, "a second create_collection must report 'already there'")
db:execute("CREATE INDEX places_area ON places USING btree (area)") -- a Tier-1 predicate on `area` needs a named index (QL_CONTRACT §6)

-- Put + get
assert(db:put("places", "ubud", '{"_key":"ubud","name":"Ubud","area":"central"}'))
assert(db:put("places", "kuta", '{"_key":"kuta","name":"Kuta","area":"south"}'))
local got = db:get("places", "ubud")
assert(got and got:find("Ubud"), "expected Ubud payload, got " .. tostring(got))
assert(db:get("places", "nowhere") == nil, "clean miss must be nil")
assert(db:exists("places", "ubud") == true)
assert(db:exists("places", "nowhere") == false)
print("last_error() after a clean call: " .. tostring(db:last_error())) -- a success clears the slot (C_ABI.md §1)
assert(db:last_error() == nil)
assert(db:last_error_code() == sekejap.STATUS_OK)

-- Query with a $n parameter
local south = db:query("SELECT name FROM places WHERE area = $1", '["south"]')
assert(south:find("Kuta"), "expected Kuta, got " .. tostring(south))
print("area=south: " .. south)

-- Scan, one row per page, walking to the end
local scan = db:scan_open("places", 1)
local scanned = 0
while true do
  local page = scan:next()
  if page == nil then break end
  scanned = scanned + 1
end
assert(scanned >= 2, "expected at least 2 scan pages, got " .. scanned)
scan:close()
print("scan pages: " .. scanned)

-- Prepare + rebind: one statement, two different parameter binds
local stmt = db:prepare("SELECT _key FROM places WHERE area = $1")
local central = stmt:query('["central"]')
assert(central:find("ubud"), "expected ubud, got " .. tostring(central))
assert(stmt:rebindable() == true, "a SELECT should compile once and rebind")
local south2 = stmt:query('["south"]') -- the rebind
assert(south2:find("kuta"), "expected kuta on rebind, got " .. tostring(south2))
print("prepared+rebind: " .. central .. " / " .. south2)

-- Link + neighbours
db:create_collection("tourists", '[{"name":"name","kind":"text"}]')
db:put("tourists", "chloe", '{"_key":"chloe","name":"Chloe"}')
assert(db:link("tourists", "chloe", "visited", "places", "ubud"))
local visited = db:neighbours("tourists", "chloe", "visited", "outgoing", 10)
assert(visited:find("ubud"), "expected ubud among neighbours, got " .. tostring(visited))
print("chloe visited: " .. visited)

-- Graph traversal in SQL/PGQ, over the same edge (0.17: GRAPH_TABLE, not e1's
-- `FROM MATCH`). `base` is the reserved name of the base graph context that
-- db:link() (no context argument) writes into -- GRAPH_CONTRACT.md §2.1.
local traversal = db:query([[
  SELECT place FROM GRAPH_TABLE (base
    MATCH (t IS tourists WHERE t._key = 'chloe')-[e IS visited]->(p IS places)
    COLUMNS (p.name AS place))
]])
assert(traversal:find("Ubud"), "expected graph traversal to find Ubud, got " .. tostring(traversal))

-- Transactions: commit makes a write stick, rollback discards it
local tx1 = db:tx_begin()
assert(tx1:put("places", "sanur", '{"_key":"sanur","name":"Sanur","area":"south"}'))
assert(tx1:commit())
assert(db:exists("places", "sanur") == true, "a committed tx write must be visible")

local tx2 = db:tx_begin()
assert(tx2:put("places", "canggu", '{"_key":"canggu","name":"Canggu","area":"south"}'))
assert(tx2:rollback())
assert(db:exists("places", "canggu") == false, "a rolled-back tx write must not be visible")

-- count_rows
local n = db:count_rows("places")
assert(n >= 3, "expected at least 3 rows (ubud, kuta, sanur), got " .. tostring(n))
print("places rows: " .. n)

-- Catalog introspection
local cols = db:collections()
assert(cols:find("places") and cols:find("tourists"), "expected both collections, got " .. tostring(cols))
local desc = db:describe("places")
assert(desc:find("places"), "expected a description for places, got " .. tostring(desc))
assert(db:describe("no_such_collection") == nil, "describe of an unknown collection must be nil")

local storage = db:storage()
assert(storage:find("data_bytes"), "expected data_bytes in storage(), got " .. tostring(storage))

-- Error path 1: malformed SQL raises (pcall-catchable), carrying last_error()
local ok, err = pcall(function() db:query("THIS IS NOT VALID SQL") end)
assert(not ok, "expected an error")
assert(tostring(err):find("sekejap"), "expected the error to be tagged 'sekejap', got " .. tostring(err))
print("caught expected error: " .. tostring(err))

-- Error path 2: a construct sekejap has no atomic for is REFUSED by name, not emulated
local ok2, err2 = pcall(function() db:compact() end)
assert(not ok2, "compact must be refused")
print("compact refused as expected: " .. tostring(err2))

db:close()
print("ALL LUA CHECKS PASSED")
