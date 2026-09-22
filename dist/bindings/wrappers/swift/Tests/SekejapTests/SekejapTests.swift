import XCTest
@testable import Sekejap

final class SekejapTests: XCTestCase {
    func tempDir(_ tag: String) -> String {
        NSTemporaryDirectory() + "sekejap-swift-\(tag)-\(UUID().uuidString)"
    }

    /// The wrapper's own end-to-end pass, per brief-wrap-common.md: open,
    /// create a collection, put, get, query with a parameter, scan,
    /// prepare + rebind, link + neighbours, tx commit/rollback, count_rows,
    /// an error path that surfaces last_error, close.
    func testEndToEnd() throws {
        let db = try Db(path: tempDir("e2e"))

        // open + version
        print("sekejap \(Db.version), format \(Db.formatVersion)")

        // create a collection
        let created = try db.createCollection("people", fields: [
            Db.FieldSpec(name: "name", kind: "text"),
            Db.FieldSpec(name: "age", kind: "int"),
        ])
        XCTAssertTrue(created)
        let createdAgain = try db.createCollection("people", fields: [
            Db.FieldSpec(name: "name", kind: "text"),
            Db.FieldSpec(name: "age", kind: "int"),
        ])
        XCTAssertFalse(createdAgain, "a second create of the same collection answers false")
        try db.execute("CREATE INDEX idx_people_age ON people (age)")

        // put
        try db.put("people", "alice", document: ["name": "Alice", "age": 30])
        try db.put("people", "bob", document: ["name": "Bob", "age": 25])
        try db.put("people", "carol", document: ["name": "Carol", "age": 40])

        // get
        let alice = try db.get("people", "alice")
        XCTAssertEqual(alice?["name"] as? String, "Alice")
        XCTAssertEqual(alice?["_key"] as? String, "alice")
        XCTAssertNil(try db.get("people", "nobody"))
        XCTAssertTrue(try db.exists("people", "alice"))
        XCTAssertFalse(try db.exists("people", "nobody"))

        // query with a parameter
        let byAge = try db.query("SELECT _key, name FROM people WHERE age = $1", params: [30])
        XCTAssertEqual(byAge.count, 1)
        XCTAssertEqual(byAge.first?["_key"] as? String, "alice")

        // scan
        let scan = try db.scan("people", pageRows: 2)
        var scanned: [String] = []
        while let page = try scan.next() {
            scanned.append(contentsOf: page.compactMap { $0["_key"] as? String })
        }
        XCTAssertEqual(Set(scanned), ["alice", "bob", "carol"])
        scan.close()

        // prepare + rebind
        let stmt = try db.prepare("SELECT _key FROM people WHERE age > $1 ORDER BY age")
        let over20 = try stmt.query(params: [20])
        XCTAssertEqual(Set(over20.compactMap { $0["_key"] as? String }), ["alice", "bob", "carol"])
        let over35 = try stmt.query(params: [35])  // rebind the same compiled statement
        XCTAssertEqual(over35.map { $0["_key"] as? String }, ["carol"])
        XCTAssertEqual(try stmt.rebindable(), .yes)
        stmt.free()

        // link + neighbours
        try db.link(from: RowRef("people", "alice"), edgeType: "knows", to: RowRef("people", "bob"))
        try db.link(from: RowRef("people", "alice"), edgeType: "knows", to: RowRef("people", "carol"))
        let neighbours = try db.neighbours("people", "alice", edgeType: "knows")
        XCTAssertEqual(Set(neighbours.map { $0.key }), ["bob", "carol"])
        XCTAssertTrue(try db.unlink(from: RowRef("people", "alice"), edgeType: "knows", to: RowRef("people", "bob")))
        XCTAssertEqual(try db.neighbours("people", "alice", edgeType: "knows").count, 1)

        // tx commit
        let tx1 = try db.transaction()
        try tx1.put("people", "dave", document: ["name": "Dave", "age": 50])
        try tx1.commit()
        XCTAssertTrue(try db.exists("people", "dave"))

        // tx rollback
        let tx2 = try db.transaction()
        try tx2.put("people", "erin", document: ["name": "Erin", "age": 22])
        try tx2.rollback()
        XCTAssertFalse(try db.exists("people", "erin"))

        // count_rows
        XCTAssertEqual(try db.countRows("people"), 4)
        XCTAssertEqual(try db.scanCountRows("people"), 4)
        XCTAssertEqual(try db.scanCountEdges(), 1)

        // an error path that surfaces last_error
        do {
            _ = try db.query("SELECT '")  // unterminated string literal: a lexer-level syntax error
            XCTFail("expected a syntax error")
        } catch let e as SekejapError {
            XCTAssertFalse(e.message.isEmpty)
            XCTAssertEqual(e.code, .invalid)
        }

        // close
        db.close()
        db.close()  // idempotent
    }

    func testRefusedByName() throws {
        let db = try Db(path: tempDir("refused"))

        XCTAssertThrowsError(try Db.openMemory()) { error in
            XCTAssertEqual((error as? SekejapError)?.code, .refused)
        }
        XCTAssertThrowsError(try db.trimMemory()) { error in
            XCTAssertEqual((error as? SekejapError)?.code, .refused)
        }
        XCTAssertThrowsError(try db.compact()) { error in
            XCTAssertEqual((error as? SekejapError)?.code, .refused)
        }
        XCTAssertThrowsError(try db.show("SHOW tables")) { error in
            XCTAssertEqual((error as? SekejapError)?.code, .refused)
        }

        // Service-only calls, refused by name on a single-mode handle.
        XCTAssertThrowsError(try db.cancel()) { error in
            XCTAssertEqual((error as? SekejapError)?.code, .refused)
        }
    }

    func testServiceModeChangeFeed() throws {
        let db = try Db.openService(path: tempDir("service"))
        try db.createCollection("events", fields: [Db.FieldSpec(name: "v", kind: "int")])

        let sub = try db.subscribe()
        try db.put("events", "e1", document: ["v": 1])
        let change = try db.nextChange(subscription: sub, timeoutMilliseconds: 1000)
        XCTAssertNotNil(change)
        XCTAssertTrue(try db.unsubscribe(sub))
        db.close()
    }

    func testMaintenance() throws {
        let db = try Db(path: tempDir("maint"))
        try db.createCollection("t", fields: [Db.FieldSpec(name: "v", kind: "int")])
        try db.put("t", "a", document: ["v": 1])
        let result = try db.checkpoint()
        XCTAssertTrue(result == .folded || result == .deferred)
        try db.publish()
        let storage = try db.storage()
        XCTAssertNotNil(storage["total_bytes"])
    }
}
