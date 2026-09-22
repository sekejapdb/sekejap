// Idiomatic Swift over the sekejap C ABI (dist/ffi, docs/dist/C_ABI.md).
//
// One class per opaque handle: Db (SekejapDb*), Statement (SekejapStmt*),
// Scan (SekejapScan*, shared by sekejap_scan_open and sekejap_query_open --
// the header states they are the same operation under two names), and Tx
// (SekejapTx*). Every derived handle keeps its owning Db alive with a strong
// reference, so Swift's own ARC enforces the ABI's ordering rule ("every
// derived handle must be freed before sekejap_close") rather than a caller
// having to remember it.
//
// Documents, rows and parameters cross the ABI as JSON text; this wrapper
// decodes/encodes them into Swift's native `[String: Any]` / `[Any]` via
// Foundation's JSONSerialization, which is cheap and needs no schema.
import CSekejap
import Foundation

// MARK: - Errors

/// Mirrors `SekejapStatus` (docs/dist/C_ABI.md §1.1): a closed reason code
/// for the last failure on this thread, so a caller can branch without
/// parsing `message`.
public enum SekejapStatus: Int32, Sendable, CustomStringConvertible {
    case ok = 0
    case refused = 1
    case corrupt = 2
    case unsupported = 3
    case io = 4
    case invalid = 5
    case busy = 6
    case unknownRow = 7
    case unknown = 8

    public var description: String {
        switch self {
        case .ok: return "ok"
        case .refused: return "refused"
        case .corrupt: return "corrupt"
        case .unsupported: return "unsupported"
        case .io: return "io"
        case .invalid: return "invalid"
        case .busy: return "busy"
        case .unknownRow: return "unknownRow"
        case .unknown: return "unknown"
        }
    }
}

/// An error surfaced by sekejap: `sekejap_last_error` + `sekejap_last_error_code`
/// read on the calling thread once per failing call (the slot is thread-local
/// and a success clears it, docs/dist/C_ABI.md §1).
public struct SekejapError: Error, CustomStringConvertible, Sendable {
    public let code: SekejapStatus
    public let message: String
    public var description: String { "\(message) [\(code)]" }
}

private func takeString(_ ptr: UnsafeMutablePointer<CChar>?) -> String? {
    guard let ptr = ptr else { return nil }
    defer { sekejap_string_free(ptr) }
    return String(cString: ptr)
}

/// Reads the thread-local error slot. The handle argument the C ABI accepts
/// is ignored there too, which is what lets a failed open with no handle yet
/// still report -- so this reads with no handle at all.
private func lastSekejapError() -> SekejapError {
    let rawCode = sekejap_last_error_code(nil)
    let code = SekejapStatus(rawValue: rawCode) ?? .unknown
    let message = takeString(sekejap_last_error(nil)) ?? "unknown error"
    return SekejapError(code: code, message: message)
}

/// A `NULL` return that might be a clean miss (`sekejap_get`, `sekejap_describe`,
/// the end of a walk, an empty change queue) rather than a failure: the ABI
/// tells the two apart only through the error code left behind
/// (docs/dist/C_ABI.md §1 "a miss is not a failure").
private func optionalString(_ ptr: UnsafeMutablePointer<CChar>?) throws -> String? {
    if let s = takeString(ptr) { return s }
    if sekejap_last_error_code(nil) != 0 { throw lastSekejapError() }
    return nil
}

/// Calls `body` with `s` bridged to a borrowed, null-terminated C string, or
/// with `nil` when `s` is `nil` -- for the ABI's optional `const char*`
/// parameters (`config_json`, `params_json`, `edge_type`), which Swift's
/// automatic String-to-C-string bridging does not reach through an Optional.
@inline(__always)
private func withOptionalCString<R>(_ s: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
    guard let s = s else { return body(nil) }
    return s.withCString(body)
}

// MARK: - JSON

