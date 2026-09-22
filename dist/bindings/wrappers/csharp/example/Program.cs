// A tour of the sekejap 0.17.0 C# binding: open, declare a collection, put,
// get, query with a $n parameter, scan, prepare + rebind, link + neighbours,
// a transaction (commit then rollback), count_rows, an error path that
// surfaces last_error, close.
//
//   # native library discoverable at run time (built elsewhere -- this
//   # wrapper runs no cargo command; see ../README.md "Build & run"):
//   DYLD_LIBRARY_PATH=/path/to/libsekejap dotnet run     # macOS
//   LD_LIBRARY_PATH=/path/to/libsekejap  dotnet run      # Linux
using System;
using System.Diagnostics;
using System.IO;
using Sekejap;

string dir = Path.Combine(Path.GetTempPath(), "sekejap-csharp-tour-" + Environment.TickCount);

using var db = SekejapDb.Open(dir);
Console.WriteLine($"sekejap {SekejapDb.Version()} (format {SekejapDb.FormatVersion()})");

// ── declare a collection ────────────────────────────────────────────────────
bool created = db.CreateCollection(
    "places",
    "[{\"name\":\"name\",\"kind\":\"text\"},{\"name\":\"area\",\"kind\":\"text\"}]");
Debug.Assert(created);
Debug.Assert(!db.CreateCollection("places", "[]"));   // already there -> false, not a failure

// ── put / get ───────────────────────────────────────────────────────────────
db.Put("places", "ubud", "{\"_key\":\"ubud\",\"name\":\"Ubud\",\"area\":\"central\"}");
db.Put("places", "kuta", "{\"_key\":\"kuta\",\"name\":\"Kuta\",\"area\":\"south\"}");
db.Put("places", "sanur", "{\"_key\":\"sanur\",\"name\":\"Sanur\",\"area\":\"south\"}");

string? ubud = db.Get("places", "ubud");
Debug.Assert(ubud is not null && ubud.Contains("Ubud"));
Debug.Assert(db.Get("places", "nowhere") is null);     // a clean miss -> null, not an exception
Debug.Assert(db.Exists("places", "kuta"));

// ── query with a parameter ──────────────────────────────────────────────────
string south = db.Query("SELECT name FROM places WHERE area = $1 ORDER BY name", "[\"south\"]");
Console.WriteLine($"south: {south}");
Debug.Assert(south.Contains("Kuta") && south.Contains("Sanur") && !south.Contains("Ubud"));

// ── scan ─────────────────────────────────────────────────────────────────────
int scannedRows = 0;
using (var scan = db.ScanOpen("places", pageRows: 2))
{
    string? page;
    while ((page = scan.Next()) is not null)
    {
        // count "_key" occurrences as a cheap row count without a JSON parser
        int at = 0;
        while ((at = page.IndexOf("\"_key\"", at, StringComparison.Ordinal)) >= 0) { scannedRows++; at += 6; }
    }
}
Debug.Assert(scannedRows == 3);

// ── prepare + rebind ─────────────────────────────────────────────────────────
using (var byArea = db.Prepare("SELECT _key FROM places WHERE area = $1"))
{
    string firstBind = byArea.Query("[\"south\"]");
    Debug.Assert(firstBind.Contains("kuta"));
    Debug.Assert(byArea.Rebindable() == SekejapRebind.Yes);   // compiled; a further bind reuses the plan
    string secondBind = byArea.Query("[\"central\"]");        // the rebind
    Debug.Assert(secondBind.Contains("ubud"));
}

// ── link + neighbours ────────────────────────────────────────────────────────
db.CreateCollection("tourists", "[{\"name\":\"name\",\"kind\":\"text\"}]");
db.Put("tourists", "chloe", "{\"_key\":\"chloe\",\"name\":\"Chloe\"}");
db.Link("tourists", "chloe", "visited", "places", "ubud");

string neighbours = db.Neighbours("tourists", "chloe", "visited", SekejapDirection.Outgoing, limit: 10);
Console.WriteLine($"chloe visited: {neighbours}");
Debug.Assert(neighbours.Contains("\"ubud\""));

// ── transaction: commit ──────────────────────────────────────────────────────
using (var tx = db.TxBegin())
{
    tx.Put("places", "denpasar", "{\"_key\":\"denpasar\",\"name\":\"Denpasar\",\"area\":\"south\"}");
    tx.Commit();
}
Debug.Assert(db.Exists("places", "denpasar"));

// ── transaction: rollback ────────────────────────────────────────────────────
using (var tx = db.TxBegin())
{
    tx.Put("places", "ghost", "{\"_key\":\"ghost\",\"name\":\"Ghost\",\"area\":\"nowhere\"}");
    tx.Rollback();
}
Debug.Assert(!db.Exists("places", "ghost"));

// ── count_rows ────────────────────────────────────────────────────────────────
long rows = db.CountRows("places");
Console.WriteLine($"places rows={rows}");
Debug.Assert(rows == 4);   // ubud, kuta, sanur, denpasar

// ── error path: surfaces last_error ─────────────────────────────────────────
try
{
    db.Query("THIS IS NOT VALID SQL", null);
    throw new Exception("expected a SekejapException");
}
catch (SekejapException e)
{
    Console.WriteLine($"caught expected error ({e.Status}): {e.Message}");
    Debug.Assert(e.Status == SekejapStatus.Invalid);
    Debug.Assert(e.Message.Length > 0);
}

// ── close ────────────────────────────────────────────────────────────────────
// `using var db` above closes it when this script ends; closing here too
// shows the call is null-safe / idempotent per docs/dist/C_ABI.md §4.1.
db.Dispose();

Console.WriteLine("ALL C# CHECKS PASSED");
