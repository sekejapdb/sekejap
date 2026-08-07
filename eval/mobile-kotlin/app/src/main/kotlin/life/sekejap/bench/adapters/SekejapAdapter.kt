package life.sekejap.bench.adapters

import life.sekejap.SekejapNative
import life.sekejap.bench.DocData
import life.sekejap.bench.EngineBench
import life.sekejap.bench.dirSize
import org.json.JSONArray
import org.json.JSONObject

/** sekejap via the RAW JNI surface (string SGQL + JSON). */
class SekejapAdapter : EngineBench {
    override val name = "sekejap-raw"
    private var h: Long = 0

    override fun open(dir: String) {
        h = SekejapNative.open(dir)
        SekejapNative.mobileProfile(h)
        try {
            SekejapNative.execute(h,
                "CREATE TABLE docs (_key TEXT PRIMARY KEY, name TEXT, category TEXT, value REAL, ts INTEGER)")
            SekejapNative.execute(h, "CREATE INDEX ON docs USING hash (category)")
            SekejapNative.execute(h, "CREATE INDEX ON docs USING btree (value)")
        } catch (_: Throwable) { /* exists on reopen */ }
    }

    override fun insertAll(docs: List<DocData>) {
        // One JNI crossing: JSON array of [slug, payloadObject] pairs, built with
        // a StringBuilder (the synthetic data has no chars needing escaping).
        val sb = StringBuilder(docs.size * 96)
        sb.append('[')
        for (i in docs.indices) {
            val d = docs[i]
            if (i > 0) sb.append(',')
            sb.append("[\"docs/").append(d.id).append("\",{")
                .append("\"_collection\":\"docs\",\"_key\":\"").append(d.id)
                .append("\",\"name\":\"").append(d.name)
                .append("\",\"category\":\"").append(d.category)
                .append("\",\"value\":").append(d.value)
                .append(",\"ts\":").append(d.ts)
                .append("}]")
        }
        sb.append(']')
        SekejapNative.putMany(h, sb.toString())
    }

    override fun getById(id: String): String? {
        val s = SekejapNative.get(h, "docs/$id") ?: return null
        return JSONObject(s).optString("name")
    }

    override fun queryCategory(cat: String): Int {
        val rows = JSONArray(SekejapNative.queryParams(h,
            "SELECT COUNT(*) AS n FROM docs WHERE category = \$1",
            JSONArray().put(cat).toString()))
        return rows.getJSONObject(0).getJSONObject("payload").getInt("n")
    }

    override fun queryRange(lo: Double, hi: Double): Int {
        val rows = JSONArray(SekejapNative.queryParams(h,
            "SELECT COUNT(*) AS n FROM docs WHERE value >= \$1 AND value <= \$2",
            JSONArray().put(lo).put(hi).toString()))
        return rows.getJSONObject(0).getJSONObject("payload").getInt("n")
    }

    override fun update(id: String, newValue: Double) {
        SekejapNative.executeParams(h,
            "UPDATE docs SET value = \$1 WHERE _key = \$2",
            JSONArray().put(newValue).put(id).toString())
    }

    override fun deleteById(id: String) {
        SekejapNative.executeParams(h,
            "DELETE FROM docs WHERE _key = \$1",
            JSONArray().put(id).toString())
    }

    override fun close() {
        if (h != 0L) { SekejapNative.compact(h); SekejapNative.close(h); h = 0 }
    }

    override fun dbSizeBytes(dir: String): Long = dirSize(dir)
}
