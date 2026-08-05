// A five-stop tour of sekejap from Go: SQL, graph, spatial, vector, hybrid.
//
//	cargo build --release -p sekejap-capi   (once, from the repo root)
//	go run ./examples/tour                  (from wrappers/go)
package main

import (
	"fmt"
	"os"

	sekejap "github.com/sekejapdb/sekejap/wrappers/go"
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
	db.Execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)")
	for _, p := range [][3]string{
		{"uluwatu", "Uluwatu Temple", "south"},
		{"kuta", "Kuta Beach", "south"},
		{"ubud", "Ubud Center", "central"},
	} {
		db.Execute(fmt.Sprintf(
			"INSERT INTO places (_key, name, area) VALUES ('%s', '%s', '%s')", p[0], p[1], p[2]))
	}
	rows, _ := db.Query("SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC")
	show("places per area:", rows)

	// ── 2. Graph ─────────────────────────────────────────────────────────
	db.Execute("CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)")
	db.Execute("CREATE TABLE flights (_key TEXT PRIMARY KEY, airline TEXT)")
	db.Execute("INSERT INTO tourists (_key, name) VALUES ('chloe', 'Chloe')")
	db.Execute("INSERT INTO flights (_key, airline) VALUES ('qf-mel', 'Qantas')")
	db.Execute("INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')")
	rows, _ = db.Query("SELECT f.airline AS airline FROM MATCH (t:tourists)-[:flew_on]->(f:flights) WHERE t._key = 'chloe'")
	show("Chloe's flight:", rows)

	// ── 3. Spatial (radius in metres) ────────────────────────────────────
	db.Execute("CREATE TABLE spots (_key TEXT PRIMARY KEY, name TEXT, geometry GEO)")
	db.Execute("CREATE INDEX ON spots USING spatial (geometry)")
	for _, p := range []struct {
		key, name string
		lon, lat  float64
	}{
		{"uluwatu", "Uluwatu Temple", 115.087, -8.829},
		{"kuta", "Kuta Beach", 115.168, -8.720},
		{"ubud", "Ubud Center", 115.263, -8.507},
	} {
		db.Put("spots/"+p.key, fmt.Sprintf(
			`{"_collection":"spots","_key":"%s","name":"%s","geometry":{"type":"Point","coordinates":[%f,%f]}}`,
			p.key, p.name, p.lon, p.lat))
	}
	rows, _ = db.Query("SELECT name FROM spots WHERE ST_DWithin(geometry, POINT(115.087 -8.829), 20000.0)")
	show("within 20 km of Uluwatu:", rows)

	// ── 4. Vector ────────────────────────────────────────────────────────
	db.Execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, emb VECTOR)")
	db.Execute("CREATE INDEX ON items USING hnsw (emb)")
	for _, p := range [][3]string{
		{"a", "apple", "[1.0, 0.0, 0.0]"},
		{"b", "banana", "[0.0, 1.0, 0.0]"},
		{"c", "cherry", "[0.9, 0.1, 0.0]"},
	} {
		db.Execute(fmt.Sprintf(
			"INSERT INTO items (_key, name, emb) VALUES ('%s', '%s', %s)", p[0], p[1], p[2]))
	}
	rows, _ = db.Query("SELECT name FROM items WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 2)")
	show("2 nearest to [1,0,0]:", rows)

	// ── 5. Hybrid: text + vector in one ORDER BY ─────────────────────────
	db.Execute("CREATE TABLE dishes (_key TEXT PRIMARY KEY, name TEXT, description TEXT, embedding VECTOR)")
	db.Execute("CREATE INDEX ON dishes USING bm25 (description)")
	db.Execute("CREATE INDEX ON dishes USING hnsw (embedding)")
	for _, p := range [][4]string{
		{"a", "Grilled Chicken", "healthy grilled chicken with herbs", "[1.0, 0.0, 0.0]"},
		{"b", "Fried Rice", "classic fried rice street food", "[0.0, 1.0, 0.0]"},
		{"c", "Grilled Fish", "grilled fish, light and healthy", "[0.8, 0.2, 0.0]"},
	} {
		db.Execute(fmt.Sprintf(
			"INSERT INTO dishes (_key, name, description, embedding) VALUES ('%s', '%s', '%s', %s)",
			p[0], p[1], p[2], p[3]))
	}
	rows, _ = db.Query("SELECT name FROM dishes WHERE BM25(description, 'grilled healthy') > 0.0 " +
		"ORDER BY BM25_NORM(description, 'grilled healthy') * 0.6 " +
		"+ VECTOR_COSINE(embedding, [1.0, 0.0, 0.0]) * 0.4 DESC")
	show("ranked dishes:", rows)
}
