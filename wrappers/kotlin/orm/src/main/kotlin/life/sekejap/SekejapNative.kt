package life.sekejap

/**
 * JNI bridge to the sekejap Rust core (`libsekejap_jni`). On Android the library
 * is bundled and found automatically; on desktop/server JVM set
 * `-Dsekejap.jni.path=/abs/path/to/libsekejap_jni.{so,dylib,dll}`.
 *
 * This is the raw surface (string SGQL, JSON strings). The typed layer
 * ([Sekejap] + generated collections) lowers to exactly these calls.
 */
object SekejapNative {
    init {
        val override = System.getProperty("sekejap.jni.path")
        if (override != null) System.load(override) else System.loadLibrary("sekejap_jni")
    }

    external fun open(path: String): Long
    external fun mobileProfile(handle: Long)
    external fun execute(handle: Long, sql: String): Long
    external fun executeParams(handle: Long, sql: String, paramsJson: String): Long
    external fun queryParams(handle: Long, sql: String, paramsJson: String): String
    external fun get(handle: Long, slug: String): String?
    external fun putMany(handle: Long, rowsJson: String): Long
    external fun compact(handle: Long)
    external fun close(handle: Long)

    // Change feed (reactive .watch()).
    external fun watchOpen(handle: Long): Long
    external fun watchNext(watch: Long): String?
    external fun watchClose(handle: Long, watch: Long)
    external fun watchFree(watch: Long)
}
