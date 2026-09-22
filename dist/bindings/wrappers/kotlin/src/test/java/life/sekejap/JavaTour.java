package life.sekejap;

import java.util.ArrayList;
import java.util.List;

/**
 * The same API, exercised from JAVA source rather than from Kotlin, so that
 * "one artifact, usable from Java and Kotlin" is a compiled fact and not a
 * claim. {@link SekejapJavaInteropTest} runs it and checks what it reports.
 *
 * <p>Everything here is plain Java: {@code Db} is an {@code AutoCloseable} in a
 * try-with-resources, the overloads that Kotlin spells with default arguments
 * are reachable because they carry {@code @JvmOverloads}, and a failure arrives
 * as an unchecked {@link SekejapException} carrying a {@link Status}.
 */
public final class JavaTour {
    private JavaTour() {}

    /** Run the tour against a fresh database in {@code dir}; returns one line per step. */
    public static List<String> run(String dir) {
        List<String> log = new ArrayList<>();
        log.add("version=" + Db.version());
        log.add("format=" + Db.formatVersion());

        try (Db db = Db.open(dir)) {
            db.createCollection("city", "[{\"name\":\"name\",\"kind\":\"text\"},"
                + "{\"name\":\"people\",\"kind\":\"int\"}]");
            db.execute("CREATE INDEX city_people ON city USING btree (people)");

            db.put("city", "melbourne",
                "{\"_key\":\"melbourne\",\"name\":\"Melbourne\",\"people\":5207145}");
            db.put("city", "geelong",
                "{\"_key\":\"geelong\",\"name\":\"Geelong\",\"people\":289000}");
            log.add("get=" + (db.get("city", "melbourne") != null));
            log.add("exists=" + db.exists("city", "geelong"));

            // @JvmOverloads: the no-parameter overload and the $1 one are both here.
            log.add("query=" + db.query("SELECT _key FROM city"));
            log.add("param=" + db.query("SELECT _key FROM city WHERE people > $1", "[1000000]"));

            try (Scan scan = db.scan("city", 1)) {
                int pages = 0;
                while (scan.next() != null) pages++;
                log.add("pages=" + pages);
            }

            try (Statement stmt = db.prepare("SELECT _key FROM city WHERE people > $1")) {
                log.add("rebindable-unbound=" + stmt.rebindable());
                stmt.query("[1000000]");
                stmt.query("[1000]");
                log.add("rebindable-bound=" + stmt.rebindable());
            }

            db.link("city", "melbourne", "near", "city", "geelong");
            log.add("neighbours=" + db.neighbours("city", "melbourne", "near", Direction.Outgoing, 256));
            log.add("edges=" + db.scanCountEdges());

            try (Tx tx = db.transaction()) {
                tx.put("city", "ballarat", "{\"_key\":\"ballarat\",\"name\":\"Ballarat\",\"people\":116000}");
                tx.commit();
            }
            log.add("rows-after-commit=" + db.countRows("city"));

            try (Tx tx = db.transaction()) {
                tx.put("city", "mildura", "{\"_key\":\"mildura\",\"name\":\"Mildura\",\"people\":33000}");
                // Leaving the block without a commit rolls back.
            }
            log.add("rows-after-rollback=" + db.countRows("city"));

            try {
                db.query("SELECT bad syntax FROM");
                log.add("error=none");
            } catch (SekejapException e) {
                log.add("error=" + e.getStatus() + ":" + (e.getMessage() != null));
            }
        }
        log.add("closed");
        return log;
    }
}
