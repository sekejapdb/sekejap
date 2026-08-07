package life.sekejap.bench

/** One record, identical across every engine. */
data class DocData(
    val id: String,
    val name: String,
    val category: String,
    val value: Double,
    val ts: Long,
)

fun makeDocs(n: Int): List<DocData> = (0 until n).map { i ->
    DocData(
        id = "k$i",
        name = "name-$i-${i % 97}",
        category = "cat${i % 10}",
        value = (i * 37 % 10000) / 10.0,
        ts = 1_700_000_000L + i,
    )
}

/** The phases every engine implements; the runner does the timing. */
interface EngineBench {
    val name: String
    fun open(dir: String)
    fun insertAll(docs: List<DocData>)
    fun getById(id: String): String?
    fun queryCategory(cat: String): Int
    fun queryRange(lo: Double, hi: Double): Int
    fun update(id: String, newValue: Double)
    fun deleteById(id: String)
    fun close()
    fun dbSizeBytes(dir: String): Long
}

data class PhaseResult(val phase: String, val ms: Double)

fun dirSize(path: String): Long {
    val d = java.io.File(path)
    if (!d.exists()) return 0
    return d.walkTopDown().filter { it.isFile }.sumOf { it.length() }
}

/** Runs the fixed phase sequence against one engine, timing each. */
fun runEngine(
    e: EngineBench,
    baseDir: String,
    docs: List<DocData>,
    log: (String) -> Unit,
): List<PhaseResult> {
    val dir = "$baseDir/${e.name}"
    java.io.File(dir).deleteRecursively()
    java.io.File(dir).mkdirs()

    val out = ArrayList<PhaseResult>()
    fun phase(label: String, body: () -> Unit) {
        val t0 = System.nanoTime()
        body()
        val ms = (System.nanoTime() - t0) / 1_000_000.0
        out.add(PhaseResult(label, ms))
        log("  ${e.name} $label: ${"%.1f".format(ms)} ms")
    }

    phase("open") { e.open(dir) }
    phase("insert_${docs.size}") { e.insertAll(docs) }
    phase("get_1000") {
        for (i in 0 until 1000) e.getById("k${(i * 7) % docs.size}")
    }
    phase("query_cat_100") {
        for (i in 0 until 100) e.queryCategory("cat${i % 10}")
    }
    phase("query_range_100") {
        for (i in 0 until 100) e.queryRange(100.0 + i, 600.0 + i)
    }
    phase("update_1000") {
        for (i in 0 until 1000) e.update("k${(i * 11) % docs.size}", 9999.0 + i)
    }
    phase("delete_1000") {
        for (i in 0 until 1000) e.deleteById("k${docs.size - 1 - i}")
    }
    phase("reopen") { e.close(); e.open(dir) }
    out.add(PhaseResult("disk_kb", e.dbSizeBytes(dir) / 1024.0))
    e.close()
    return out
}
