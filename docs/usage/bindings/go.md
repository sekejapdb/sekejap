# Go

Install as a Go module:

```bash
go get github.com/sekejapdb/sekejap/wrappers/go
```

The binding uses cgo over the C ABI, so a C toolchain must be present
(standard on macOS/Linux dev machines).

## First query

```go
import sekejap "github.com/sekejapdb/sekejap/wrappers/go"

db, err := sekejap.Open("./mydb")
if err != nil { panic(err) }
defer db.Close()

db.Execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)")
db.Execute("INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')")

rows, _ := db.Query("SELECT * FROM places")   // []map[string]any
fmt.Println(rows)
```

API: `Open`, `Execute`, `Query` / `QueryParams`, `Put` / `Get`, `Link`,
`Contains`, `NodeCount` / `EdgeCount`, `Compact`, `Version` — idiomatic Go
(`error` returns, maps for rows).

Full build notes: [`wrappers/go/`](../../../wrappers/go/); runnable tour:
[`wrappers/go/examples/tour/`](../../../wrappers/go/examples/tour/).