/// The native shapes the ABI's JSON crosses the boundary as: a document or a
/// row is `[String: Any]`, a parameter list or an answer is `[Any]` / `[[String: Any]]`.
enum JSON {
    static func encode(_ value: Any) throws -> String {
        let data: Data
        if JSONSerialization.isValidJSONObject(value) {
            data = try JSONSerialization.data(withJSONObject: value)
        } else {
            // A bare scalar (Int/String/Bool/Double) is not a "valid JSON
            // object" by Foundation's top-level rule; wrap it in a
            // one-element array and strip the brackets back off.
            let wrapped = try JSONSerialization.data(withJSONObject: [value])
            guard let s = String(data: wrapped, encoding: .utf8), s.count >= 2 else {
                throw SekejapError(code: .invalid, message: "could not encode value as JSON")
            }
            return String(s.dropFirst().dropLast())
        }
        guard let s = String(data: data, encoding: .utf8) else {
            throw SekejapError(code: .invalid, message: "could not encode value as JSON")
        }
        return s
    }

    static func decodeAny(_ text: String) throws -> Any {
        try JSONSerialization.jsonObject(with: Data(text.utf8), options: [.fragmentsAllowed])
    }

    static func decodeObject(_ text: String) throws -> [String: Any] {
        guard let obj = try decodeAny(text) as? [String: Any] else {
            throw SekejapError(code: .invalid, message: "expected a JSON object, got: \(text)")
        }
        return obj
    }

    static func decodeArray(_ text: String) throws -> [Any] {
        guard let arr = try decodeAny(text) as? [Any] else {
            throw SekejapError(code: .invalid, message: "expected a JSON array, got: \(text)")
        }
        return arr
    }

    static func decodeRows(_ text: String) throws -> [[String: Any]] {
        try decodeArray(text).map { row in
            guard let obj = row as? [String: Any] else {
                throw SekejapError(code: .invalid, message: "expected a row object, got: \(row)")
            }
            return obj
        }
    }

    static func encodeParams(_ params: [Any]) throws -> String? {
        params.isEmpty ? nil : try encode(params)
    }
}

/// A row's address: the pair a collection and a key always mean together
/// (`Db.put`, `Db.link`, ...).
public struct RowRef: Sendable {
    public let collection: String
    public let key: String
    public init(_ collection: String, _ key: String) {
        self.collection = collection
        self.key = key
    }
}

/// One hop away, from `Db.neighbours` -- the collection is carried because a
/// neighbour can be in a different one than the row it was reached from.
public struct Neighbour {
    public let collection: String
    public let key: String
    public let document: [String: Any]
}

/// Which way an edge points, for `Db.neighbours` (`SekejapDirection`).
public enum Direction: Int32, Sendable {
    case outgoing = 0
    case incoming = 1
    case both = 2
}

/// The third answer of `Statement.rebindable`: not bound yet, so there is
/// nothing to answer (`SEKEJAP_REBIND_UNBOUND`). Not a failure.
public enum Rebindable {
    case yes
    case no
    case unbound
}

/// `Db.checkpoint`'s two non-failure outcomes: `0` from the ABI is DEFERRED
/// (a live reader holds a slot), not failed.
public enum CheckpointResult {
    case folded
    case deferred
}

// MARK: - Db

/// An open sekejap database (`SekejapDb*`). `Send + Sync` in the engine
/// (docs/dist/C_ABI.md §3): a `Db` MAY be called from multiple threads at
/// once. Its derived handles (`Statement`, `Scan`, `Tx`) are each
/// single-threaded and hold a strong reference back to this instance, so
/// Swift's ARC keeps the database open until every derived handle has been
/// released -- the ordering `sekejap_close` requires.
public final class Db {
    fileprivate let handle: OpaquePointer
    private var closed = false

    private init(_ handle: OpaquePointer) {
        self.handle = handle
    }

    /// Open (or create) the database directory at `path`.
    public convenience init(path: String) throws {
        guard let h = sekejap_open(path) else { throw lastSekejapError() }
        self.init(h)
    }

