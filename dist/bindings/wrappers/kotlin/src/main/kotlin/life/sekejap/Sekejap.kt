@file:JvmName("Sekejap")

package life.sekejap

/**
 * sekejap 0.17.0 for the JVM, over the C ABI `libsekejap`
 * (`dist/ffi/include/sekejap.h`, contract `docs/dist/C_ABI.md`) bound with the
 * Foreign Function & Memory API (Panama, JDK 22+). Pure JVM: no JNI shim, no
 * extra runtime dependency.
 *
 * One class per handle, as the ABI has them: [Db], [Statement], [Scan], [Tx].
 * A document, a parameter list and an answer cross the boundary as JSON TEXT
 * and are handed over as `String`. The JVM has no JSON type in its standard
 * library, so parsing one here would mean picking a library for every caller;
 * the text is returned as the ABI produced it and the caller parses it with
 * whatever it already uses.
 */

/**
 * Why the last call failed, as the closed enumeration the ABI carries
 * (`SekejapStatus`), so a caller maps a failure without reading the message.
 */
enum class Status(val code: Int) {
    /** The call succeeded, or answered a clean miss. */
    Ok(0),

    /** A construct sekejap has no atomic for, refused with its reason. */
    Refused(1),

    /** A page or a log failed verification. Nothing was changed. */
    Corrupt(2),

    /** A format, policy or configuration this build does not implement. */
    Unsupported(3),

    /** The directory, the file or the medium refused. */
    Io(4),

    /** The caller's arguments are wrong. */
    Invalid(5),

    /** A bound refused rather than waiting. */
    Busy(6),

    /** The named row is not in the collection, on a call that needs it. */
    UnknownRow(7),

    /** Nothing above classified it, a panic caught at the boundary included. */
    Unknown(8);

    companion object {
        @JvmStatic
        fun of(code: Int): Status = entries.firstOrNull { it.code == code } ?: Unknown
    }
}

/** A failure from libsekejap: the message it reported and the code it classified it under. */
class SekejapException(message: String, val status: Status) : RuntimeException(message)

/** Which way an edge points, for [Db.neighbours] (`SekejapDirection`). */
enum class Direction(val code: Int) {
    /** Edges that leave the row. */
    Outgoing(0),

    /** Edges that arrive at the row. */
    Incoming(1),

    /** Both, with each neighbour reported once. */
    Both(2),
}

/** What a further bind of a [Statement] would compile (`sekejap_stmt_rebindable`). */
enum class Rebindable {
    /** A further bind compiles nothing. */
    Yes,

    /** A further bind recompiles. Every WRITING statement answers this. */
    No,

    /** Not bound yet, so there is nothing to answer. Not a failure. */
    Unbound,
}

/** Raise the last failure on this thread as an exception. */
internal fun fail(what: String): Nothing {
    val status = Status.of(Ffi.lastErrorCode())
    val message = Ffi.lastError() ?: "$what failed with no message"
    throw SekejapException(message, status)
}

/** Whether the last call left a clean status, which is what separates a MISS from a failure. */
internal fun lastWasClean(): Boolean = Ffi.lastErrorCode() == Status.Ok.code

/**
 * An open sekejap database.
 *
 * `sekejap::Db` is `Send + Sync`, so one handle MAY be used from several
 * threads; a [Statement], a [Scan] and a [Tx] taken from it are each used from
 * one thread at a time. Every derived handle is freed by [close] if the caller
 * has not freed it already, because the ABI requires them gone before
 * `sekejap_close`.
 */
class Db private constructor(handle: Long) : AutoCloseable {

    private var handle: Long = handle
    private val children = linkedSetOf<AutoCloseable>()

    private fun h(): Long {
        val v = handle
        check(v != 0L) { "this Db is closed" }
        return v
    }

    internal fun adopt(child: AutoCloseable) { synchronized(children) { children.add(child) } }
    internal fun disown(child: AutoCloseable) { synchronized(children) { children.remove(child) } }

