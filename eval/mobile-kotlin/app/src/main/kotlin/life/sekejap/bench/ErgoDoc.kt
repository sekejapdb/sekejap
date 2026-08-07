package life.sekejap.bench

import life.sekejap.Index
import life.sekejap.IndexKind
import life.sekejap.Key
import life.sekejap.SekejapEntity

// The ergonomic-mode entity. KSP generates ErgoDocCollection + `Sekejap.docs`.
@Suppress("unused")
@SekejapEntity(collection = "docs")
data class ErgoDoc(
    @Key val id: String,
    val name: String,
    @Index(IndexKind.HASH) val category: String,
    @Index val value: Double, // btree
    val ts: Long,
)
