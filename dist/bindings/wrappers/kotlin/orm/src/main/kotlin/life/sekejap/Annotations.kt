package life.sekejap

/**
 * Marks a class as a stored entity (a collection). The KSP processor generates a
 * typed `db.<collection>` accessor, a columns object for `where`/`sortBy`,
 * (de)serializers, and a schema descriptor. The generated code lowers to the
 * same SGQL the engine runs — a typed front-end, not a second engine.
 */
@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.SOURCE)
annotation class SekejapEntity(val collection: String = "")

/** The primary-key property. Exactly one per entity. Maps to `_key`. */
@Target(AnnotationTarget.PROPERTY)
@Retention(AnnotationRetention.SOURCE)
annotation class Key

enum class IndexKind { BTREE, HASH }

/** A scalar index on the property (equality/range use it instead of a scan). */
@Target(AnnotationTarget.PROPERTY)
@Retention(AnnotationRetention.SOURCE)
annotation class Index(val kind: IndexKind = IndexKind.BTREE)

/** A `GeoPoint` property, spatially indexed. */
@Target(AnnotationTarget.PROPERTY)
@Retention(AnnotationRetention.SOURCE)
annotation class Geo

/** A `List<Double>` vector property of the given dimension (ANN indexed). */
@Target(AnnotationTarget.PROPERTY)
@Retention(AnnotationRetention.SOURCE)
annotation class Vector(val dim: Int)

/** A text property indexed for BM25 full-text ranking. */
@Target(AnnotationTarget.PROPERTY)
@Retention(AnnotationRetention.SOURCE)
annotation class Bm25
