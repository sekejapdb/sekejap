package life.sekejap

import java.nio.file.Files
import kotlin.test.Test
import kotlin.test.assertContains
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The Kotlin wrapper against a real libsekejap: every answer is compared with a
 * value this test process holds, never with the wrapper's own second opinion.
 */
class SekejapTest {

    private fun tempDir(name: String): String =
        Files.createTempDirectory("sekejap-kotlin-$name").toString()

    private fun openCities(name: String): Db {
        val db = Db.open(tempDir(name))
        db.createCollection(
            "city",
            """[{"name":"name","kind":"text"},{"name":"people","kind":"int"}]""",
        )
        // QL_CONTRACT section 6: a Tier-1 predicate on a field is answered
        // index-side, so a WHERE or an ORDER BY names an index or is refused.
        db.execute("CREATE INDEX city_people ON city USING btree (people)")
        return db
    }

    @Test
    fun `the library reports the version and the disk format this build carries`() {
        assertEquals("0.17.3", Db.version())
        assertEquals(2, Db.formatVersion())
    }

    @Test
    fun `a collection is declared once and is then in the catalog`() {
        Db.open(tempDir("catalog")).use { db ->
            assertEquals("[]", db.collections())
            assertTrue(db.createCollection("city", """[{"name":"name","kind":"text"}]"""))
            assertFalse(
                db.createCollection("city", """[{"name":"name","kind":"text"}]"""),
                "a second declaration of the same collection reports 0, not 1",
            )
            assertContains(db.collections(), "city")

            val described = assertNotNull(db.describe("city"))
            assertContains(described, "\"name\"")
            assertNull(db.describe("nowhere"), "no such collection is a miss, not a failure")

            assertTrue(db.dropCollection("city"))
            assertFalse(db.dropCollection("city"))
        }
    }