    companion object {
        /** Open the database in [path], creating it when the directory holds none. */
        @JvmStatic
        fun open(path: String): Db {
            val h = Ffi.open(path)
            if (h == 0L) fail("sekejap_open")
            return Db(h)
        }

        /**
         * [open] under a store configuration, a JSON object
         * `{"budget_bytes": n, "io": "buffered"|"direct", "sync": "full"|"normal"|"off"}`.
         * Every member is optional; `null` keeps sekejap's own defaults.
         */
        @JvmStatic
        @JvmOverloads
        fun openWithConfig(path: String, configJson: String? = null): Db {
            val h = Ffi.openWithConfig(path, configJson)
            if (h == 0L) fail("sekejap_open_with_config")
            return Db(h)
        }

        /**
         * Open in SERVICE mode: one writer, parallel readers on a published
         * snapshot, and the change feed, the statement timeout and the cancel of
         * `docs/dist/OPS_CONTRACT.md`. The service calls answer only on a handle
         * opened this way.
         */
        @JvmStatic
        fun openService(path: String): Db {
            val h = Ffi.openService(path)
            if (h == 0L) fail("sekejap_open_service")
            return Db(h)
        }

        /**
         * REFUSED: sekejap is disk-first and has no in-memory store. Always throws
         * with [Status.Refused]; give [open] a directory.
         */
        @JvmStatic
        fun openMemory(): Nothing {
            Ffi.openMemory()
            fail("sekejap_open_memory")
        }

        /** The library version, as `MAJOR.MINOR.PATCH`. */
        @JvmStatic
        fun version(): String = Ffi.version()

        /** The sekejap disk format this build reads and writes. */
        @JvmStatic
        fun formatVersion(): Int = Ffi.formatVersion()
    }

    // ── documents ────────────────────────────────────────────────────────────

    /**
     * Write one document, committed before this call returns. [documentJson] is a
     * JSON object; a `_key` member in it must equal [key].
     */
    fun put(collection: String, key: String, documentJson: String) {
        if (Ffi.put(h(), collection, key, documentJson) != 0) fail("sekejap_put")
    }

    /**
     * Write many documents into one collection under ONE commit. [rowsJson] is a
     * JSON array of `{"key": "...", "doc": { ... }}`. A failure stores none of the
     * batch. Returns the rows written.
     */
    fun putMany(collection: String, rowsJson: String): Long {
        val n = Ffi.putMany(h(), collection, rowsJson)
        if (n < 0) fail("sekejap_put_many")
        return n
    }

    /** One document with `_key` set, or `null` for a miss. */
    fun get(collection: String, key: String): String? {
        val s = Ffi.get(h(), collection, key)
        if (s == null && !lastWasClean()) fail("sekejap_get")
        return s
    }

    /** Whether the row is there. */
    fun exists(collection: String, key: String): Boolean {
        val r = Ffi.exists(h(), collection, key)
        if (r < 0) fail("sekejap_exists")
        return r == 1
    }

    /** Delete one row and every edge that touches it, committed. True if it was there. */
    fun delete(collection: String, key: String): Boolean {
        val r = Ffi.delete(h(), collection, key)
        if (r < 0) fail("sekejap_delete")
        return r == 1
    }

    /**
     * Open a walk of one collection in stable id order, holding at most [pageRows]
     * rows at a time (`0` means sekejap's default of 256 rows).
     */
    @JvmOverloads
    fun scan(collection: String, pageRows: Long = 0): Scan {
        val s = Ffi.scanOpen(h(), collection, pageRows)
        if (s == 0L) fail("sekejap_scan_open")
        return Scan(this, s, paged = false)
    }

    // ── SQL ──────────────────────────────────────────────────────────────────

    /**
     * Run one writing statement and commit. Returns the rows it moved; a statement
     * that only raises a notice returns 0. [paramsJson] is a JSON array or `null`.
     */
    @JvmOverloads
    fun execute(sql: String, paramsJson: String? = null): Long {
        val n = Ffi.execute(h(), sql, paramsJson)
        if (n < 0) fail("sekejap_execute")
        return n
    }

