package life.sekejap

/** Accumulates positional parameters while a filter renders to SGQL. Params are
 *  serialized with a hand-rolled JSON writer (no kotlinx allocation per call) —
 *  this is the write-path hot loop for update/delete. */
class SqlContext {
    private val params = mutableListOf<Any?>()

    fun placeholder(value: Any?): String {
        params.add(value)
        return "\$${params.size}"
    }

    fun paramsJson(): String = buildString {
        append('[')
        params.forEachIndexed { i, v ->
            if (i > 0) append(',')
            appendJson(v)
        }
        append(']')
    }

    private fun StringBuilder.appendJson(v: Any?) {
        when (v) {
            null -> append("null")
            is Boolean, is Int, is Long, is Double, is Float -> append(v.toString())
            is String -> { append('"'); appendEscaped(v); append('"') }
            else -> { append('"'); appendEscaped(v.toString()); append('"') }
        }
    }

    private fun StringBuilder.appendEscaped(s: String) {
        for (c in s) when (c) {
            '"' -> append("\\\"")
            '\\' -> append("\\\\")
            '\n' -> append("\\n")
            '\r' -> append("\\r")
            '\t' -> append("\\t")
            else -> append(c)
        }
    }
}

/** A boolean predicate over a row. Compose with [and] / [or]. */
sealed class Filter {
    abstract fun render(ctx: SqlContext): String
    infix fun and(other: Filter): Filter = AndFilter(this, other)
    infix fun or(other: Filter): Filter = OrFilter(this, other)
}

/** A typed column reference — the receiver in `where { it.field eq x }`. */
class Col<T>(val name: String) {
    infix fun eq(value: T): Filter = Compare(name, "=", value)
    infix fun neq(value: T): Filter = Compare(name, "!=", value)
    infix fun lt(value: T): Filter = Compare(name, "<", value)
    infix fun lte(value: T): Filter = Compare(name, "<=", value)
    infix fun gt(value: T): Filter = Compare(name, ">", value)
    infix fun gte(value: T): Filter = Compare(name, ">=", value)
    fun between(lo: T, hi: T): Filter = Between(name, lo, hi)
}

class Compare(val column: String, val op: String, val value: Any?) : Filter() {
    override fun render(ctx: SqlContext) = "$column $op ${ctx.placeholder(value)}"
}

class Between(val column: String, val lo: Any?, val hi: Any?) : Filter() {
    override fun render(ctx: SqlContext) =
        "$column BETWEEN ${ctx.placeholder(lo)} AND ${ctx.placeholder(hi)}"
}

class AndFilter(val a: Filter, val b: Filter) : Filter() {
    override fun render(ctx: SqlContext) = "(${a.render(ctx)} AND ${b.render(ctx)})"
}

class OrFilter(val a: Filter, val b: Filter) : Filter() {
    override fun render(ctx: SqlContext) = "(${a.render(ctx)} OR ${b.render(ctx)})"
}

/** A WGS84 longitude/latitude point for spatial predicates. */
data class GeoPoint(val lon: Double, val lat: Double) {
    fun toSql(): String = "POINT($lon $lat)"
}

/** Vector similarity used for ranking (higher is nearer, ranked DESC). */
enum class VectorMetric(val fn: String) { COSINE("VECTOR_COSINE"), DOT("VECTOR_DOT") }
