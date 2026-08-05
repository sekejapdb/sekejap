# Swift

Install with Swift Package Manager (iOS + macOS):

```swift
// Package.swift
.package(url: "https://github.com/sekejapdb/sekejap-swift.git", from: "0.13.0")
```

## First query

```swift
import Sekejap

let db = try SekejapDB(path: "./mydb")
try db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)")
try db.execute("INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')")

let rows = try db.query("SELECT * FROM places")   // JSON string
print(rows)
```

Methods throw where the engine can fail. API: `execute`, `query` /
`query(_:params:)`, `put` / `get`, `link`, `contains`,
`nodeCount` / `edgeCount`, `compact`, `sekejapVersion()`.

On iOS the database is a directory inside your app's container — no server
process, works offline. Full build notes:
[`wrappers/swift/`](../../../wrappers/swift/).