    /**
     * Run one row-returning statement. The answer is a JSON ARRAY of objects keyed
     * by COLUMN NAME; a column missing in a row is omitted from that object,
     * because missing is not null.
     */
    @JvmOverloads
    fun query(sql: String, paramsJson: String? = null): String =
        Ffi.query(h(), sql, paramsJson) ?: fail("sekejap_query")

    /** The plan the engine would build for one statement. */
    @JvmOverloads
    fun explain(sql: String, paramsJson: String? = null): String =
        Ffi.explain(h(), sql, paramsJson) ?: fail("sekejap_explain")

    /**
     * Prepare one statement. It is PARSED here -- a syntax error is reported now --
     * and compiled by its first bind.
     */
    fun prepare(sql: String): Statement {
        val s = Ffi.prepare(h(), sql)
        if (s == 0L) fail("sekejap_prepare")
        return Statement(this, s)
    }

    /**
     * Run a row-returning statement and open a PAGED DELIVERY of its answer:
     * each [Scan.next] hands back at most [pageRows] rows (`0` means 4,096), so no
     * single string holds the whole answer. This bounds the string per call and
     * lets a caller stop reading; it does not bound the answer.
     */
    @JvmOverloads
    fun stream(sql: String, paramsJson: String? = null, pageRows: Long = 0): Scan {
        val s = Ffi.queryOpen(h(), sql, paramsJson, pageRows)
        if (s == 0L) fail("sekejap_query_open")
        return Scan(this, s, paged = true)
    }

    // ── edges ────────────────────────────────────────────────────────────────

    /**
     * Link two rows with a typed edge in the base graph context, committed. BOTH
     * endpoints must already exist: a missing one is [Status.UnknownRow], never a
     * dangling identity.
     */
    fun link(
        fromCollection: String,
        fromKey: String,
        edgeType: String,
        toCollection: String,
        toKey: String,
    ) {
        if (Ffi.link(h(), fromCollection, fromKey, edgeType, toCollection, toKey) != 0) {
            fail("sekejap_link")
        }
    }

    /** [link] carrying a JSON properties object. */
    fun linkWith(
        fromCollection: String,
        fromKey: String,
        edgeType: String,
        toCollection: String,
        toKey: String,
        propertiesJson: String,
    ) {
        if (Ffi.linkWith(h(), fromCollection, fromKey, edgeType, toCollection, toKey, propertiesJson) != 0) {
            fail("sekejap_link_with")
        }
    }

    /** Remove one edge, committed. True if it was there. */
    fun unlink(
        fromCollection: String,
        fromKey: String,
        edgeType: String,
        toCollection: String,
        toKey: String,
    ): Boolean {
        val r = Ffi.unlink(h(), fromCollection, fromKey, edgeType, toCollection, toKey)
        if (r < 0) fail("sekejap_unlink")
        return r == 1
    }

    /**
     * The rows one hop away, in one direction, under a complete-or-error bound of
     * at most 256 edges: a wider walk is `GRAPH_TABLE` in SQL and is refused here
     * by name. [edgeType] may be `null` for every type.
     *
     * The answer is a JSON array of
     * `{"collection": "...", "key": "...", "document": { ... }}`, because a
     * neighbour can be in another collection and its name is part of the answer.
     */
    @JvmOverloads
    fun neighbours(
        collection: String,
        key: String,
        edgeType: String? = null,
        direction: Direction = Direction.Both,
        limit: Long = 256,
    ): String = Ffi.neighbours(h(), collection, key, edgeType, direction.code, limit)
        ?: fail("sekejap_neighbours")

    // ── the catalog ──────────────────────────────────────────────────────────

    /**
     * Declare a collection. [fieldsJson] is a JSON array of
     * `{"name": "...", "kind": "text"|"int"|"real"|"bool"|"json"|"geo"|"point"|"vector", "dimension": n}`,
     * where `dimension` is required for `vector` and rejected for every other kind.
     * True if it was created, false if it was already there.
     *
     * The declaration is a floor, not a fence: a document may carry a field the
     * declaration does not name, and it is stored in the row's extras.
     */
    fun createCollection(name: String, fieldsJson: String): Boolean {
        val r = Ffi.createCollection(h(), name, fieldsJson)
        if (r < 0) fail("sekejap_create_collection")
        return r == 1
    }

