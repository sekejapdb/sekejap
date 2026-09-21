// Runnable example for the sekejap Go binding.
//
//	cargo build --release -p sekejap-capi   # build libsekejap first
//	go run ./wrappers/go/examples
package main

import (
	"fmt"
	"os"

	sekejap "github.com/sekejapdb/sekejap/wrappers/go"
)

func main() {
	dir, _ := os.MkdirTemp("", "sekejap_example")
	defer os.RemoveAll(dir)

	db, err := sekejap.Open(dir)
	if err != nil {
		panic(err)
	}
	defer db.Close()

	must(db.Execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)"))
	must(db.Execute("INSERT INTO places (_key, name, area) VALUES ('uluwatu', 'Uluwatu Temple', 'south')"))
	must(db.Execute("INSERT INTO places (_key, name, area) VALUES ('kuta', 'Kuta Beach', 'south')"))
	must(db.Execute("INSERT INTO places (_key, name, area) VALUES ('ubud', 'Ubud Center', 'central')"))

	// An edge, then a graph query.
	_ = db.Link("places/uluwatu", "places/kuta", "near")

	rows, err := db.Query("SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC")
	if err != nil {
		panic(err)
	}
	fmt.Printf("sekejap %s — places per area:\n", sekejap.Version())
	for _, r := range rows {
		fmt.Printf("  %v: %v\n", r["area"], r["n"])
	}
}

func must(_ int64, err error) {
	if err != nil {
		panic(err)
	}
}
