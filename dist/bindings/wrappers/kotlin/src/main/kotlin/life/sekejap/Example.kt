package life.sekejap

import java.nio.file.Files

/**
 * The whole wrapper in one pass, end to end: open, declare a collection, write,
 * read, query with a parameter, walk, prepare and rebind, link and ask for
 * neighbours, commit one transaction and roll another back, count rows, take an
 * error, close.
 *
 * Run it with `./gradlew run -Psekejap.nativeDir=<directory holding libsekejap>`.
 */
fun main() {
    val dir = Files.createTempDirectory("sekejap-kotlin-example")
    println("sekejap ${Db.version()}, disk format ${Db.formatVersion()}")
    println("database: $dir")

    Db.open(dir.toString()).use { db ->
        // The catalog: a declaration is a floor, not a fence.
        db.createCollection(
            "city",
            """[{"name":"name","kind":"text"},{"name":"people","kind":"int"}]""",
        )
        // QL_CONTRACT section 6: a Tier-1 predicate is answered index-side, so a
        // WHERE names an index or is refused by name rather than scanned.
        db.execute("CREATE INDEX city_people ON city USING btree (people)")
        println("collections: ${db.collections()}")

        // Documents, one commit per call.
        db.put("city", "melbourne", """{"_key":"melbourne","name":"Melbourne","people":5207145}""")
        db.put("city", "geelong", """{"_key":"geelong","name":"Geelong","people":289000}""")
        println("get melbourne: ${db.get("city", "melbourne")}")
        println("get nowhere:   ${db.get("city", "nowhere")}")

        // SQL with a $n parameter.
        println("query:  ${db.query("SELECT name FROM city WHERE people > \$1", "[1000000]")}")

        // A walk of the collection, one page at a time.
        db.scan("city", pageRows = 1).use { scan ->
            scan.pages().forEachIndexed { i, page -> println("page ${i + 1}: $page") }
        }

        // One parse, many binds.
        db.prepare("SELECT name FROM city WHERE people > \$1").use { stmt ->
            println("rebindable before a bind: ${stmt.rebindable()}")
            println("bind 1: ${stmt.query("[1000000]")}")
            println("bind 2: ${stmt.query("[1000]")}")
            println("rebindable after a bind:  ${stmt.rebindable()}")
        }

        // Edges, and the rows one hop away.
        db.link("city", "melbourne", "near", "city", "geelong")
        println("neighbours: ${db.neighbours("city", "melbourne", "near", Direction.Outgoing)}")
        println("edges: ${db.scanCountEdges()}")

        // Many writes, one barrier -- committed.
        db.transaction { tx ->
            tx.put("city", "ballarat", """{"_key":"ballarat","name":"Ballarat","people":116000}""")
            tx.put("city", "bendigo", """{"_key":"bendigo","name":"Bendigo","people":103000}""")
        }
        println("rows after the commit:   ${db.countRows("city")}")

        // The same, rolled back.
        db.transaction().use { tx ->
            tx.put("city", "mildura", """{"_key":"mildura","name":"Mildura","people":33000}""")
            tx.rollback()
        }
        println("rows after the rollback: ${db.countRows("city")}")
        println("rows by walking them:    ${db.scanCountRows("city")}")

        // An error path: the message and the code the ABI reported.
        try {
            db.query("SELECT bad syntax FROM")
        } catch (e: SekejapException) {
            println("refused [${e.status}]: ${e.message}")
        }

        // A refusal that keeps its symbol, so it arrives with a reason.
        try {
            db.compact()
        } catch (e: SekejapException) {
            println("refused [${e.status}]: ${e.message}")
        }

        println("storage: ${db.storage()}")
    }
    println("closed")
}