    /// A store configuration for `init(path:config:)`
    /// (`sekejap_open_with_config`); every field is optional and an absent
    /// one keeps sekejap's own default, which is `SyncMode::Full`.
    public struct Config {
        public var budgetBytes: Int?
        public var io: String?     // "buffered" | "direct"
        public var sync: String?   // "full" | "normal" | "off"
        public init(budgetBytes: Int? = nil, io: String? = nil, sync: String? = nil) {
            self.budgetBytes = budgetBytes
            self.io = io
            self.sync = sync
        }
    }

    /// `init(path:)` under a store configuration.
    public convenience init(path: String, config: Config) throws {
        var obj: [String: Any] = [:]
        if let b = config.budgetBytes { obj["budget_bytes"] = b }
        if let io = config.io { obj["io"] = io }
        if let sync = config.sync { obj["sync"] = sync }
        let json = obj.isEmpty ? nil : try JSON.encode(obj)
        guard let h = withOptionalCString(json, { sekejap_open_with_config(path, $0) }) else {
            throw lastSekejapError()
        }
        self.init(h)
    }

    /// Open in SERVICE mode: one writer, parallel readers on a published
    /// snapshot, the change feed, the statement timeout and the cancel
    /// (`docs/dist/OPS_CONTRACT.md` §1-§5).
    public static func openService(path: String) throws -> Db {
        guard let h = sekejap_open_service(path) else { throw lastSekejapError() }
        return Db(h)
    }

    /// Close the handle. Idempotent; also run by `deinit`. Every `Statement`,
    /// `Scan` and `Tx` taken from this handle must already be gone -- Swift's
    /// ARC guarantees that as long as none of them is leaked past this call.
    /// Uncommitted work is discarded: a close is not a commit.
    public func close() {
        guard !closed else { return }
        closed = true
        sekejap_close(handle)
    }

    deinit { if !closed { sekejap_close(handle) } }

    /// `MAJOR.MINOR.PATCH` of the linked library.
    public static var version: String { String(cString: sekejap_version()) }

    /// The disk format this build reads and writes.
    public static var formatVersion: Int32 { sekejap_format_version() }

    // MARK: Documents (§4.3)

    /// Write one document, committed before this call returns. `document`
    /// must carry no `_key` member, or one equal to `key`.
    public func put(_ collection: String, _ key: String, document: [String: Any]) throws {
        let json = try JSON.encode(document)
        if sekejap_put(handle, collection, key, json) != 0 { throw lastSekejapError() }
    }

