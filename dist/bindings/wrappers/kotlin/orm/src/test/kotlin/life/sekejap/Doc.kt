package life.sekejap

@SekejapEntity
data class Doc(
    @Key val id: String,
    val name: String,
    @Index(IndexKind.HASH) val category: String,
    @Index val value: Double, // btree
    val ts: Long,
)