    /** Remove a collection, its rows, its indexes and its descriptor. True if it was there. */
    fun dropCollection(name: String): Boolean {
        val r = Ffi.dropCollection(h(), name)
        if (r < 0) fail("sekejap_drop_collection")
        return r == 1
    }

    /** Every collection name in the catalog, in key order, as a JSON array of strings. */
    fun collections(): String = Ffi.collections(h()) ?: fail("sekejap_collections")

    /**
     * The declared shape of one collection, as a JSON object
     * `{"name", "timestamps", "rows", "fields": [...], "indexes": [...]}`, or
     * `null` when there is no such collection. `rows` is the LIVE row count or
     * `null` where this database keeps no record -- `null` is "no record", not
     * "no rows".
     */
    fun describe(collection: String): String? {
        val s = Ffi.describe(h(), collection)
        if (s == null && !lastWasClean()) fail("sekejap_describe")
        return s
    }

    /**
     * The rows of one collection, from the LIVE record when this database keeps one
     * and from the walk when it does not.
     */
    fun countRows(collection: String): Long {
        val n = Ffi.countRows(h(), collection)
        if (n < 0) fail("sekejap_count_rows")
        return n
    }

    /** Count the rows of one collection BY WALKING them, whether or not a record exists. */
    fun scanCountRows(collection: String): Long {
        val n = Ffi.scanCountRows(h(), collection)
        if (n < 0) fail("sekejap_scan_count_rows")
        return n
    }

    /**
     * Count every edge BY WALKING the primary edge keyspace. sekejap keeps no O(1)
     * edge counter, so this is a scan and is named as one.
     */
    fun scanCountEdges(): Long {
        val n = Ffi.scanCountEdges(h())
        if (n < 0) fail("sekejap_scan_count_edges")
        return n
    }

    // ── transactions ─────────────────────────────────────────────────────────

    /**
     * Take the writer for many writes under ONE barrier. Every plain call commits
     * per call; this is the other bargain. While the transaction is open it HOLDS
     * the writer: a call on this handle that needs the writer waits for it.
     */
    fun transaction(): Tx {
        val t = Ffi.txBegin(h())
        if (t == 0L) fail("sekejap_tx_begin")
        return Tx(this, t)
    }

    /**
     * Run [body] inside a transaction: commit when it returns, roll back when it
     * throws.
     */
    fun <T> transaction(body: (Tx) -> T): T {
        val tx = transaction()
        val value: T
        try {
            value = body(tx)
        } catch (e: Throwable) {
            try { tx.rollback() } catch (r: Throwable) { e.addSuppressed(r) }
            throw e
        }
        tx.commit()
        return value
    }

    // ── maintenance ──────────────────────────────────────────────────────────

    /**
     * Fold the committed write-ahead log into the data file. True when it folded,
     * false when a live reader holds a slot and the fold is DEFERRED -- which in
     * service mode is every call. Deferred is not a failure.
     */
    fun checkpoint(): Boolean {
        val r = Ffi.checkpoint(h())
        if (r < 0) fail("sekejap_checkpoint")
        return r == 1
    }

    /**
     * Make the newest commit visible to readers now. In single mode there is no
     * published view to swap and every commit is already visible to this handle, so
     * this succeeds having done nothing.
     */
    fun publish() {
        if (Ffi.publish(h()) != 0) fail("sekejap_publish")
    }

    /** The bytes on disk, as `{"data_bytes": n, "wal_bytes": n, "total_bytes": n}`. */
    fun storage(): String = Ffi.storage(h()) ?: fail("sekejap_storage")

    // ── service mode ─────────────────────────────────────────────────────────

    /**
     * Refuse a statement that runs longer than [milliseconds]; `0` clears the
     * timeout. Refused by name on a handle that is not in service mode.
     */
    fun statementTimeoutMs(milliseconds: Long) {
        if (Ffi.statementTimeoutMs(h(), milliseconds) != 0) fail("sekejap_statement_timeout_ms")
    }

