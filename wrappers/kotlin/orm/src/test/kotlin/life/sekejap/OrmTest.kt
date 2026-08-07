package life.sekejap

import java.nio.file.Files
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

class OrmTest {
    private lateinit var db: Sekejap

    private fun open(): Sekejap {
        val dir = Files.createTempDirectory("sk_orm").toString()
        return Sekejap.open(dir, schema = listOf(docSchema)).also { db = it }
    }

    @AfterTest fun tearDown() { db.close() }

    @Test fun typedPutGetRoundTrips() {
        val db = open()
        db.docs.put(Doc("d1", "Nasi", "main", 45000.0, 1L))
        val got = db.docs.get("d1")!!
        assertEquals("d1", got.id)
        assertEquals("main", got.category)
        assertEquals(45000.0, got.value)
    }

    @Test fun typedWhereSortLimitLowersToSgql() {
        val db = open()
        db.docs.putAll((0 until 100).map {
            Doc("k$it", "name-$it", "cat${it % 10}", it.toDouble(), 1_700_000_000L + it)
        })

        val cheapMains = db.docs
            .where { it.category eq "cat3" and (it.value lt 50.0) }
            .sortBy { it.value }
            .find()
        assertTrue(cheapMains.all { it.category == "cat3" && it.value < 50.0 })
        assertTrue(cheapMains.isNotEmpty())

        // Range uses the btree index (flat AND, no wrapping parens).
        val n = db.docs.where { it.value.between(10.0, 20.0) }.count()
        assertEquals(11, n) // 10.0 .. 20.0 inclusive
    }

    @Test fun typedUpdateAndDelete() {
        val db = open()
        db.docs.put(Doc("x", "n", "c", 1.0, 0L))
        db.docs.where { it.id eq "x" }.update(mapOf("value" to 999.0))
        assertEquals(999.0, db.docs.get("x")!!.value)
        db.docs.where { it.id eq "x" }.deleteAll()
        assertNull(db.docs.get("x"))
    }
}
