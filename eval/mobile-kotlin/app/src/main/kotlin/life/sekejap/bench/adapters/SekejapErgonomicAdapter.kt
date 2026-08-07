package life.sekejap.bench.adapters

import life.sekejap.Sekejap
import life.sekejap.bench.DocData
import life.sekejap.bench.EngineBench
import life.sekejap.bench.ErgoDoc
import life.sekejap.bench.dirSize
import life.sekejap.bench.docs
import life.sekejap.bench.ergoDocSchema

/** sekejap via the ERGONOMIC typed layer (KSP-generated `db.docs`, typed builder). */
class SekejapErgonomicAdapter : EngineBench {
    override val name = "sekejap-ergo"
    private lateinit var db: Sekejap

    override fun open(dir: String) {
        db = Sekejap.open(dir, schema = listOf(ergoDocSchema), mobileProfile = true)
    }

    override fun insertAll(docs: List<DocData>) {
        db.docs.putAll(docs.map { ErgoDoc(it.id, it.name, it.category, it.value, it.ts) })
    }

    override fun getById(id: String): String? = db.docs.get(id)?.name

    override fun queryCategory(cat: String): Int =
        db.docs.where { it.category eq cat }.count()

    override fun queryRange(lo: Double, hi: Double): Int =
        db.docs.where { (it.value gte lo) and (it.value lte hi) }.count()

    override fun update(id: String, newValue: Double) {
        db.docs.where { it.id eq id }.update(mapOf("value" to newValue))
    }

    override fun deleteById(id: String) {
        db.docs.where { it.id eq id }.deleteAll()
    }

    override fun close() { db.compact(); db.close() }
    override fun dbSizeBytes(dir: String): Long = dirSize(dir)
}
