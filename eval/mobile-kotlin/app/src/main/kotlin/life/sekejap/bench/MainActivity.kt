package life.sekejap.bench

import android.graphics.Typeface
import android.os.Bundle
import android.util.Log
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import life.sekejap.bench.adapters.ObjectBoxAdapter
import life.sekejap.bench.adapters.RealmAdapter
import life.sekejap.bench.adapters.RoomAdapter
import life.sekejap.bench.adapters.SekejapAdapter
import life.sekejap.bench.adapters.SekejapErgonomicAdapter
import life.sekejap.bench.adapters.SqlDelightAdapter
import org.json.JSONObject

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val tv = TextView(this).apply {
            setTextIsSelectable(true)
            textSize = 11f
            typeface = Typeface.MONOSPACE
            setPadding(24, 24, 24, 24)
        }
        setContentView(ScrollView(this).apply { addView(tv) })

        Thread {
            runAll { line ->
                Log.i("BENCH", line)
                runOnUiThread { tv.append(line + "\n") }
            }
        }.start()
    }

    private fun runAll(log: (String) -> Unit) {
        val base = filesDir.absolutePath
        val docs = makeDocs(10_000)
        val engines = listOf(
            SekejapAdapter(),          // sekejap-raw (string SGQL)
            SekejapErgonomicAdapter(), // sekejap-ergo (typed layer)
            RoomAdapter(this),
            ObjectBoxAdapter(this),
            RealmAdapter(),
            SqlDelightAdapter(this),
        )
        val results = LinkedHashMap<String, List<PhaseResult>>()
        for (e in engines) {
            log("── ${e.name} ──")
            try {
                results[e.name] = runEngine(e, base, docs, log)
            } catch (t: Throwable) {
                log("  ${e.name} FAILED: $t")
            }
        }
        val obj = JSONObject()
        for ((k, v) in results) {
            val p = JSONObject()
            for (r in v) p.put(r.phase, r.ms)
            obj.put(k, p)
        }
        Log.i("BENCH-JSON", obj.toString())
        log("done — JSON in logcat (tag BENCH-JSON)")
    }
}
