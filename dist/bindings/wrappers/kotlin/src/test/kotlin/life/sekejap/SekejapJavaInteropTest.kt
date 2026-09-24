package life.sekejap

import java.nio.file.Files
import kotlin.test.Test
import kotlin.test.assertContains
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * One artifact for two languages: [JavaTour] is JAVA source calling the same
 * public API, and this test runs it. If a Kotlin-only construct leaked into the
 * surface -- a default argument with no overload, a value class, a nullable type
 * with no annotation -- `JavaTour` would not compile and this test would not
 * exist to run.
 */
class SekejapJavaInteropTest {

    @Test
    fun `the whole API answers the same way when it is called from Java source`() {
        val dir = Files.createTempDirectory("sekejap-kotlin-java-interop").toString()
        val log = JavaTour.run(dir)
        for (line in log) println("java: $line")

        assertContains(log, "version=0.17.3")
        assertContains(log, "format=2")
        assertContains(log, "get=true")
        assertContains(log, "exists=true")
        assertContains(log, "pages=2")
        assertContains(log, "rebindable-unbound=Unbound")
        assertContains(log, "rebindable-bound=Yes")
        assertContains(log, "edges=1")
        assertContains(log, "rows-after-commit=3")
        assertContains(log, "rows-after-rollback=3")
        assertContains(log, "error=Invalid:true")
        assertContains(log, "closed")

        val parameterised = log.single { it.startsWith("param=") }
        assertEquals(
            listOf("melbourne"),
            listOf("melbourne", "geelong").filter { parameterised.contains(it) },
            "only the row above the bound is in the parameterised answer: $parameterised",
        )
        assertTrue(
            log.single { it.startsWith("neighbours=") }.contains("geelong"),
            "the one-hop answer names the neighbour",
        )
    }
}
