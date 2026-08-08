-- Tour + assertions for the sekejap Lua binding.
--   cargo build --release -p sekejap-capi && make test
local sekejap = require("sekejap")
print("sekejap " .. sekejap.version())

local dir = "/tmp/sekejap-lua-test-" .. os.time()
local db = sekejap.open(dir)

-- Relational
db:execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)")
db:execute("INSERT INTO places (_key, name, area) VALUES ('ubud', 'Ubud', 'central')")
db:execute_params("INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)", '["kuta","Kuta","south"]')

local rows = db:query("SELECT name FROM places WHERE area = 'central'")
assert(rows:find("Ubud"), "expected Ubud")
print("central: " .. rows)

-- Prepared
local st = db:prepare("SELECT _key FROM places WHERE area = $1")
local south = db:query_prepared(st, '["south"]')
assert(south:find("kuta"), "expected kuta")

-- Records + graph
db:put("places/sanur", '{"_collection":"places","_key":"sanur","name":"Sanur","area":"south"}')
assert(db:contains("places/sanur"))
local got = db:get("places/sanur")
assert(got and got:find("Sanur"), "expected Sanur payload")
assert(db:get("places/nowhere") == nil, "clean miss must be nil")

db:execute("CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)")
db:put("tourists/chloe", '{"_collection":"tourists","_key":"chloe","name":"Chloe"}')
db:link("tourists/chloe", "places/ubud", "visited")
local visited = db:query(
  "SELECT p.name AS place FROM MATCH (t:tourists)-[:visited]->(p:places) WHERE t._key = 'chloe'")
assert(visited:find("Ubud"), "expected graph traversal to find Ubud")
print("chloe visited: " .. visited)

print(("nodes=%d edges=%d"):format(db:node_count(), db:edge_count()))
assert(db:node_count() >= 4 and db:edge_count() >= 1)

-- Error path: malformed SQL raises (pcall-catchable)
local ok, err = pcall(function() db:query("THIS IS NOT VALID SQL") end)
assert(not ok, "expected an error")
print("caught expected error: " .. tostring(err))

db:close()
print("ALL LUA CHECKS PASSED")
