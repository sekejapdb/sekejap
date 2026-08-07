package life.sekejap

/** A generated entity's schema: collection name, CREATE TABLE, and index DDL. */
class EntitySchema(
    val collection: String,
    val createTableSql: String,
    val indexSql: List<String> = emptyList(),
)

/**
 * A typed sekejap database. Open with [Sekejap.open]; obtain typed collections
 * via the generated extensions (e.g. `db.dishes`). Works on Android and on
 * desktop/server JVM over the same JNI core.
 */
class Sekejap private constructor(val handle: Long) : AutoCloseable {

    companion object {
        /**
         * Open a persistent database at [path] and apply [schema]. Set
         * [mobileProfile] for relaxed-durability phone flash (WAL sync = Normal,
         * manual compaction).
         */
        fun open(
            path: String,
            schema: List<EntitySchema> = emptyList(),
            mobileProfile: Boolean = false,
        ): Sekejap {
            val h = SekejapNative.open(path)
            require(h != 0L) { "sekejap: open failed at $path" }
            if (mobileProfile) SekejapNative.mobileProfile(h)
            for (e in schema) {
                // Idempotent: CREATE returns -1 (not an exception) if it already exists.
                SekejapNative.execute(h, e.createTableSql)
                for (idx in e.indexSql) SekejapNative.execute(h, idx)
            }
            return Sekejap(h)
        }
    }

    fun compact() = SekejapNative.compact(handle)
    override fun close() = SekejapNative.close(handle)
}