    @Test
    fun `a document written by collection and key is read back with its key set`() {
        openCities("document").use { db ->
            db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":5207145}""")

            val read = assertNotNull(db.get("city", "melbourne"))
            assertContains(read, "\"_key\"")
            assertContains(read, "melbourne")
            assertContains(read, "5207145")

            assertTrue(db.exists("city", "melbourne"))
            assertFalse(db.exists("city", "nowhere"))
            assertNull(db.get("city", "nowhere"), "a miss is null with a clean status")

            assertTrue(db.delete("city", "melbourne"))
            assertFalse(db.delete("city", "melbourne"), "a second delete reports 0, not a failure")
        }
    }

    @Test
    fun `many documents land under one commit and the rows are counted two ways`() {
        openCities("count").use { db ->
            val written = db.putMany(
                "city",
                """[{"key":"a","doc":{"_key":"a","name":"A","people":1}},
                    {"key":"b","doc":{"_key":"b","name":"B","people":2}},
                    {"key":"c","doc":{"_key":"c","name":"C","people":3}}]""",
            )
            assertEquals(3L, written)
            assertEquals(3L, db.countRows("city"))
            assertEquals(3L, db.scanCountRows("city"), "the walk agrees with the record")
        }
    }

    @Test
    fun `a statement with a parameter answers the rows the test process expects`() {
        openCities("query").use { db ->
            val people = mapOf("a" to 10, "b" to 200, "c" to 3000)
            for ((key, n) in people) {
                db.put("city", key, """{"_key":"$key","name":"${key.uppercase()}","people":$n}""")
            }

            val expected = people.filterValues { it > 100 }.keys.sorted()
            val answer = db.query("SELECT _key FROM city WHERE people > \$1", "[100]")
            for (key in expected) assertContains(answer, "\"$key\"")
            assertFalse(answer.contains("\"a\""), "the row below the bound is not in the answer: $answer")

            assertEquals(
                1L,
                db.execute("UPDATE city SET people = \$1 WHERE _key = \$2", """[11, "a"]"""),
                "a writing statement reports the rows it moved",
            )
            assertContains(db.query("SELECT people FROM city WHERE _key = 'a'"), "11")
            val plan = db.explain("SELECT _key FROM city WHERE people > \$1", "[100]")
            assertContains(plan, "driver:", message = "the plan names the driver it would walk")
            assertContains(plan, "city_people", message = "and the index the predicate compiles to")
        }
    }

    @Test
    fun `a walk of a collection hands back every row one page at a time`() {
        openCities("scan").use { db ->
            val keys = (0 until 5).map { "k$it" }
            for (key in keys) db.put("city", key, """{"_key":"$key","name":"$key","people":1}""")

            val pages = db.scan("city", pageRows = 2).use { it.pages().toList() }
            assertEquals(3, pages.size, "five rows at two rows a page is three pages: $pages")
            val seen = pages.joinToString("")
            for (key in keys) assertContains(seen, "\"$key\"")

            val streamed = db.stream("SELECT _key FROM city", pageRows = 2).use { it.pages().toList() }
            assertEquals(3, streamed.size, "the paged answer pages the same way: $streamed")
        }
    }

    @Test
    fun `a prepared statement is parsed once and rebinds without compiling again`() {
        openCities("prepare").use { db ->
            for (i in 0 until 5) db.put("city", "k$i", """{"_key":"k$i","name":"k$i","people":$i}""")

            db.prepare("SELECT _key FROM city WHERE people = \$1").use { stmt ->
                assertEquals(
                    Rebindable.Unbound,
                    stmt.rebindable(),
                    "nothing is bound yet, so there is nothing to answer",
                )
                for (i in 0 until 5) {
                    assertContains(stmt.query("[$i]"), "\"k$i\"")
                }
                assertEquals(
                    Rebindable.Yes,
                    stmt.rebindable(),
                    "a row-returning statement rebinds after its first bind",
                )
            }

            db.prepare("UPDATE city SET people = \$1 WHERE _key = \$2").use { stmt ->
                assertEquals(1L, stmt.execute("""[99, "k0"]"""))
                assertEquals(
                    Rebindable.No,
                    stmt.rebindable(),
                    "a writing statement folds its document at compile and is never rebindable",
                )
            }
            assertContains(db.query("SELECT people FROM city WHERE _key = 'k0'"), "99")
        }
    }

    @Test
    fun `an edge joins two rows and the neighbour answer names the collection it is in`() {
        openCities("edges").use { db ->
            db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":5207145}""")
            db.put("city", "geelong", """{"_key":"geelong","name":"Geelong","people":289000}""")

            db.link("city", "melbourne", "near", "city", "geelong")
            assertEquals(1L, db.scanCountEdges())

            val out = db.neighbours("city", "melbourne", "near", Direction.Outgoing)
            assertContains(out, "\"collection\"")
            assertContains(out, "geelong")

            val incoming = db.neighbours("city", "melbourne", "near", Direction.Incoming)
            assertFalse(incoming.contains("geelong"), "the edge leaves melbourne: $incoming")

            db.linkWith("city", "geelong", "road", "city", "melbourne", """{"km":75}""")
            assertEquals(2L, db.scanCountEdges())
            assertContains(db.neighbours("city", "geelong", null, Direction.Both), "melbourne")

            assertTrue(db.unlink("city", "melbourne", "near", "city", "geelong"))
            assertFalse(db.unlink("city", "melbourne", "near", "city", "geelong"))
            assertEquals(1L, db.scanCountEdges())
        }
    }

    @Test
    fun `a missing endpoint is refused as an unknown row rather than a dangling identity`() {
        openCities("endpoint").use { db ->
            db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":1}""")
            val e = assertFailsWith<SekejapException> {
                db.link("city", "melbourne", "near", "city", "nowhere")
            }
            assertEquals(Status.UnknownRow, e.status, "message: ${e.message}")
            assertEquals(0L, db.scanCountEdges())
        }
    }

    @Test
    fun `a committed transaction keeps every write and a rolled back one keeps none`() {
        openCities("transaction").use { db ->
            db.transaction { tx ->
                tx.put("city", "a", """{"_key":"a","name":"A","people":1}""")
                tx.put("city", "b", """{"_key":"b","name":"B","people":2}""")
                tx.link("city", "a", "near", "city", "b")
            }
            assertEquals(2L, db.countRows("city"))
            assertEquals(1L, db.scanCountEdges())

            db.transaction().use { tx ->
                tx.put("city", "c", """{"_key":"c","name":"C","people":3}""")
                assertEquals(1L, tx.execute("UPDATE city SET people = \$1 WHERE _key = 'a'", "[42]"))
                tx.rollback()
            }
            assertEquals(2L, db.countRows("city"), "the rolled-back row is not there")
            assertFalse(db.exists("city", "c"))
            assertContains(
                db.query("SELECT people FROM city WHERE _key = 'a'"),
                "1",
            )

            assertFailsWith<SekejapException> {
                db.transaction { tx ->
                    tx.put("city", "d", """{"_key":"d","name":"D","people":4}""")
                    throw SekejapException("the body threw", Status.Unknown)
                }
            }
            assertFalse(db.exists("city", "d"), "a body that throws rolls the transaction back")

            db.transaction { tx -> assertTrue(tx.delete("city", "b")) }
            assertEquals(1L, db.countRows("city"))
        }
    }

    @Test
    fun `a failing call surfaces the message and the code the library reported`() {
        openCities("errors").use { db ->
            val syntax = assertFailsWith<SekejapException> { db.query("SELECT bad syntax FROM") }
            assertEquals(Status.Invalid, syntax.status, "message: ${syntax.message}")
            assertTrue(syntax.message!!.isNotEmpty(), "last_error carried a sentence")

            val unknownCollection = assertFailsWith<SekejapException> {
                db.put("nowhere", "k", """{"_key":"k"}""")
            }
            assertEquals(Status.Invalid, unknownCollection.status, "message: ${unknownCollection.message}")

            // The closed enumeration is total over the codes the ABI defines, and
            // anything outside it lands on Unknown rather than on nothing.
            val codes = (0..8).map { Status.of(it) }
            assertEquals(Status.entries.toList(), codes, "one status per code, in order")
            assertEquals(Status.Unknown, Status.of(99))

            // A success clears the slot, so the next miss is readable as a miss.
            assertNull(db.get("city", "nowhere"))
        }
    }

    @Test
    fun `the four calls with no atomic underneath are refused by name`() {
        openCities("refusals").use { db ->
            assertEquals(Status.Refused, assertFailsWith<SekejapException> { db.compact() }.status)
            assertEquals(Status.Refused, assertFailsWith<SekejapException> { db.trimMemory() }.status)
            assertEquals(
                Status.Refused,
                assertFailsWith<SekejapException> { db.show("SHOW TABLES") }.status,
            )
            assertEquals(Status.Refused, assertFailsWith<SekejapException> { Db.openMemory() }.status)
        }
    }

    @Test
    fun `a service call on a single-mode handle is refused and answers on a service handle`() {
        openCities("single-mode").use { db ->
            assertEquals(
                Status.Refused,
                assertFailsWith<SekejapException> { db.subscribe() }.status,
            )
        }

        Db.openService(tempDir("service")).use { db ->
            db.createCollection("city", """[{"name":"name","kind":"text"}]""")
            db.statementTimeoutMs(5_000)

            val subscription = db.subscribe()
            db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne"}""")
            db.publish()

            val event = db.nextChange(subscription, timeoutMs = 2_000)
            assertNotNull(event, "the commit reached the feed")
            assertContains(event, "\"sequence\":1")
            assertContains(event, "\"key\":\"melbourne\"")
            assertContains(event, "\"kind\":\"put\"")
            assertContains(event, "\"keys_truncated\":false")
            // `collections` and a key's `collection` arrive as the catalog's
            // numeric id, not as the name the caller declared. Reported as a gap.
            assertContains(event, "\"collections\":[")

            assertTrue(db.unsubscribe(subscription))
            assertFalse(db.unsubscribe(subscription))

            db.cancel()
            assertTrue(db.clearInterrupt(), "a cancel was standing")
            assertFalse(db.clearInterrupt(), "and now none is")

            db.statementTimeoutMs(0)
        }
    }

    @Test
    fun `the write-ahead log folds into the data file and the bytes on disk are reported`() {
        openCities("storage").use { db ->
            db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":1}""")
            db.checkpoint() // 1 folded, 0 deferred: both are a success
            val storage = db.storage()
            assertContains(storage, "data_bytes")
            assertContains(storage, "wal_bytes")
            assertContains(storage, "total_bytes")
        }
    }

    @Test
    fun `closing the database frees the statements and walks still open on it`() {
        val db = openCities("close")
        db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":1}""")
        val stmt = db.prepare("SELECT _key FROM city")
        val scan = db.scan("city")
        val tx = db.transaction()
        tx.put("city", "geelong", """{"_key":"geelong","name":"Geelong","people":2}""")
        db.close() // frees the transaction (rolling it back), the walk and the statement first

        assertFailsWith<IllegalStateException> { stmt.query() }
        assertFailsWith<IllegalStateException> { scan.next() }
        assertFailsWith<IllegalStateException> { tx.put("city", "x", """{"_key":"x"}""") }
        db.close() // a second close is a no-op
    }

    @Test
    fun `a database opened under a store configuration reads back what was written`() {
        val dir = tempDir("config")
        Db.openWithConfig(dir, """{"budget_bytes": 16777216, "sync": "full"}""").use { db ->
            db.createCollection("city", """[{"name":"name","kind":"text"}]""")
            db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne"}""")
        }
        Db.open(dir).use { db ->
            assertContains(assertNotNull(db.get("city", "melbourne")), "Melbourne")
        }
    }
}
