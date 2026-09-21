package life.sekejap

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject

/**
 * Typed accessor for one collection. A generated `<Entity>Collection` extends
 * this, supplying the collection name, columns, and (de)serializers. Offers
 * typed writes and multi-model query starters, each returning a chainable [Query].
 */
abstract class Collection<T, C> {
    abstract val db: Sekejap
    abstract val collectionName: String
    abstract val columns: C
    abstract fun fromJson(payload: JsonObject): T
    /** The stored payload as a JSON object string (`{"_collection":..,"_key":..}`). */
    abstract fun toJson(entity: T): String
    abstract fun keyOf(entity: T): String

    fun query(): Query<T, C> = Query(db, collectionName, columns, ::fromJson)

    fun where(build: (C) -> Filter): Query<T, C> = query().where(build)
    fun sortBy(desc: Boolean = false, select: (C) -> Col<*>): Query<T, C> = query().sortBy(desc, select)
    fun near(select: (C) -> Col<*>, point: GeoPoint, metres: Double): Query<T, C> = query().near(select, point, metres)
    fun matchText(select: (C) -> Col<*>, terms: String): Query<T, C> = query().matchText(select, terms)
    fun rankByText(select: (C) -> Col<*>, terms: String, weight: Double = 1.0, normalized: Boolean = true): Query<T, C> =
        query().rankByText(select, terms, weight, normalized)
    fun rankByVector(select: (C) -> Col<*>, vector: List<Double>, metric: VectorMetric = VectorMetric.COSINE, weight: Double = 1.0): Query<T, C> =
        query().rankByVector(select, vector, metric, weight)
    fun all(): Query<T, C> = query()
    fun find(): List<T> = query().find()
    fun count(): Int = query().count()
    fun watch(): kotlinx.coroutines.flow.Flow<List<T>> = query().watch()

    // ── writes ──────────────────────────────────────────────────────────────────

    fun put(entity: T) = putAll(listOf(entity))

    fun putAll(entities: kotlin.collections.List<T>) {
        val sb = StringBuilder(entities.size * 96)
        sb.append('[')
        entities.forEachIndexed { i, e ->
            if (i > 0) sb.append(',')
            sb.append("[\"").append(collectionName).append('/').append(keyOf(e))
                .append("\",").append(toJson(e)).append(']')
        }
        sb.append(']')
        SekejapNative.putMany(db.handle, sb.toString())
    }

    fun get(key: String): T? {
        val raw = SekejapNative.get(db.handle, "$collectionName/$key") ?: return null
        return fromJson(Json.parseToJsonElement(raw).jsonObject)
    }

    fun delete(key: String) {
        SekejapNative.executeParams(
            db.handle,
            "DELETE FROM $collectionName WHERE _key = \$1",
            "[\"$key\"]",
        )
    }
}
