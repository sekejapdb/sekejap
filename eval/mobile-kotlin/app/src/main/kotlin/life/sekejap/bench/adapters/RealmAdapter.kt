package life.sekejap.bench.adapters

import io.realm.kotlin.Realm
import io.realm.kotlin.RealmConfiguration
import io.realm.kotlin.UpdatePolicy
import io.realm.kotlin.ext.query
import io.realm.kotlin.types.RealmObject
import io.realm.kotlin.types.annotations.PrimaryKey
import life.sekejap.bench.DocData
import life.sekejap.bench.EngineBench
import life.sekejap.bench.dirSize

class DocR : RealmObject {
    @PrimaryKey var id: String = ""
    var name: String = ""
    var category: String = ""
    var value: Double = 0.0
    var ts: Long = 0
}

class RealmAdapter : EngineBench {
    override val name = "realm"
    private var realm: Realm? = null

    override fun open(dir: String) {
        val cfg = RealmConfiguration.Builder(schema = setOf(DocR::class))
            .directory(dir)
            .name("realm.realm")
            .build()
        realm = Realm.open(cfg)
    }

    override fun insertAll(docs: List<DocData>) {
        realm!!.writeBlocking {
            for (d in docs) {
                copyToRealm(DocR().apply {
                    id = d.id; name = d.name; category = d.category
                    value = d.value; ts = d.ts
                }, UpdatePolicy.ALL)
            }
        }
    }

    override fun getById(id: String): String? =
        realm!!.query<DocR>("id == $0", id).first().find()?.name

    override fun queryCategory(cat: String): Int =
        realm!!.query<DocR>("category == $0", cat).count().find().toInt()

    override fun queryRange(lo: Double, hi: Double): Int =
        realm!!.query<DocR>("value >= $0 AND value <= $1", lo, hi).count().find().toInt()

    override fun update(id: String, newValue: Double) {
        realm!!.writeBlocking {
            query<DocR>("id == $0", id).first().find()?.let { it.value = newValue }
        }
    }

    override fun deleteById(id: String) {
        realm!!.writeBlocking {
            query<DocR>("id == $0", id).first().find()?.let { delete(it) }
        }
    }

    override fun close() { realm?.close(); realm = null }
    override fun dbSizeBytes(dir: String): Long = dirSize(dir)
}
