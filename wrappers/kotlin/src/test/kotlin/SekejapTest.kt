import life.sekejap.SekejapDB
import life.sekejap.SekejapException
import java.nio.file.Files
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

class SekejapTest {
    @Test
    fun roundTrip() {
        val dir = Files.createTempDirectory("sekejap-kotlin").toString()
        SekejapDB(dir).use { db ->
            db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
            assertEquals(1L, db.execute("INSERT INTO t (_key, v) VALUES ('a', 42)"))

            db.put("t/b", """{"_collection":"t","_key":"b","v":7}""")
            assertEquals(2L, db.nodeCount())
            assertTrue(db.contains("t/a"))
            assertFalse(db.contains("t/zzz"))

            val rows = db.query("SELECT v FROM t WHERE _key = 'a'")
            assertTrue(rows.contains("42"), "rows: $rows")

            val params = db.queryParams("SELECT _key FROM t WHERE v = \$1", "[7]")
            assertTrue(params.contains("b"), "params: $params")

            assertNotNull(db.get("t/b"))

            db.link("t/a", "t/b", "near")
            assertEquals(1L, db.edgeCount())

            assertFailsWith<SekejapException> { db.execute("SELECT bad syntax FROM") }

            db.compact()
            println("OK — sekejap ${SekejapDB.version()}")
        }
    }

    @Test
    fun prepared() {
        val dir = Files.createTempDirectory("sekejap-kotlin-prep").toString()
        SekejapDB(dir).use { db ->
            db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
            for (i in 0 until 5) db.execute("INSERT INTO t (_key, v) VALUES ('k$i', $i)")

            db.prepare("SELECT _key FROM t WHERE v = \$1").use { stmt ->
                for (i in 0 until 5) {
                    val rows = db.queryPrepared(stmt, "[$i]")
                    assertTrue(rows.contains("k$i"), "param $i: $rows")
                }
            }
        }
    }
}
