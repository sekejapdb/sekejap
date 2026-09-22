// Runnable example for the sekejap Go binding.
//
//	cargo build --release -p sekejap-capi   # build libsekejap first (dist/ffi)
//	go run ./dist/bindings/wrappers/go/examples
package main

import (
	"fmt"
	"os"

	sekejap "github.com/sekejapdb/sekejap/dist/bindings/wrappers/go"
)

func main() {
	dir, _ := os.MkdirTemp("", "sekejap_example")
	defer os.RemoveAll(dir)

	db, err := sekejap.Open(dir)
	if err != nil {
		panic(err)
	}
	defer db.Close()

	must(db.Execute("CREATE TABLE places (key TEXT PRIMARY KEY, name TEXT, area TEXT)"))
	must(db.Execute("INSERT INTO places (key, name, area) VALUES ($1, $2, $3)", "uluwatu", "Uluwatu Temple", "south"))
	must(db.Execute("INSERT INTO places (key, name, area) VALUES ($1, $2, $3)", "kuta", "Kuta Beach", "south"))
	must(db.Execute("INSERT INTO places (key, name, area) VALUES ($1, $2, $3)", "ubud", "Ubud Center", "central"))

	// An edge (collection + key on each end, not one slug string), then a
	// graph query over it.
	if err := db.Link("places", "uluwatu", "near", "places", "kuta"); err != nil {
		panic(err)
	}

	rows, err := db.Query("SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC")
	if err != nil {
		panic(err)
	}
	fmt.Printf("sekejap %s — places per area:\n", sekejap.Version())
	for _, r := range rows {
		fmt.Printf("  %v: %v\n", r["area"], r["n"])
	}

	neighbours, err := db.Neighbours("places", "uluwatu", "near", sekejap.DirectionOutgoing, 10)
	if err != nil {
		panic(err)
	}
	fmt.Println("near Uluwatu:")
	for _, n := range neighbours {
		fmt.Printf("  %s/%s: %v\n", n.Collection, n.Key, n.Document)
	}
}

func must(_ int64, err error) {
	if err != nil {
		panic(err)
	}
}
