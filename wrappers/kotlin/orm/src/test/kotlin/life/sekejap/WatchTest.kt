package life.sekejap

import java.nio.file.Files
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking

class WatchTest {
    private lateinit var db: Sekejap

    private fun open(): Sekejap {
        val dir = Files.createTempDirectory("sk_watch").toString()
        return Sekejap.open(dir, schema = listOf(docSchema)).also { db = it }
    }

    @AfterTest fun tearDown() { db.close() }

    @Test fun reactiveWatchReEmitsOnChange() = runBlocking {
        val db = open()
        db.docs.put(Doc("a", "n", "main", 1.0, 0L))

        val snapshots = mutableListOf<List<Doc>>()
        val job = launch(Dispatchers.IO) {
            db.docs.where { it.category eq "main" }.watch().collect { snapshots.add(it) }
        }

        delay(250) // initial snapshot
        assertEquals(1, snapshots.size)
        assertEquals(setOf("a"), snapshots.last().map { it.id }.toSet())

        db.docs.put(Doc("b", "n", "main", 2.0, 0L)) // matching change → re-emit
        delay(250)

        job.cancel() // triggers watchClose → worker exits cleanly

        assertTrue(snapshots.size >= 2, "expected a re-emit; got ${snapshots.size}")
        assertEquals(setOf("a", "b"), snapshots.last().map { it.id }.toSet())
    }
}
