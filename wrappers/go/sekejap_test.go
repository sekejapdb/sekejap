package sekejap

import (
	"fmt"
	"os"
	"testing"
)

func TestRoundTrip(t *testing.T) {
	dir, err := os.MkdirTemp("", "sekejap_go")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(dir)

	db, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	if _, err := db.Execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)"); err != nil {
		t.Fatal(err)
	}
	if n, err := db.Execute("INSERT INTO t (_key, v) VALUES ('a', 42)"); err != nil || n != 1 {
		t.Fatalf("insert: err=%v n=%d", err, n)
	}

	// Direct node put (no SQL).
	if err := db.Put("t/b", `{"_collection":"t","_key":"b","v":7}`); err != nil {
		t.Fatal(err)
	}
	if got := db.NodeCount(); got != 2 {
		t.Fatalf("node count = %d, want 2", got)
	}
	if !db.Contains("t/a") || db.Contains("t/zzz") {
		t.Fatal("Contains wrong")
	}

	// Query → decoded rows.
	rows, err := db.Query("SELECT v FROM t WHERE _key = 'a'")
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || rows[0]["v"] != float64(42) {
		t.Fatalf("query rows = %v", rows)
	}

	// Parameterized (injection-safe).
	rows, err = db.QueryParams("SELECT _key FROM t WHERE v = $1", 7)
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 {
		t.Fatalf("params rows = %v", rows)
	}

	// Get by slug.
	if payload, ok, err := db.Get("t/b"); err != nil || !ok || payload == "" {
		t.Fatalf("get: err=%v ok=%v payload=%q", err, ok, payload)
	}

	// Edge.
	if err := db.Link("t/a", "t/b", "near"); err != nil {
		t.Fatal(err)
	}
	if got := db.EdgeCount(); got != 1 {
		t.Fatalf("edge count = %d, want 1", got)
	}

	// Error path: a bad statement returns an error.
	if _, err := db.Execute("SELECT bad syntax FROM"); err == nil {
		t.Fatal("expected an error from a bad statement")
	}

	if err := db.Compact(); err != nil {
		t.Fatal(err)
	}

	t.Logf("OK — sekejap %s", Version())
}

func TestPrepared(t *testing.T) {
	dir, _ := os.MkdirTemp("", "sekejap_go_prep")
	defer os.RemoveAll(dir)
	db, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	db.Execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
	for i := 0; i < 5; i++ {
		db.Execute(fmt.Sprintf("INSERT INTO t (_key, v) VALUES ('k%d', %d)", i, i))
	}

	stmt, err := db.Prepare("SELECT _key FROM t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	defer stmt.Close()

	for i := 0; i < 5; i++ {
		rows, err := db.QueryPrepared(stmt, i)
		if err != nil {
			t.Fatal(err)
		}
		if len(rows) != 1 || rows[0]["_key"] != fmt.Sprintf("k%d", i) {
			t.Fatalf("param %d: %v", i, rows)
		}
	}
}