    /// Write many documents into one collection under ONE commit. A failure
    /// stores none of the batch. Returns the rows written.
    @discardableResult
    public func putMany(_ collection: String, _ rows: [(key: String, document: [String: Any])]) throws -> Int {
        let payload: [[String: Any]] = rows.map { ["key": $0.key, "doc": $0.document] }
        let json = try JSON.encode(payload)
        let n = sekejap_put_many(handle, collection, json)
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    /// One document with `_key` set, or `nil` if the row is absent.
    public func get(_ collection: String, _ key: String) throws -> [String: Any]? {
        guard let json = try optionalString(sekejap_get(handle, collection, key)) else { return nil }
        return try JSON.decodeObject(json)
    }

    /// Whether the row is there.
    public func exists(_ collection: String, _ key: String) throws -> Bool {
        let r = sekejap_exists(handle, collection, key)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// Delete one row and every edge that touches it, committed. `true` if it
    /// was there.
    @discardableResult
    public func delete(_ collection: String, _ key: String) throws -> Bool {
        let r = sekejap_delete(handle, collection, key)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// Open a walk of one collection in stable id order, holding at most
    /// `pageRows` rows at a time (`0` means sekejap's default of 256).
    public func scan(_ collection: String, pageRows: Int = 0) throws -> Scan {
        guard let s = sekejap_scan_open(handle, collection, UInt(pageRows)) else { throw lastSekejapError() }
        return Scan(db: self, handle: s)
    }

    // MARK: SQL (§4.4)

    /// Run one writing statement, committed. Returns the rows it moved; a
    /// statement that only raises a notice returns `0`.
    @discardableResult
    public func execute(_ sql: String, params: [Any] = []) throws -> Int {
        let json = try JSON.encodeParams(params)
        let n = withOptionalCString(json) { sekejap_execute(handle, sql, $0) }
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    /// Run one row-returning statement.
    public func query(_ sql: String, params: [Any] = []) throws -> [[String: Any]] {
        let json = try JSON.encodeParams(params)
        guard let out = withOptionalCString(json, { sekejap_query(handle, sql, $0) }) else {
            throw lastSekejapError()
        }
        return try JSON.decodeRows(take(out))
    }

    /// The plan the engine would build for one statement.
    public func explain(_ sql: String, params: [Any] = []) throws -> String {
        let json = try JSON.encodeParams(params)
        guard let out = withOptionalCString(json, { sekejap_explain(handle, sql, $0) }) else {
            throw lastSekejapError()
        }
        return take(out)
    }

    /// Prepare one statement: PARSED now (a syntax error is reported here),
    /// compiled by its first bind.
    public func prepare(_ sql: String) throws -> Statement {
        guard let s = sekejap_prepare(handle, sql) else { throw lastSekejapError() }
        return Statement(db: self, handle: s)
    }

    /// Run a row-returning statement and open a paged delivery of its
    /// answer: `Scan.next()` hands back at most `pageRows` rows per call
    /// (`0` means 4,096). This bounds the string per call; it does not bound
    /// the answer (docs/dist/C_ABI.md §4.4).
    public func stream(_ sql: String, params: [Any] = [], pageRows: Int = 0) throws -> Scan {
        let json = try JSON.encodeParams(params)
        guard let s = withOptionalCString(json, { sekejap_query_open(handle, sql, $0, UInt(pageRows)) }) else {
            throw lastSekejapError()
        }
        return Scan(db: self, handle: s)
    }

    // MARK: Edges (§4.5)

    /// Link two rows with a typed edge in the base graph context, committed.
    /// Both endpoints must already exist.
    public func link(from: RowRef, edgeType: String, to: RowRef) throws {
        if sekejap_link(handle, from.collection, from.key, edgeType, to.collection, to.key) != 0 {
            throw lastSekejapError()
        }
    }

    /// `link(from:edgeType:to:)` carrying a JSON properties object.
    public func link(from: RowRef, edgeType: String, to: RowRef, properties: [String: Any]) throws {
        let json = try JSON.encode(properties)
        if sekejap_link_with(handle, from.collection, from.key, edgeType, to.collection, to.key, json) != 0 {
            throw lastSekejapError()
        }
    }

    /// Remove one edge, committed. `true` if it was there.
    @discardableResult
    public func unlink(from: RowRef, edgeType: String, to: RowRef) throws -> Bool {
        let r = sekejap_unlink(handle, from.collection, from.key, edgeType, to.collection, to.key)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// The rows one hop away in one direction, under a complete-or-refused
    /// bound of at most 256 edges; a wider walk is SQL's `GRAPH_TABLE`.
    /// `edgeType` of `nil` matches every type.
    public func neighbours(_ collection: String, _ key: String, edgeType: String? = nil,
                            direction: Direction = .outgoing, limit: Int = 256) throws -> [Neighbour] {
        guard let out = withOptionalCString(edgeType, {
            sekejap_neighbours(handle, collection, key, $0, direction.rawValue, UInt(limit))
        }) else { throw lastSekejapError() }
        let arr = try JSON.decodeArray(take(out))
        return try arr.map { item in
            guard let obj = item as? [String: Any],
                  let c = obj["collection"] as? String,
                  let k = obj["key"] as? String,
                  let d = obj["document"] as? [String: Any] else {
                throw SekejapError(code: .invalid, message: "unexpected sekejap_neighbours shape: \(item)")
            }
            return Neighbour(collection: c, key: k, document: d)
        }
    }

    // MARK: The catalog (§4.6)

    /// A field declaration for `createCollection`.
    public struct FieldSpec {
        public let name: String
        public let kind: String  // "text"|"int"|"real"|"bool"|"json"|"geo"|"point"|"vector"
        public let dimension: Int?
        public init(name: String, kind: String, dimension: Int? = nil) {
            self.name = name
            self.kind = kind
            self.dimension = dimension
        }
        fileprivate var json: [String: Any] {
            var o: [String: Any] = ["name": name, "kind": kind]
            if let d = dimension { o["dimension"] = d }
            return o
        }
    }

    /// Declare a collection. Returns `true` if it was created, `false` if it
    /// was already there. The declaration is a floor, not a fence: a
    /// document may carry a field it does not name.
    @discardableResult
    public func createCollection(_ name: String, fields: [FieldSpec]) throws -> Bool {
        let json = try JSON.encode(fields.map { $0.json })
        let r = sekejap_create_collection(handle, name, json)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// Remove a collection, its rows, its indexes and its descriptor.
    /// Returns `true` if it was there.
    @discardableResult
    public func dropCollection(_ name: String) throws -> Bool {
        let r = sekejap_drop_collection(handle, name)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// Every collection name in the catalog, in key order.
    public func collections() throws -> [String] {
        guard let out = takeString(sekejap_collections(handle)) else { throw lastSekejapError() }
        return try JSON.decodeArray(out).compactMap { $0 as? String }
    }

    /// The declared shape of one collection, or `nil` if there is no such
    /// collection.
    public func describe(_ collection: String) throws -> [String: Any]? {
        guard let json = try optionalString(sekejap_describe(handle, collection)) else { return nil }
        return try JSON.decodeObject(json)
    }

    /// The rows of one collection, from the LIVE record when this database
    /// keeps one and from a walk when it does not.
    public func countRows(_ collection: String) throws -> Int {
        let n = sekejap_count_rows(handle, collection)
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    /// Count the rows of one collection BY WALKING them, whether or not a
    /// live record exists.
    public func scanCountRows(_ collection: String) throws -> Int {
        let n = sekejap_scan_count_rows(handle, collection)
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    /// Count every edge BY WALKING the primary edge keyspace: sekejap keeps
    /// no O(1) edge counter, so this is a scan and is named as one.
    public func scanCountEdges() throws -> Int {
        let n = sekejap_scan_count_edges(handle)
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    // MARK: Transactions (§4.7)

    /// Take the writer for many writes under ONE barrier. Every plain `Db`
    /// call above commits per call; `Tx` is the other bargain.
    public func transaction() throws -> Tx {
        guard let t = sekejap_tx_begin(handle) else { throw lastSekejapError() }
        return Tx(db: self, handle: t)
    }

    // MARK: Maintenance (§4.8)

    /// Fold the committed write-ahead log into the data file.
    public func checkpoint() throws -> CheckpointResult {
        switch sekejap_checkpoint(handle) {
        case 1: return .folded
        case 0: return .deferred
        default: throw lastSekejapError()
        }
    }

    /// Make the newest commit visible to readers now.
    public func publish() throws {
        if sekejap_publish(handle) != 0 { throw lastSekejapError() }
    }

    /// The bytes on disk: `{"data_bytes", "wal_bytes", "total_bytes"}`.
    public func storage() throws -> [String: Any] {
        guard let out = takeString(sekejap_storage(handle)) else { throw lastSekejapError() }
        return try JSON.decodeObject(out)
    }

    // MARK: Service mode (§4.9)
    //
    // Every call below is REFUSED BY NAME on a handle not opened with
    // `openService(path:)`.

    /// Refuse a statement that runs longer than `milliseconds`. `0` clears
    /// the timeout.
    public func setStatementTimeout(milliseconds: UInt64) throws {
        if sekejap_statement_timeout_ms(handle, milliseconds) != 0 { throw lastSekejapError() }
    }

    /// Cancel the work in flight on this service, from any thread. STICKY
    /// until `clearInterrupt()`.
    public func cancel() throws {
        if sekejap_cancel(handle) != 0 { throw lastSekejapError() }
    }

    /// Clear a cancel so the service accepts work again. `true` when a
    /// cancel was standing.
    @discardableResult
    public func clearInterrupt() throws -> Bool {
        let r = sekejap_clear_interrupt(handle)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// Subscribe to the commit-time change feed. The id is usable from any
    /// thread; a subscription left open is closed by `close()`.
    public func subscribe() throws -> Int {
        let id = sekejap_subscribe(handle)
        if id < 0 { throw lastSekejapError() }
        return Int(id)
    }

    /// The next change event for `subscription`, or `nil` when none arrived
    /// within `timeoutMilliseconds` (`0` polls and returns at once).
    public func nextChange(subscription: Int, timeoutMilliseconds: UInt64 = 0) throws -> [String: Any]? {
        guard let json = try optionalString(sekejap_next_change(handle, subscription, timeoutMilliseconds)) else {
            return nil
        }
        return try JSON.decodeObject(json)
    }

    /// Close one subscription. `true` when it was open.
    @discardableResult
    public func unsubscribe(_ subscription: Int) throws -> Bool {
        let r = sekejap_unsubscribe(handle, subscription)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    // MARK: Refused by name (§4.10)
    //
    // sekejap has no atomic underneath these; each always fails, with
    // `SekejapStatus.refused` and a message naming what was asked for.

    /// REFUSED: sekejap is disk-first and has no in-memory store. Always
    /// throws; `Db(path:)` takes a directory.
    public static func openMemory() throws -> Db {
        _ = sekejap_open_memory()
        throw lastSekejapError()
    }

    /// REFUSED: sekejap holds nothing proportional to rows to trim. Always
    /// throws.
    public func trimMemory() throws {
        _ = sekejap_trim_memory(handle)
        throw lastSekejapError()
    }

    /// REFUSED: there is no payload-rewriting compaction. `checkpoint()`
    /// folds the write-ahead log; it does not rewrite rows. Always throws.
    public func compact() throws {
        _ = sekejap_compact(handle)
        throw lastSekejapError()
    }

    /// REFUSED: the `SHOW` family is not in this dialect. `collections()`
    /// and `describe(_:)` answer the same questions as data. Always throws.
    public func show(_ statement: String? = nil) throws -> String {
        _ = withOptionalCString(statement) { sekejap_show(handle, $0) }
        throw lastSekejapError()
    }
}

/// Frees `ptr` and returns its contents. Used where the caller has already
/// established `ptr` is non-nil.
private func take(_ ptr: UnsafeMutablePointer<CChar>) -> String {
    defer { sekejap_string_free(ptr) }
    return String(cString: ptr)
}

// MARK: - Statement

/// One statement, parsed once at `Db.prepare` and compiled by its first
/// bind. Free with `free()` (also run by `deinit`), before the owning `Db`
/// closes -- guaranteed here because this instance holds a strong reference
/// to it.
public final class Statement {
    private let db: Db
    fileprivate let handle: OpaquePointer
    private var freed = false

    fileprivate init(db: Db, handle: OpaquePointer) {
        self.db = db
        self.handle = handle
    }

    /// Run this statement as a row-returning one.
    public func query(params: [Any] = []) throws -> [[String: Any]] {
        let json = try JSON.encodeParams(params)
        guard let out = withOptionalCString(json, { sekejap_stmt_query(handle, $0) }) else {
            throw lastSekejapError()
        }
        return try JSON.decodeRows(take(out))
    }

    /// Run this statement as a writing one and commit. Returns the rows it
    /// moved.
    @discardableResult
    public func execute(params: [Any] = []) throws -> Int {
        let json = try JSON.encodeParams(params)
        let n = withOptionalCString(json) { sekejap_stmt_execute(handle, $0) }
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    /// Whether a further bind of this statement compiles nothing. A writing
    /// statement is never rebindable -- its document is folded at compile.
    public func rebindable() throws -> Rebindable {
        switch sekejap_stmt_rebindable(handle) {
        case 1: return .yes
        case 0: return .no
        case 2: return .unbound  // SEKEJAP_REBIND_UNBOUND
        default: throw lastSekejapError()
        }
    }

    /// Free the statement. Idempotent.
    public func free() {
        guard !freed else { return }
        freed = true
        sekejap_stmt_free(handle)
    }

    deinit { if !freed { sekejap_stmt_free(handle) } }
}

// MARK: - Scan

/// A paged walk, of one collection (`Db.scan`) or of one statement's answer
/// (`Db.stream`) -- the ABI states these are the same operation under two
/// names, so this one class serves both.
public final class Scan {
    private let db: Db
    fileprivate let handle: OpaquePointer
    private var closed = false

    fileprivate init(db: Db, handle: OpaquePointer) {
        self.db = db
        self.handle = handle
    }

    /// The next page, or `nil` at the end of the walk.
    public func next() throws -> [[String: Any]]? {
        guard let json = try optionalString(sekejap_scan_next(handle)) else { return nil }
        return try JSON.decodeRows(json)
    }

    /// Close the walk. Idempotent.
    public func close() {
        guard !closed else { return }
        closed = true
        sekejap_scan_close(handle)
    }

    deinit { if !closed { sekejap_scan_close(handle) } }
}

// MARK: - Tx

/// The writer, held across many writes under one commit barrier. Created by
/// `Db.transaction()`; freed by `commit()` or `rollback()` -- both free the
/// handle whether they succeed or not. A `Tx` dropped any other way ROLLS
/// BACK, so `deinit` calls `sekejap_tx_rollback` when neither has run.
public final class Tx {
    private let db: Db
    private var handle: OpaquePointer?

    fileprivate init(db: Db, handle: OpaquePointer) {
        self.db = db
        self.handle = handle
    }

    private func liveHandle() throws -> OpaquePointer {
        guard let h = handle else {
            throw SekejapError(code: .invalid, message: "transaction already committed or rolled back")
        }
        return h
    }

    /// Write one document inside the transaction, with NO commit.
    public func put(_ collection: String, _ key: String, document: [String: Any]) throws {
        let h = try liveHandle()
        let json = try JSON.encode(document)
        if sekejap_tx_put(h, collection, key, json) != 0 { throw lastSekejapError() }
    }

    /// Delete one row inside the transaction, with NO commit. `true` if it
    /// was there.
    @discardableResult
    public func delete(_ collection: String, _ key: String) throws -> Bool {
        let h = try liveHandle()
        let r = sekejap_tx_delete(h, collection, key)
        if r < 0 { throw lastSekejapError() }
        return r == 1
    }

    /// Link two rows inside the transaction, with NO commit.
    public func link(from: RowRef, edgeType: String, to: RowRef) throws {
        let h = try liveHandle()
        if sekejap_tx_link(h, from.collection, from.key, edgeType, to.collection, to.key) != 0 {
            throw lastSekejapError()
        }
    }

    /// Run one writing statement inside the transaction, with NO commit.
    /// Returns the rows it moved.
    @discardableResult
    public func execute(_ sql: String, params: [Any] = []) throws -> Int {
        let h = try liveHandle()
        let json = try JSON.encodeParams(params)
        let n = withOptionalCString(json) { sekejap_tx_execute(h, sql, $0) }
        if n < 0 { throw lastSekejapError() }
        return Int(n)
    }

    /// Commit the transaction and free the handle, whether the commit
    /// succeeds or not.
    public func commit() throws {
        let h = try liveHandle()
        handle = nil
        if sekejap_tx_commit(h) != 0 { throw lastSekejapError() }
    }

    /// Roll the transaction back and free the handle.
    public func rollback() throws {
        let h = try liveHandle()
        handle = nil
        if sekejap_tx_rollback(h) != 0 { throw lastSekejapError() }
    }

    deinit {
        // A handle dropped any other way ROLLS BACK (docs/dist/C_ABI.md §4.7).
        if let h = handle { sekejap_tx_rollback(h) }
    }
}
