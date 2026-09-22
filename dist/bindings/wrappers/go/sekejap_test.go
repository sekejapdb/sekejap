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

	if _, err := db.Execute("CREATE TABLE t (key TEXT PRIMARY KEY, v INTEGER)"); err != nil {
		t.Fatal(err)
	}
	// QL_CONTRACT §6: every Tier-1 predicate on an indexed field is answered
	// index-side, so WHERE v = ... needs a named index over v.
	if _, err := db.Execute("CREATE INDEX t_v ON t USING btree (v)"); err != nil {
		t.Fatal(err)
	}
	if n, err := db.Execute("INSERT INTO t (key, v) VALUES ('a', 42)"); err != nil || n != 1 {
		t.Fatalf("insert: err=%v n=%d", err, n)
	}

	// Direct document put (no SQL), collection + key rather than one slug.
	if err := db.Put("t", "b", map[string]any{"v": 7}); err != nil {
		t.Fatal(err)
	}
	if n, err := db.CountRows("t"); err != nil || n != 2 {
		t.Fatalf("count_rows: err=%v n=%d", err, n)
	}
	if ok, err := db.Exists("t", "a"); err != nil || !ok {
		t.Fatalf("exists(t/a): ok=%v err=%v", ok, err)
	}
	if ok, err := db.Exists("t", "zzz"); err != nil || ok {
		t.Fatalf("exists(t/zzz): ok=%v err=%v", ok, err)
	}

	// Query -> decoded rows.
	rows, err := db.Query("SELECT v FROM t WHERE _key = 'a'")
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || rows[0]["v"] != float64(42) {
		t.Fatalf("query rows = %v", rows)
	}

	// Parameterized (injection-safe): a $n placeholder bound from Query's
	// variadic params.
	rows, err = db.Query("SELECT _key FROM t WHERE v = $1", 7)
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 {
		t.Fatalf("params rows = %v", rows)
	}

	// Get by collection + key.
	doc, ok, err := db.Get("t", "b")
	if err != nil || !ok || doc["v"] != float64(7) {
		t.Fatalf("get: err=%v ok=%v doc=%v", err, ok, doc)
	}

	// A scan walks the collection in stable id order.
	scan, err := db.Scan("t", 0)
	if err != nil {
		t.Fatal(err)
	}
	seen := 0
	for {
		page, ok, err := scan.Next()
		if err != nil {
			t.Fatal(err)
		}
		if !ok {
			break
		}
		seen += len(page)
	}
	scan.Close()
	if seen != 2 {
		t.Fatalf("scan saw %d rows, want 2", seen)
	}

	// Prepare + rebind: parsed once, executed with different parameters.
	stmt, err := db.Prepare("SELECT _key FROM t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := stmt.Query(42); err != nil {
		t.Fatal(err)
	}
	if _, err := stmt.Query(7); err != nil {
		t.Fatal(err)
	}
	if rb, err := stmt.Rebindable(); err != nil || rb != RebindYes {
		t.Fatalf("rebindable = %v, err = %v", rb, err)
	}
	stmt.Close()

	// Link + neighbours.
	if err := db.Link("t", "a", "near", "t", "b"); err != nil {
		t.Fatal(err)
	}
	if n, err := db.ScanCountEdges(); err != nil || n != 1 {
		t.Fatalf("scan_count_edges: err=%v n=%d", err, n)
	}
	neighbours, err := db.Neighbours("t", "a", "", DirectionOutgoing, 10)
	if err != nil {
		t.Fatal(err)
	}
	if len(neighbours) != 1 || neighbours[0].Collection != "t" || neighbours[0].Key != "b" {
		t.Fatalf("neighbours = %v", neighbours)
	}

	// Transaction: many writes under one commit barrier.
	tx, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx.Put("t", "c", map[string]any{"v": 100}); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
	if ok, err := db.Exists("t", "c"); err != nil || !ok {
		t.Fatalf("post-commit exists(t/c): ok=%v err=%v", ok, err)
	}

	// A rolled-back transaction leaves nothing behind.
	tx2, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx2.Put("t", "d", map[string]any{"v": 200}); err != nil {
		t.Fatal(err)
	}
	if err := tx2.Rollback(); err != nil {
		t.Fatal(err)
	}
	if ok, err := db.Exists("t", "d"); err != nil || ok {
		t.Fatalf("post-rollback exists(t/d): ok=%v err=%v", ok, err)
	}

	// Error path: a bad statement returns an error that surfaces last_error.
	if _, err := db.Execute("SELECT bad syntax FROM"); err == nil {
		t.Fatal("expected an error from a bad statement")
	} else if se, ok := err.(*Error); !ok || se.Code != StatusInvalid {
		t.Fatalf("expected a StatusInvalid *Error, got %#v", err)
	}

	if _, err := db.Checkpoint(); err != nil {
		t.Fatal(err)
	}

	t.Logf("OK — sekejap %s (format %d)", Version(), FormatVersion())
}

func TestPrepared(t *testing.T) {
	dir, _ := os.MkdirTemp("", "sekejap_go_prep")
	defer os.RemoveAll(dir)
	db, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	db.Execute("CREATE TABLE t (key TEXT PRIMARY KEY, v INTEGER)")
	db.Execute("CREATE INDEX t_v ON t USING btree (v)")
	for i := 0; i < 5; i++ {
		db.Execute(fmt.Sprintf("INSERT INTO t (key, v) VALUES ('k%d', %d)", i, i))
	}

	stmt, err := db.Prepare("SELECT _key FROM t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	defer stmt.Close()

	for i := 0; i < 5; i++ {
		rows, err := stmt.Query(i)
		if err != nil {
			t.Fatal(err)
		}
		if len(rows) != 1 || rows[0]["_key"] != fmt.Sprintf("k%d", i) {
			t.Fatalf("param %d: %v", i, rows)
		}
	}
}

// Constructs sekejap has no atomic for are refused by name, never emulated
// -- OpenMemory is one of them.
func TestRefusedByName(t *testing.T) {
	if _, err := OpenMemory(); err == nil {
		t.Fatal("expected OpenMemory to be refused")
	} else if se, ok := err.(*Error); !ok || se.Code != StatusRefused {
		t.Fatalf("expected a StatusRefused *Error, got %#v", err)
	}
}
