// A tour of the C# binding — SQL, params, prepared, records, graph.
//
//   cargo build --release -p sekejap-capi
//   cd wrappers/csharp/example
//   DYLD_LIBRARY_PATH=../../../target/release dotnet run     # macOS
//   LD_LIBRARY_PATH=../../../target/release  dotnet run      # Linux
using System;
using System.Diagnostics;
using System.IO;
using Sekejap;

string dir = Path.Combine(Path.GetTempPath(), "sekejap-csharp-tour-" + Environment.TickCount);

using var db = SekejapDb.Open(dir);
Console.WriteLine($"sekejap {SekejapDb.Version()}");

// Relational
db.Execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)");
db.Execute("INSERT INTO places (_key, name, area) VALUES ('ubud', 'Ubud', 'central')");
db.ExecuteParams("INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)", "[\"kuta\",\"Kuta\",\"south\"]");

string central = db.Query("SELECT name FROM places WHERE area = 'central'");
Console.WriteLine($"central: {central}");
Debug.Assert(central.Contains("Ubud"));

// Prepared
using var byArea = db.Prepare("SELECT _key FROM places WHERE area = $1");
string south = db.QueryPrepared(byArea, "[\"south\"]");
Debug.Assert(south.Contains("kuta"));

// Records + graph
db.Put("places/sanur", "{\"_collection\":\"places\",\"_key\":\"sanur\",\"name\":\"Sanur\",\"area\":\"south\"}");
Debug.Assert(db.Contains("places/sanur"));
Debug.Assert(db.Get("places/sanur")?.Contains("Sanur") == true);
Debug.Assert(db.Get("places/nowhere") is null);   // clean miss → null, not an exception

db.Execute("CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)");
db.Put("tourists/chloe", "{\"_collection\":\"tourists\",\"_key\":\"chloe\",\"name\":\"Chloe\"}");
db.Link("tourists/chloe", "places/ubud", "visited");
string visited = db.Query(
    "SELECT p.name AS place FROM MATCH (t:tourists)-[:visited]->(p:places) WHERE t._key = 'chloe'");
Console.WriteLine($"chloe visited: {visited}");
Debug.Assert(visited.Contains("Ubud"));

Console.WriteLine($"nodes={db.NodeCount()} edges={db.EdgeCount()}");

// Error path
try
{
    db.Query("THIS IS NOT VALID SQL");
    throw new Exception("expected a SekejapException");
}
catch (SekejapException e)
{
    Console.WriteLine($"caught expected error: {e.Message}");
}

Console.WriteLine("ALL C# CHECKS PASSED");
