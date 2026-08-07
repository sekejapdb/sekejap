package life.sekejap.bench.adapters

import android.content.Context
import app.cash.sqldelight.db.SqlDriver
import app.cash.sqldelight.driver.android.AndroidSqliteDriver
import life.sekejap.bench.DocData
import life.sekejap.bench.EngineBench
import life.sekejap.bench.sqld.BenchDb

class SqlDelightAdapter(private val ctx: Context) : EngineBench {
    override val name = "sqldelight"
    private val dbName = "sqld.db"
    private var driver: SqlDriver? = null
    private var db: BenchDb? = null
    private var wiped = false

    override fun open(dir: String) {
        if (!wiped) { ctx.deleteDatabase(dbName); wiped = true } // fresh per run, not on reopen
        driver = AndroidSqliteDriver(BenchDb.Schema, ctx, dbName)
        db = BenchDb(driver!!)
    }

    override fun insertAll(docs: List<DocData>) {
        db!!.docQueries.transaction {
            for (d in docs) db!!.docQueries.insert(d.id, d.name, d.category, d.value, d.ts)
        }
    }

    override fun getById(id: String): String? =
        db!!.docQueries.getName(id).executeAsOneOrNull()

    override fun queryCategory(cat: String): Int =
        db!!.docQueries.countCategory(cat).executeAsOne().toInt()

    override fun queryRange(lo: Double, hi: Double): Int =
        db!!.docQueries.countRange(lo, hi).executeAsOne().toInt()

    override fun update(id: String, newValue: Double) =
        db!!.docQueries.updateValue(newValue, id)

    override fun deleteById(id: String) =
        db!!.docQueries.deleteById(id)

    override fun close() { driver?.close(); driver = null; db = null }

    override fun dbSizeBytes(dir: String): Long = ctx.getDatabasePath(dbName).length()
}
