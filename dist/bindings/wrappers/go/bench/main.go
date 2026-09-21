// Cross-wrapper micro-benchmark, Go over the C ABI (cgo). See bench_native.rs.
package main

import (
	"fmt"
	"os"
	"strconv"
	"time"

	sekejap "github.com/sekejapdb/sekejap/wrappers/go"
)

func main() {
	dir, _ := os.MkdirTemp("", "skbench_go")
	defer os.RemoveAll(dir)

	db, _ := sekejap.Open(dir)
	defer db.Close()
	db.Execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
	for i := 0; i < 1000; i++ {
		db.Execute(fmt.Sprintf("INSERT INTO t (_key, v) VALUES ('k%d', %d)", i, i))
	}

	n := 50000
	if v := os.Getenv("N"); v != "" {
		n, _ = strconv.Atoi(v)
	}
	sql := "SELECT v FROM t WHERE _key = 'k500'"
	// QueryJSON (raw result string, no unmarshal) to match the other bindings —
	// measures pure binding round-trip, not language-specific JSON parsing.
	db.QueryJSON(sql) // warm

	t := time.Now()
	for i := 0; i < n; i++ {
		db.QueryJSON(sql)
	}
	el := time.Since(t).Seconds()
	fmt.Printf("go %.0f %.3f\n", float64(n)/el, el*1e6/float64(n))
}