    /** Cancel the work in flight on this service, from any thread. STICKY until cleared. */
    fun cancel() {
        if (Ffi.cancel(h()) != 0) fail("sekejap_cancel")
    }

    /** Clear a cancel so the service accepts work again. True when one was standing. */
    fun clearInterrupt(): Boolean {
        val r = Ffi.clearInterrupt(h())
        if (r < 0) fail("sekejap_clear_interrupt")
        return r == 1
    }

    /**
     * Subscribe to the commit-time change feed; returns the subscription id. The
     * subscription is owned by this handle, so the id is valid from any thread and
     * one left open is closed by [close].
     */
    fun subscribe(): Long {
        val id = Ffi.subscribe(h())
        if (id < 0) fail("sekejap_subscribe")
        return id
    }

    /**
     * The next change event for one subscription, as a JSON object, or `null` when
     * none arrived. [timeoutMs] of `0` polls and returns at once.
     */
    @JvmOverloads
    fun nextChange(subscription: Long, timeoutMs: Long = 0): String? {
        val s = Ffi.nextChange(h(), subscription, timeoutMs)
        if (s == null && !lastWasClean()) fail("sekejap_next_change")
        return s
    }

    /** Close one subscription. True when it was open on the service. */
    fun unsubscribe(subscription: Long): Boolean {
        val r = Ffi.unsubscribe(h(), subscription)
        if (r < 0) fail("sekejap_unsubscribe")
        return r == 1
    }

    // ── refused by name ──────────────────────────────────────────────────────

    /**
     * REFUSED: sekejap holds nothing proportional to rows to trim -- the buffer pool
     * is bounded by `budget_bytes` and the plan cache by its three ceilings. Always
     * throws with [Status.Refused].
     */
    fun trimMemory(): Nothing {
        Ffi.trimMemory(h())
        fail("sekejap_trim_memory")
    }

    /**
     * REFUSED: there is no payload-rewriting compaction. [checkpoint] folds the
     * committed write-ahead log into the data file; it does not rewrite rows.
     * Always throws with [Status.Refused].
     */
    fun compact(): Nothing {
        Ffi.compact(h())
        fail("sekejap_compact")
    }

    /**
     * REFUSED: the `SHOW` family is not in this dialect. [collections] and
     * [describe] answer the same questions as DATA. Always throws with
     * [Status.Refused].
     */
    fun show(statement: String): Nothing {
        Ffi.show(h(), statement)
        fail("sekejap_show")
    }

    // ── closing ──────────────────────────────────────────────────────────────

    /**
     * Close the handle. Uncommitted work is discarded: a close is not a commit.
     * Every statement, walk and transaction still open on this handle is freed
     * first, because the ABI requires them gone before `sekejap_close`.
     */
    override fun close() {
        val open = handle
        if (open == 0L) return
        val pending = synchronized(children) { children.toList().also { children.clear() } }
        for (child in pending.asReversed()) {
            runCatching { child.close() }
        }
        handle = 0L
        Ffi.close(open)
    }
}

/**
 * One statement, parsed once at [Db.prepare] and compiled by its first bind.
 * Freed by [close], which must happen before the database is closed -- [Db.close]
 * does it for a statement left open.
 */
class Statement internal constructor(private val db: Db, handle: Long) : AutoCloseable {

    private var handle: Long = handle

    init { db.adopt(this) }

    private fun h(): Long {
        val v = handle
        check(v != 0L) { "this Statement is freed" }
        return v
    }

    /** Run it as a row-returning statement; the same JSON shape as [Db.query]. */
    @JvmOverloads
    fun query(paramsJson: String? = null): String =
        Ffi.stmtQuery(h(), paramsJson) ?: fail("sekejap_stmt_query")

    /** Run it as a writing statement and commit. Returns the rows it moved. */
    @JvmOverloads
    fun execute(paramsJson: String? = null): Long {
        val n = Ffi.stmtExecute(h(), paramsJson)
        if (n < 0) fail("sekejap_stmt_execute")
        return n
    }

