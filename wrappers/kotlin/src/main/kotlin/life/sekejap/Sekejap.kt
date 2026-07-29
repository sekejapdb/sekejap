package life.sekejap

class SekejapException(message: String) : Exception(message)

/**
 * A prepared (compiled) query. Create with [SekejapDB.prepare], run with
 * [SekejapDB.queryPrepared], and [close] when done.
 */
class PreparedStatement internal constructor(internal val handle: Long) : AutoCloseable {
    override fun close() = Ffi.stmtFree(handle)
}

/**
 * An open sekejap database, bound via the **Panama / Foreign Function & Memory API**
 * (JDK 22+) over the C ABI — pure JVM, no native glue. Not safe for concurrent use
 * from multiple threads (wraps single-threaded CoreDB); serialize access.
 * Implements [AutoCloseable].
 *
 * The native library is located via `-Dsekejap.lib=<path>` or `-Djava.library.path`.
 */
class SekejapDB(path: String) : AutoCloseable {
    private val handle: Long = Ffi.open(path).also {
        if (it == 0L) throw SekejapException("open failed")
    }

    /** Run a mutating statement; returns affected rows. */
    fun execute(sql: String): Long {
        val n = Ffi.execute(handle, sql)
        if (n < 0) throw lastError()
        return n
    }

    /** Parameterized mutating statement ($1, $2, …); [paramsJson] is a JSON array string. */
    fun executeParams(sql: String, paramsJson: String): Long {
        val n = Ffi.executeParams(handle, sql, paramsJson)
        if (n < 0) throw lastError()
        return n
    }

    /** Run a SELECT; returns the JSON-array result string. */
    fun query(sql: String): String = Ffi.query(handle, sql) ?: throw lastError()

    /** Parameterized SELECT; [paramsJson] is a JSON array string. */
    fun queryParams(sql: String, paramsJson: String): String =
        Ffi.queryParams(handle, sql, paramsJson) ?: throw lastError()

    /** Insert/replace one node by slug with a JSON payload. */
    fun put(slug: String, payloadJson: String) {
        if (Ffi.put(handle, slug, payloadJson) != 0) throw lastError()
    }

    /** Fetch one node's payload by slug; null if absent. */
    fun get(slug: String): String? = Ffi.get(handle, slug)

    /** Create a plain edge from -> to of the given type. */
    fun link(from: String, to: String, type: String) {
        if (Ffi.link(handle, from, to, type) != 0) throw lastError()
    }

    /** Create an edge carrying attributes (a JSON object). */
    fun linkMeta(from: String, to: String, type: String, metaJson: String) {
        if (Ffi.linkMeta(handle, from, to, type, metaJson) != 0) throw lastError()
    }

    /** Remove an edge from -> to of the given type. */
    fun unlink(from: String, to: String, type: String) {
        if (Ffi.unlink(handle, from, to, type) != 0) throw lastError()
    }

    fun contains(slug: String): Boolean = Ffi.contains(handle, slug)
    fun nodeCount(): Long = Ffi.nodeCount(handle)
    fun edgeCount(): Long = Ffi.edgeCount(handle)

    /** Compact: truncate WAL, rewrite payloads/topology, reclaim RAM. */
    fun compact() {
        if (Ffi.compact(handle) != 0) throw lastError()
    }

    /** Compile a query once for repeated execution (a prepared statement). */
    fun prepare(sql: String): PreparedStatement {
        val h = Ffi.prepare(handle, sql)
        if (h == 0L) throw lastError()
        return PreparedStatement(h)
    }

    /** Run a prepared statement, binding `$1`, `$2`, … from a JSON-array string. */
    fun queryPrepared(stmt: PreparedStatement, paramsJson: String): String =
        Ffi.queryPrepared(handle, stmt.handle, paramsJson) ?: throw lastError()

    override fun close() = Ffi.close(handle)

    private fun lastError(): SekejapException =
        SekejapException(Ffi.lastError(handle) ?: "unknown error")

    companion object {
        /** The library version. */
        fun version(): String = Ffi.version()
    }
}
