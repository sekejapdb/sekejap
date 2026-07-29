// Cross-wrapper micro-benchmark, Kotlin (JNA over the C ABI). See bench_native.rs.
import life.sekejap.SekejapDB
import java.nio.file.Files

fun main() {
    val dir = Files.createTempDirectory("skbench-kotlin").toString()
    SekejapDB(dir).use { db ->
        db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
        for (i in 0 until 1000) db.execute("INSERT INTO t (_key, v) VALUES ('k$i', $i)")

        val n = (System.getenv("N") ?: "50000").toInt()
        val sql = "SELECT v FROM t WHERE _key = 'k500'"
        db.query(sql) // warm

        val t = System.nanoTime()
        for (i in 0 until n) db.query(sql)
        val el = (System.nanoTime() - t) / 1e9
        println("kotlin %.0f %.3f".format(n / el, el * 1e6 / n))
    }
}