    /**
     * Whether a further bind of this statement compiles nothing. A writing
     * statement is never rebindable -- its document is folded at compile -- and
     * says so here rather than pretending.
     */
    fun rebindable(): Rebindable = when (val r = Ffi.stmtRebindable(h())) {
        1 -> Rebindable.Yes
        0 -> Rebindable.No
        2 -> Rebindable.Unbound
        else -> fail("sekejap_stmt_rebindable ($r)")
    }

    override fun close() {
        val open = handle
        if (open == 0L) return
        handle = 0L
        db.disown(this)
        Ffi.stmtFree(open)
    }
}

/**
 * A paged walk: of one collection ([Db.scan]) or of one statement's answer
 * ([Db.stream]). Freed by [close], before the database is closed.
 */
class Scan internal constructor(
    private val db: Db,
    handle: Long,
    private val paged: Boolean,
) : AutoCloseable {

    private var handle: Long = handle

    init { db.adopt(this) }

    private fun h(): Long {
        val v = handle
        check(v != 0L) { "this Scan is closed" }
        return v
    }

    /**
     * The next page, as a JSON array -- of documents for a collection walk, of
     * objects keyed by column name for a statement's answer -- or `null` at the END
     * of the walk.
     */
    fun next(): String? {
        val open = h()
        val s = if (paged) Ffi.queryNext(open) else Ffi.scanNext(open)
        if (s == null && !lastWasClean()) fail(if (paged) "sekejap_query_next" else "sekejap_scan_next")
        return s
    }

    /** The remaining pages, one page per element, read as the sequence is consumed. */
    fun pages(): Sequence<String> = generateSequence { next() }

    override fun close() {
        val open = handle
        if (open == 0L) return
        handle = 0L
        db.disown(this)
        if (paged) Ffi.queryClose(open) else Ffi.scanClose(open)
    }
}

/**
 * The writer, held across many writes. Created by [Db.transaction] and consumed
 * by [commit] or [rollback]; a handle closed any other way ROLLS BACK, because
 * committing on a stray close would make an abandoned batch durable.
 */
class Tx internal constructor(private val db: Db, handle: Long) : AutoCloseable {

    private var handle: Long = handle

    init { db.adopt(this) }

    private fun h(): Long {
        val v = handle
        check(v != 0L) { "this Tx is finished" }
        return v
    }

    /** Whether the transaction is still open. */
    val isOpen: Boolean get() = handle != 0L

    /** Write one document inside the transaction, with NO commit. */
    fun put(collection: String, key: String, documentJson: String) {
        if (Ffi.txPut(h(), collection, key, documentJson) != 0) fail("sekejap_tx_put")
    }

    /** Delete one row inside the transaction, with NO commit. True if it was there. */
    fun delete(collection: String, key: String): Boolean {
        val r = Ffi.txDelete(h(), collection, key)
        if (r < 0) fail("sekejap_tx_delete")
        return r == 1
    }

    /** Link two rows inside the transaction, with NO commit. */
    fun link(
        fromCollection: String,
        fromKey: String,
        edgeType: String,
        toCollection: String,
        toKey: String,
    ) {
        if (Ffi.txLink(h(), fromCollection, fromKey, edgeType, toCollection, toKey) != 0) {
            fail("sekejap_tx_link")
        }
    }

    /** Run one writing statement inside the transaction, with NO commit. */
    @JvmOverloads
    fun execute(sql: String, paramsJson: String? = null): Long {
        val n = Ffi.txExecute(h(), sql, paramsJson)
        if (n < 0) fail("sekejap_tx_execute")
        return n
    }

    /**
     * Commit and FREE the handle, whether the commit succeeded or not: the handle
     * is finished after this call in both cases.
     */
    fun commit() {
        val open = h()
        handle = 0L
        db.disown(this)
        if (Ffi.txCommit(open) != 0) fail("sekejap_tx_commit")
    }

    /** Roll back and FREE the handle. As [commit], the handle is finished either way. */
    fun rollback() {
        val open = h()
        handle = 0L
        db.disown(this)
        if (Ffi.txRollback(open) != 0) fail("sekejap_tx_rollback")
    }

    /** Roll back if the transaction is still open, so `use` cannot leave the writer held. */
    override fun close() {
        if (isOpen) rollback()
    }
}
