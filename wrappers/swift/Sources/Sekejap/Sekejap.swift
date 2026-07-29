import CSekejap
import Foundation

/// An error from the sekejap engine.
public enum SekejapError: Error, CustomStringConvertible {
    case message(String)
    public var description: String {
        switch self { case .message(let m): return m }
    }
}

/// An open sekejap database. Not safe for concurrent use from multiple threads
/// (wraps single-threaded CoreDB); serialize access.
public final class SekejapDB {
    private let handle: OpaquePointer

    /// Open (or create) a database at `path`.
    public init(path: String) throws {
        guard let h = sekejap_open(path) else {
            throw SekejapError.message("open failed")
        }
        handle = h
    }

    deinit { sekejap_close(handle) }

    /// Run a mutating statement; returns affected rows.
    @discardableResult
    public func execute(_ sql: String) throws -> Int {
        let n = sekejap_execute(handle, sql)
        if n < 0 { throw lastError() }
        return Int(n)
    }

    /// Run a SELECT; returns the JSON-array result string.
    public func query(_ sql: String) throws -> String {
        guard let out = sekejap_query(handle, sql) else { throw lastError() }
        defer { sekejap_string_free(out) }
        return String(cString: out)
    }

    /// Parameterized SELECT ($1, $2, …); `paramsJSON` is a JSON array string.
    public func query(_ sql: String, params paramsJSON: String) throws -> String {
        guard let out = sekejap_query_params(handle, sql, paramsJSON) else { throw lastError() }
        defer { sekejap_string_free(out) }
        return String(cString: out)
    }

    /// Insert/replace one node by slug with a JSON payload.
    public func put(_ slug: String, _ payloadJSON: String) throws {
        if sekejap_put(handle, slug, payloadJSON) != 0 { throw lastError() }
    }

    /// Fetch one node's payload by slug; nil if absent.
    public func get(_ slug: String) -> String? {
        guard let out = sekejap_get(handle, slug) else { return nil }
        defer { sekejap_string_free(out) }
        return String(cString: out)
    }

    /// Create a plain edge from → to of the given type.
    public func link(_ from: String, _ to: String, type: String) {
        sekejap_link(handle, from, to, type)
    }

    /// Whether a node with the given slug exists.
    public func contains(_ slug: String) -> Bool {
        sekejap_contains(handle, slug) == 1
    }

    public var nodeCount: Int { Int(sekejap_node_count(handle)) }
    public var edgeCount: Int { Int(sekejap_edge_count(handle)) }

    /// Compact: truncate WAL, rewrite payloads/topology, reclaim RAM.
    public func compact() throws {
        if sekejap_compact(handle) != 0 { throw lastError() }
    }

    /// Compile a query once for repeated execution (a prepared statement).
    /// Run it with ``query(_:params:)``; use `$1`, `$2`, … placeholders.
    public func prepare(_ sql: String) throws -> PreparedStatement {
        guard let s = sekejap_prepare(handle, sql) else { throw lastError() }
        return PreparedStatement(s)
    }

    /// Run a prepared statement, binding `$1`, `$2`, … from a JSON-array string.
    /// Returns the JSON-array result string.
    public func query(_ stmt: PreparedStatement, params paramsJSON: String = "[]") throws -> String {
        guard let out = sekejap_query_prepared(handle, stmt.handle, paramsJSON) else { throw lastError() }
        defer { sekejap_string_free(out) }
        return String(cString: out)
    }

    private func lastError() -> SekejapError {
        if let e = sekejap_last_error(handle) {
            defer { sekejap_string_free(e) }
            return .message(String(cString: e))
        }
        return .message("unknown error")
    }
}

/// A prepared (compiled) query. Create with ``SekejapDB/prepare(_:)``, run with
/// ``SekejapDB/query(_:params:)``. Freed automatically.
public final class PreparedStatement {
    fileprivate let handle: OpaquePointer
    fileprivate init(_ handle: OpaquePointer) { self.handle = handle }
    deinit { sekejap_stmt_free(handle) }
}

/// The library version.
public func sekejapVersion() -> String {
    String(cString: sekejap_version())
}
