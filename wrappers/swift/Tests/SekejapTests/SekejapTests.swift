import XCTest
@testable import Sekejap

final class SekejapTests: XCTestCase {
    func testRoundTrip() throws {
        let dir = NSTemporaryDirectory() + "sekejap-swift-\(UUID().uuidString)"
        let db = try SekejapDB(path: dir)

        try db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
        XCTAssertEqual(try db.execute("INSERT INTO t (_key, v) VALUES ('a', 42)"), 1)

        try db.put("t/b", #"{"_collection":"t","_key":"b","v":7}"#)
        XCTAssertEqual(db.nodeCount, 2)
        XCTAssertTrue(db.contains("t/a"))
        XCTAssertFalse(db.contains("t/zzz"))

        let rows = try db.query("SELECT v FROM t WHERE _key = 'a'")
        XCTAssertTrue(rows.contains("42"), "rows: \(rows)")

        let params = try db.query("SELECT _key FROM t WHERE v = $1", params: "[7]")
        XCTAssertTrue(params.contains("b"), "params: \(params)")

        XCTAssertNotNil(db.get("t/b"))

        db.link("t/a", "t/b", type: "near")
        XCTAssertEqual(db.edgeCount, 1)

        XCTAssertThrowsError(try db.execute("SELECT bad syntax FROM"))

        try db.compact()
        print("OK — sekejap \(sekejapVersion())")
    }
}

extension SekejapTests {
    func testPrepared() throws {
        let dir = NSTemporaryDirectory() + "sekejap-swift-prep-\(UUID().uuidString)"
        let db = try SekejapDB(path: dir)
        try db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
        for i in 0..<5 { try db.execute("INSERT INTO t (_key, v) VALUES ('k\(i)', \(i))") }

        let stmt = try db.prepare("SELECT _key FROM t WHERE v = $1")
        for i in 0..<5 {
            let rows = try db.query(stmt, params: "[\(i)]")
            XCTAssertTrue(rows.contains("k\(i)"), "param \(i): \(rows)")
        }
    }
}
