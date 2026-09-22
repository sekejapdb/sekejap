// A five-stop tour of sekejap from Go: SQL, graph, spatial, vector, hybrid.
//
//	cargo build --release -p sekejap-capi   (once, from the repo root)
//	go run ./dist/bindings/wrappers/go/examples/tour
package main

import (
	"fmt"
	"os"

	sekejap "github.com/sekejapdb/sekejap/dist/bindings/wrappers/go"
)

func show(label string, rows []map[string]any) {
	fmt.Println(label)
	for _, r := range rows {
		fmt.Println("  ", r)
	}
}

func main() {
	dir, _ := os.MkdirTemp("", "sekejap-tour-")
	defer os.RemoveAll(dir)
	db, err := sekejap.Open(dir)
	if err != nil {
		panic(err)
	}
	defer db.Close()

	// ── 1. Core SQL ──────────────────────────────────────────────────────
	db.Execute("CREATE TABLE places (key TEXT PRIMARY KEY, name TEXT, area TEXT)")
	for _, p := range [][3]string{
		{"uluwatu", "Uluwatu Temple", "south"},
		{"kuta", "Kuta Beach", "south"},
		{"ubud", "Ubud Center", "central"},
	} {
		db.Execute("INSERT INTO places (key, name, area) VALUES ($1, $2, $3)", p[0], p[1], p[2])
	}
	rows, _ := db.Query("SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC")
	show("places per area:", rows)

	// ── 2. Graph ─────────────────────────────────────────────────────────
	// Edges are written through the C ABI's Link/LinkWith, not SQL DML --
	// `INSERT INTO GRAPH ... EDGE` is a Phase-3 item still being built
	// (docs/lang/QL_CONTRACT.md §2) -- and read back with SQL's GRAPH_TABLE,
	// which IS Tier 1 today.
	db.Execute("CREATE TABLE tourists (key TEXT PRIMARY KEY, name TEXT)")
	db.Execute("CREATE TABLE flights (key TEXT PRIMARY KEY, airline TEXT)")
	db.Execute("INSERT INTO tourists (key, name) VALUES ('chloe', 'Chloe')")
	db.Execute("INSERT INTO flights (key, airline) VALUES ('qf-mel', 'Qantas')")
	if err := db.Link("tourists", "chloe", "flew_on", "flights", "qf-mel"); err != nil {
		panic(err)
	}
	rows, _ = db.Query(
		"SELECT k FROM GRAPH_TABLE (base MATCH (t:tourists WHERE t._key = $1)-[:flew_on]->(f:flights) "+
			"COLUMNS (f.airline AS k))",
		"chloe")
	show("Chloe's flight:", rows)

	// ── 3. Spatial (radius in metres, PostGIS function names) ─────────────
	db.Execute("CREATE TABLE spots (key TEXT PRIMARY KEY, name TEXT, loc GEOMETRY(Point,4326))")
	db.Execute("CREATE INDEX spots_loc ON spots USING gist (loc)")
	for _, p := range []struct {
		key, name string
		lon, lat  float64
	}{
		{"uluwatu", "Uluwatu Temple", 115.087, -8.829},
		{"kuta", "Kuta Beach", 115.168, -8.720},
		{"ubud", "Ubud Center", 115.263, -8.507},
	} {
		db.Execute("INSERT INTO spots (key, name, loc) VALUES ($1, $2, $3)",
			p.key, p.name, fmt.Sprintf(`{"type":"Point","coordinates":[%f,%f]}`, p.lon, p.lat))
	}
	rows, _ = db.Query(
		"SELECT name FROM spots WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3, true)",
		115.087, -8.829, 20000.0)
	show("within 20 km of Uluwatu:", rows)

	// ── 4. Vector ────────────────────────────────────────────────────────
	db.Execute("CREATE TABLE items (key TEXT PRIMARY KEY, name TEXT, emb VECTOR(3))")
	db.Execute("CREATE INDEX items_emb ON items USING exact (emb)")
	for _, p := range []struct {
		key, name string
		emb       []float64
	}{
		{"a", "apple", []float64{1.0, 0.0, 0.0}},
		{"b", "banana", []float64{0.0, 1.0, 0.0}},
		{"c", "cherry", []float64{0.9, 0.1, 0.0}},
	} {
		db.Execute("INSERT INTO items (key, name, emb) VALUES ($1, $2, $3)", p.key, p.name, p.emb)
	}
	rows, _ = db.Query("SELECT name FROM items ORDER BY emb <=> $1::vector LIMIT 2", []float64{1.0, 0.0, 0.0})
	show("2 nearest to [1,0,0]:", rows)

	// ── 5. Hybrid: text + vector in one ORDER BY ─────────────────────────
	db.Execute("CREATE TABLE dishes (key TEXT PRIMARY KEY, name TEXT, description TEXT, embedding VECTOR(3))")
	db.Execute("CREATE INDEX dishes_descr ON dishes USING gin (to_tsvector('simple', description))")
	db.Execute("CREATE INDEX dishes_emb ON dishes USING exact (embedding)")
	for _, p := range []struct {
		key, name, description string
		embedding              []float64
	}{
		{"a", "Grilled Chicken", "healthy grilled chicken with herbs", []float64{1.0, 0.0, 0.0}},
		{"b", "Fried Rice", "classic fried rice street food", []float64{0.0, 1.0, 0.0}},
		{"c", "Grilled Fish", "grilled fish, light and healthy", []float64{0.8, 0.2, 0.0}},
	} {
		db.Execute("INSERT INTO dishes (key, name, description, embedding) VALUES ($1, $2, $3, $4)",
			p.key, p.name, p.description, p.embedding)
	}
	rows, _ = db.Query(
		"SELECT name FROM dishes WHERE to_tsvector('simple', description) @@ to_tsquery('simple', $1) "+
			"ORDER BY 0.5 * bm25(description, $1) + 0.5 * (1 - (embedding <=> $2::vector)) DESC",
		"grilled & healthy", []float64{1.0, 0.0, 0.0})
	show("ranked dishes:", rows)
}
