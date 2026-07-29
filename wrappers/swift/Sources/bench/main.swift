// Cross-wrapper micro-benchmark, Swift (SwiftPM over the C ABI). See bench_native.rs.
import Foundation
import Sekejap

let dir = NSTemporaryDirectory() + "skbench-swift-\(UUID().uuidString)"
let db = try! SekejapDB(path: dir)
try! db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
for i in 0..<1000 {
    try! db.execute("INSERT INTO t (_key, v) VALUES ('k\(i)', \(i))")
}

let n = Int(ProcessInfo.processInfo.environment["N"] ?? "50000")!
let sql = "SELECT v FROM t WHERE _key = 'k500'"
_ = try! db.query(sql) // warm

let start = Date()
for _ in 0..<n { _ = try! db.query(sql) }
let el = -start.timeIntervalSinceNow
print("swift \(String(format: "%.0f", Double(n) / el)) \(String(format: "%.3f", el * 1e6 / Double(n)))")
