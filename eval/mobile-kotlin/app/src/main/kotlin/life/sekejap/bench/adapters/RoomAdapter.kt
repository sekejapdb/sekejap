package life.sekejap.bench.adapters

import android.content.Context
import androidx.room.Dao
import androidx.room.Database
import androidx.room.Entity
import androidx.room.Index
import androidx.room.Insert
import androidx.room.OnConflictStrategy
import androidx.room.PrimaryKey
import androidx.room.Query
import androidx.room.Room
import androidx.room.RoomDatabase
import life.sekejap.bench.DocData
import life.sekejap.bench.EngineBench
import life.sekejap.bench.dirSize

@Entity(tableName = "docs", indices = [Index("category"), Index("value")])
data class DocEntity(
    @PrimaryKey val id: String,
    val name: String,
    val category: String,
    val value: Double,
    val ts: Long,
)

@Dao
interface DocDao {
    @Insert(onConflict = OnConflictStrategy.REPLACE)
    fun insertAll(docs: List<DocEntity>)

    @Query("SELECT name FROM docs WHERE id = :id")
    fun getName(id: String): String?

    @Query("SELECT COUNT(*) FROM docs WHERE category = :cat")
    fun countCategory(cat: String): Int

    @Query("SELECT COUNT(*) FROM docs WHERE value >= :lo AND value <= :hi")
    fun countRange(lo: Double, hi: Double): Int

    @Query("UPDATE docs SET value = :v WHERE id = :id")
    fun updateValue(id: String, v: Double)

    @Query("DELETE FROM docs WHERE id = :id")
    fun deleteById(id: String)
}

@Database(entities = [DocEntity::class], version = 1)
abstract class AppDb : RoomDatabase() {
    abstract fun dao(): DocDao
}

class RoomAdapter(private val ctx: Context) : EngineBench {
    override val name = "room"
    private var db: AppDb? = null

    override fun open(dir: String) {
        db = Room.databaseBuilder(ctx, AppDb::class.java, "$dir/room.db")
            .allowMainThreadQueries()
            .build()
    }

    override fun insertAll(docs: List<DocData>) {
        db!!.dao().insertAll(docs.map { DocEntity(it.id, it.name, it.category, it.value, it.ts) })
    }

    override fun getById(id: String): String? = db!!.dao().getName(id)
    override fun queryCategory(cat: String): Int = db!!.dao().countCategory(cat)
    override fun queryRange(lo: Double, hi: Double): Int = db!!.dao().countRange(lo, hi)
    override fun update(id: String, newValue: Double) = db!!.dao().updateValue(id, newValue)
    override fun deleteById(id: String) = db!!.dao().deleteById(id)

    override fun close() { db?.close(); db = null }
    override fun dbSizeBytes(dir: String): Long = dirSize(dir)
}
