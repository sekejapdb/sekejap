package life.sekejap

import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

/**
 * A composable, typed, multi-model query over one collection producing `List<T>`.
 * `C` is the generated columns type (the `it` in `where { it.field eq x }`). A
 * query lowers to a single SGQL statement and maps result payloads back to `T`.
 */
class Query<T, C>(
    private val db: Sekejap,
    private val collection: String,
    private val columns: C,
    private val fromJson: (JsonObject) -> T,
) {
    private var where: Filter? = null
    private val extraWhere = mutableListOf<String>()
    private var orderBy: String? = null
    private var desc = false
    private val rankTerms = mutableListOf<String>()
    private var limit: Int? = null
    private var offset: Int? = null

    fun where(build: (C) -> Filter): Query<T, C> {
        val f = build(columns)
        where = where?.let { AndFilter(it, f) } ?: f
        return this
    }

    fun sortBy(desc: Boolean = false, select: (C) -> Col<*>): Query<T, C> {
        orderBy = select(columns).name; this.desc = desc; return this
    }

    fun near(select: (C) -> Col<*>, point: GeoPoint, metres: Double): Query<T, C> {
        extraWhere.add("ST_DWithin(${select(columns).name}, ${point.toSql()}, $metres)")
        return this
    }

    fun matchText(select: (C) -> Col<*>, terms: String): Query<T, C> {
        extraWhere.add("BM25(${select(columns).name}, ${str(terms)}) > 0.0")
        return this
    }

    fun rankByText(select: (C) -> Col<*>, terms: String, weight: Double = 1.0, normalized: Boolean = true): Query<T, C> {
        val fn = if (normalized) "BM25_NORM" else "BM25"
        rankTerms.add("$fn(${select(columns).name}, ${str(terms)}) * $weight")
        return this
    }

    fun rankByVector(select: (C) -> Col<*>, vector: List<Double>, metric: VectorMetric = VectorMetric.COSINE, weight: Double = 1.0): Query<T, C> {
        rankTerms.add("${metric.fn}(${select(columns).name}, ${vec(vector)}) * $weight")
        return this
    }

    fun limitTo(n: Int): Query<T, C> { limit = n; return this }
    fun offsetBy(n: Int): Query<T, C> { offset = n; return this }

    // ── lowering ──────────────────────────────────────────────────────────────

    /** Flatten top-level ANDs so `value >= x AND value <= y` stays a flat
     *  conjunction (no wrapping parens) — the form the btree range scan detects. */
    private fun clauses(ctx: SqlContext): List<String> {
        val out = mutableListOf<String>()
        fun flatten(f: Filter) {
            if (f is AndFilter) { flatten(f.a); flatten(f.b) } else out.add(f.render(ctx))
        }
        where?.let { flatten(it) }
        out.addAll(extraWhere)
        return out
    }

    private fun build(projection: String = "*"): Pair<String, String> {
        val ctx = SqlContext()
        val sb = StringBuilder("SELECT $projection FROM $collection")
        val cs = clauses(ctx)
        if (cs.isNotEmpty()) sb.append(" WHERE ").append(cs.joinToString(" AND "))
        if (rankTerms.isNotEmpty()) sb.append(" ORDER BY ").append(rankTerms.joinToString(" + ")).append(" DESC")
        else orderBy?.let { sb.append(" ORDER BY ").append(it).append(if (desc) " DESC" else " ASC") }
        limit?.let { sb.append(" LIMIT ").append(it) }
        offset?.let { sb.append(" OFFSET ").append(it) }
        return sb.toString() to ctx.paramsJson()
    }

    fun find(): List<T> {
        val (sql, params) = build()
        val rows = Json.parseToJsonElement(SekejapNative.queryParams(db.handle, sql, params)) as JsonArray
        return rows.map { fromJson((it.jsonObject["payload"]!!).jsonObject) }
    }

    fun findFirst(): T? {
        val saved = limit; limit = 1
        val r = find(); limit = saved
        return r.firstOrNull()
    }

    fun count(): Int {
        val (sql, params) = build(projection = "COUNT(*) AS n")
        val rows = Json.parseToJsonElement(SekejapNative.queryParams(db.handle, sql, params)) as JsonArray
        if (rows.isEmpty()) return 0
        return rows[0].jsonObject["payload"]!!.jsonObject["n"]!!.jsonPrimitive.int
    }

    /** `UPDATE … SET … WHERE …`; returns rows changed. */
    fun update(assignments: Map<String, Any?>): Int {
        val ctx = SqlContext()
        val sets = assignments.entries.joinToString(", ") { "${it.key} = ${ctx.placeholder(it.value)}" }
        val sb = StringBuilder("UPDATE $collection SET $sets")
        val cs = clauses(ctx)
        if (cs.isNotEmpty()) sb.append(" WHERE ").append(cs.joinToString(" AND "))
        return SekejapNative.executeParams(db.handle, sb.toString(), ctx.paramsJson()).toInt()
    }

    /** `DELETE FROM … WHERE …`; returns rows deleted. */
    fun deleteAll(): Int {
        val ctx = SqlContext()
        val sb = StringBuilder("DELETE FROM $collection")
        val cs = clauses(ctx)
        if (cs.isNotEmpty()) sb.append(" WHERE ").append(cs.joinToString(" AND "))
        return SekejapNative.executeParams(db.handle, sb.toString(), ctx.paramsJson()).toInt()
    }

    /**
     * Reactive results: emits the current list immediately, then a fresh list
     * each time a committed change touches this collection. Cancelling the
     * collection releases the native listener. A dedicated daemon thread parks
     * on the native change feed (blocking JNI), so cancellation is delivered via
     * [SekejapNative.watchClose]'s stop signal, not by interrupting the thread.
     */
    fun watch(): Flow<List<T>> = callbackFlow {
        val wh = SekejapNative.watchOpen(db.handle)
        trySend(find())
        val worker = Thread {
            try {
                while (true) {
                    val evJson = SekejapNative.watchNext(wh) ?: break
                    val cols = (Json.parseToJsonElement(evJson).jsonObject["collections"]!!.jsonArray)
                        .map { it.jsonPrimitive.content }
                    if (collection in cols) trySend(find())
                }
            } catch (_: Throwable) {
                // channel closed / db gone
            } finally {
                SekejapNative.watchFree(wh) // sole owner frees, after watchNext returned null
            }
        }.apply { isDaemon = true; name = "sekejap-watch-$collection"; start() }

        awaitClose {
            // Unsubscribe + wake the parked watchNext (which lets `worker` exit).
            SekejapNative.watchClose(db.handle, wh)
        }
    }

    private fun str(s: String) = "'${s.replace("'", "''")}'"
    private fun vec(v: List<Double>) = "[${v.joinToString(", ")}]"
}
