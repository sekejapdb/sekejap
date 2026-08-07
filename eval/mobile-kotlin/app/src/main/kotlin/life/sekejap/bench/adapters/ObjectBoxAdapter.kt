package life.sekejap.bench.adapters

import android.content.Context
import io.objectbox.Box
import io.objectbox.BoxStore
import io.objectbox.annotation.Entity
import io.objectbox.annotation.Id
import io.objectbox.annotation.Index
import life.sekejap.bench.DocData
import life.sekejap.bench.EngineBench
import life.sekejap.bench.dirSize
import java.io.File

@Entity
data class DocOb(
    @Id var obId: Long = 0,
    @Index var docId: String = "",
    var name: String = "",
    @Index var category: String = "",
    var value: Double = 0.0,
    var ts: Long = 0,
)

class ObjectBoxAdapter(private val ctx: Context) : EngineBench {
    override val name = "objectbox"
    private var store: BoxStore? = null
    private var box: Box<DocOb>? = null

    override fun open(dir: String) {
        store = MyObjectBox.builder()
            .androidContext(ctx.applicationContext)
            .directory(File(dir, "obx"))
            .build()
        box = store!!.boxFor(DocOb::class.java)
    }

    override fun insertAll(docs: List<DocData>) {
        box!!.put(docs.map { DocOb(0, it.id, it.name, it.category, it.value, it.ts) })
    }

    override fun getById(id: String): String? =
        box!!.query(DocOb_.docId.equal(id)).build().use { it.findFirst()?.name }

    override fun queryCategory(cat: String): Int =
        box!!.query(DocOb_.category.equal(cat)).build().use { it.count().toInt() }

    override fun queryRange(lo: Double, hi: Double): Int =
        box!!.query(DocOb_.value.between(lo, hi)).build().use { it.count().toInt() }

    override fun update(id: String, newValue: Double) {
        box!!.query(DocOb_.docId.equal(id)).build().use { q ->
            q.findFirst()?.let { it.value = newValue; box!!.put(it) }
        }
    }

    override fun deleteById(id: String) {
        box!!.query(DocOb_.docId.equal(id)).build().use { q ->
            q.findFirst()?.let { box!!.remove(it) }
        }
    }

    override fun close() { store?.close(); store = null; box = null }
    override fun dbSizeBytes(dir: String): Long = dirSize(dir)
}
