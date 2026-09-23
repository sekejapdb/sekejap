//! Every query the README shows, run against a small Bali dataset whose
//! answers are known by construction, and each one CONTRASTED with a
//! neighbouring query that must answer differently.
//!
//! A query that merely runs proves little: a radius read in the wrong unit, an
//! edge walked the wrong way or a filter that is silently dropped all still
//! return rows. So every README query is asserted row for row, and beside it
//! sits the query one step away -- a smaller radius, the other direction, one
//! hop instead of two, OR instead of AND, another author -- whose different,
//! equally known answer is asserted too. `every_tested_query_is_written_in_the_readme`
//! keeps this file and the README from drifting: each README query here must
//! appear in README.md word for word, whitespace aside.
//!
//! The data (distances from the README's centre, 115.168 E 8.690 S):
//!
//! | restaurant    | area     | from the centre |
//! |---------------|----------|-----------------|
//! | warung-made   | Seminyak | about 0.9 km    |
//! | la-lucciola   | Seminyak | about 1.4 km    |
//! | nasi-uluwatu  | Uluwatu  | about 18 km     |
//! | bebek-tepi    | Ubud     | about 23 km     |

use sekejap::{Db, Direction, Rows};
use serde_json::{json, Value};

const README: &str = include_str!("../../../README.md");

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A statement the README shows. Asserting it appears there is what makes
/// this file a test OF the README rather than of statements like it.
fn readme(sql: &str) -> &str {
    assert!(
        squash(README).contains(&squash(sql)),
        "not in README.md, word for word:\n{sql}"
    );
    sql
}

fn open() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("bali")).unwrap();
    (dir, db)
}

fn run(db: &Db, sql: &str) {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("refused:\n{sql}\n{e}"));
}

fn query(db: &Db, sql: &str) -> Rows {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("refused:\n{sql}\n{e}"))
}

/// One column of every row, in the order the statement returned them.
fn column(rows: &Rows, name: &str) -> Vec<Value> {
    rows.iter()
        .map(|row| row.json(name).unwrap_or(Value::Null))
        .collect()
}

fn texts(rows: &Rows, name: &str) -> Vec<String> {
    column(rows, name)
        .into_iter()
        .map(|v| v.as_str().unwrap_or_else(|| panic!("{name} = {v}")).to_owned())
        .collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn refused(db: &Db, sql: &str, said: &str) {
    let error = match db.query(sql, &[]) {
        Err(e) => e.to_string(),
        Ok(rows) => panic!("accepted, answering {} rows:\n{sql}", rows.len()),
    };
    assert!(error.contains(said), "`{sql}` said `{error}`, not `{said}`");
}

/// The README's schema, indexes and rows, then this file's own data.
fn bali() -> (tempfile::TempDir, Db) {
    let (dir, db) = open();
    // Section 1, verbatim.
    run(&db, readme("CREATE TABLE tourists (
        _key      TEXT PRIMARY KEY,
        name      TEXT,
        home_city TEXT,
        arrival   TIMESTAMPTZ,
        taste     VECTOR(4)        -- an embedding of what this person likes
    )"));
    run(&db, readme("CREATE TABLE flights     (_key TEXT PRIMARY KEY, airline TEXT, duration_hours INT)"));
    run(&db, readme("CREATE TABLE restaurants (_key TEXT PRIMARY KEY, name TEXT, area TEXT, geometry GEOMETRY(Point,4326))"));
    run(&db, readme("CREATE TABLE dishes (
        _key TEXT PRIMARY KEY, name TEXT, price INT, protein_g INT,
        description TEXT, geometry GEOMETRY(Point,4326), open_now BOOLEAN, embedding VECTOR(4)
    )"));
    // Section 2, verbatim.
    run(&db, readme("CREATE INDEX ON dishes USING gin  (to_tsvector('simple', description))"));
    run(&db, readme("CREATE INDEX ON tourists USING quantized (taste vector_cosine_ops)"));
    run(&db, readme("CREATE INDEX ON dishes USING exact (embedding)"));
    // Section 3, verbatim, then the rest of the tourists.
    run(&db, readme("INSERT INTO tourists (_key, name, home_city, arrival) VALUES ('chloe', 'Chloe', 'Melbourne', '2024-06-01')"));
    run(&db, readme("INSERT INTO tourists (_key, name, home_city, arrival) VALUES ('aiym',  'Aiym',  'Almaty',    '2024-06-02')"));
    run(&db, readme("INSERT INTO flights (_key, airline, duration_hours) VALUES ('qf-mel', 'Qantas', 6)"));
    db.link(("tourists", "chloe"), "flew_on", ("flights", "qf-mel")).unwrap();

    // The tastes the README's vector queries rank. Chloe's is written by
    // UPDATE, the path a caller takes to add a field to an existing row.
    run(&db, "UPDATE tourists SET taste = '[0.9, 0.1, 0.0, 0.0]' WHERE _key = 'chloe'");
    run(&db, "UPDATE tourists SET taste = '[0.1, 0.9, 0.0, 0.0]' WHERE _key = 'aiym'");
    run(&db, "INSERT INTO tourists (_key, name, home_city, arrival, taste) VALUES ('budi', 'Budi', 'Melbourne', '2024-06-03', '[0.8, 0.2, 0.0, 0.0]')");
    run(&db, "INSERT INTO tourists (_key, name, home_city, arrival, taste) VALUES ('dewi', 'Dewi', 'Jakarta', '2024-06-04', '[0.0, 0.0, 1.0, 0.0]')");
    run(&db, "INSERT INTO flights (_key, airline, duration_hours) VALUES ('kc-ala', 'Air Astana', 9)");
    db.link(("tourists", "aiym"), "flew_on", ("flights", "kc-ala")).unwrap();
    db.link(("tourists", "budi"), "flew_on", ("flights", "qf-mel")).unwrap();
    // similar_taste: chloe -> budi -> dewi, and aiym -> chloe.
    db.link(("tourists", "chloe"), "similar_taste", ("tourists", "budi")).unwrap();
    db.link(("tourists", "budi"), "similar_taste", ("tourists", "dewi")).unwrap();
    db.link(("tourists", "aiym"), "similar_taste", ("tourists", "chloe")).unwrap();

    for (key, name, area, lon, lat) in [
        ("warung-made", "Warung Made", "Seminyak", 115.160, -8.690),
        ("la-lucciola", "La Lucciola", "Seminyak", 115.155, -8.692),
        ("nasi-uluwatu", "Nasi Uluwatu", "Uluwatu", 115.085, -8.829),
        ("bebek-tepi", "Bebek Tepi Sawah", "Ubud", 115.262, -8.506),
    ] {
        run(&db, &format!(
            r#"INSERT INTO restaurants (_key, name, area, geometry) VALUES ('{key}', '{name}', '{area}', '{{"type":"Point","coordinates":[{lon},{lat}]}}')"#
        ));
    }
    // Every dish but one fails exactly ONE condition of the combined query,
    // so a condition that stopped filtering lets exactly its dish through.
    for (key, name, price, protein, text, lon, lat, open, emb) in [
        ("ayam-bakar", "Ayam Bakar", 55000, 32, "grilled healthy chicken with sambal", 115.160, -8.690, true, "[0.7, 0.3, 0.0, 0.0]"),
        ("ikan-bakar", "Ikan Bakar", 75000, 28, "grilled healthy fish with lime", 115.155, -8.692, true, "[0.6, 0.4, 0.0, 0.0]"),
        // not "healthy"
        ("babi-guling", "Babi Guling", 85000, 30, "roast pork with grilled skin", 115.160, -8.690, true, "[0.2, 0.8, 0.0, 0.0]"),
        // 23 km away
        ("sate-lilit", "Sate Lilit", 45000, 26, "grilled healthy minced fish satay", 115.262, -8.506, true, "[0.7, 0.3, 0.0, 0.0]"),
        // too little protein
        ("gado-gado", "Gado Gado", 40000, 12, "grilled healthy vegetables in peanut sauce", 115.160, -8.690, true, "[0.9, 0.1, 0.0, 0.0]"),
        // closed
        ("tempe-bakar", "Tempe Bakar", 50000, 30, "grilled healthy tempeh", 115.160, -8.690, false, "[0.7, 0.3, 0.0, 0.0]"),
        // too dear
        ("lobster", "Lobster Bakar", 250000, 40, "grilled healthy lobster", 115.160, -8.690, true, "[0.7, 0.3, 0.0, 0.0]"),
    ] {
        run(&db, &format!(
            r#"INSERT INTO dishes (_key, name, price, protein_g, description, geometry, open_now, embedding) VALUES ('{key}', '{name}', {price}, {protein}, '{text}', '{{"type":"Point","coordinates":[{lon},{lat}]}}', {open}, '{emb}')"#
        ));
    }
    db.link(("restaurants", "warung-made"), "serves", ("dishes", "ayam-bakar")).unwrap();
    (dir, db)
}

#[test]
fn step_4_and_records_and_time() {
    let (_dir, db) = bali();
    let rows = query(&db, readme("SELECT name, home_city FROM tourists WHERE home_city = 'Melbourne'"));
    assert_eq!(sorted(texts(&rows, "name")), ["Budi", "Chloe"]);
    // Contrast: another value of the same column.
    let rows = query(&db, "SELECT name, home_city FROM tourists WHERE home_city = 'Almaty'");
    assert_eq!(texts(&rows, "name"), ["Aiym"]);

    let rows = query(&db, readme("SELECT area, COUNT(*) AS n
        FROM restaurants
        GROUP BY area
        ORDER BY n DESC"));
    assert_eq!(texts(&rows, "area")[0], "Seminyak");
    assert_eq!(column(&rows, "n")[0], json!(2));
    assert_eq!(sorted(texts(&rows, "area")), ["Seminyak", "Ubud", "Uluwatu"]);
    // Contrast: the other direction puts a one-row area first.
    let rows = query(&db, "SELECT area, COUNT(*) AS n FROM restaurants GROUP BY area ORDER BY n ASC");
    assert_eq!(column(&rows, "n")[0], json!(1));

    let rows = query(&db, readme("SELECT name, arrival
        FROM tourists WHERE _key = 'chloe'"));
    assert_eq!(texts(&rows, "arrival"), ["2024-06-01T00:00:00Z"]);
    // Contrast: a range on the same column, and the latest arrival first.
    let rows = query(&db, "SELECT name FROM tourists WHERE arrival >= '2024-06-02' ORDER BY arrival");
    assert_eq!(texts(&rows, "name"), ["Aiym", "Budi", "Dewi"]);
    let rows = query(&db, "SELECT name FROM tourists ORDER BY arrival DESC LIMIT 1");
    assert_eq!(texts(&rows, "name"), ["Dewi"]);
}

#[test]
fn graph_hops_directions_and_depths() {
    let (_dir, db) = bali();
    let rows = query(&db, readme("SELECT airline, hours
        FROM GRAPH_TABLE (base MATCH
            (t:tourists WHERE t._key = 'chloe')-[:flew_on]->(f:flights)
            COLUMNS (f.airline AS airline, f.duration_hours AS hours))"));
    assert_eq!(texts(&rows, "airline"), ["Qantas"]);
    assert_eq!(column(&rows, "hours"), [json!(6)]);
    // Contrast: the same edge walked BACKWARD from the flight.
    let rows = query(&db, "SELECT who FROM GRAPH_TABLE (base MATCH
        (f:flights WHERE f._key = 'qf-mel')<-[:flew_on]-(t:tourists)
        COLUMNS (t.name AS who))");
    assert_eq!(sorted(texts(&rows, "who")), ["Budi", "Chloe"]);

    let rows = query(&db, readme("SELECT tourist
        FROM GRAPH_TABLE (base MATCH
            (c:tourists WHERE c._key = 'chloe')-[:similar_taste]->{1,2}(t:tourists)
            COLUMNS (t.name AS tourist))"));
    assert_eq!(sorted(texts(&rows, "tourist")), ["Budi", "Dewi"]);
    // Contrast: one hop stops at Budi; backward finds who points at Chloe.
    let rows = query(&db, "SELECT tourist FROM GRAPH_TABLE (base MATCH
        (c:tourists WHERE c._key = 'chloe')-[:similar_taste]->(t:tourists)
        COLUMNS (t.name AS tourist))");
    assert_eq!(texts(&rows, "tourist"), ["Budi"]);
    let rows = query(&db, "SELECT tourist FROM GRAPH_TABLE (base MATCH
        (c:tourists WHERE c._key = 'chloe')<-[:similar_taste]-(t:tourists)
        COLUMNS (t.name AS tourist))");
    assert_eq!(texts(&rows, "tourist"), ["Aiym"]);

    // What a pattern may not do, refused by name rather than answered wrong.
    refused(&db, "SELECT who FROM GRAPH_TABLE (base MATCH
        (c:tourists WHERE c._key = 'chloe')-[:flew_on]->(f:flights)
        COLUMNS (c.name AS who))", "is the starting node");
    refused(&db, "SELECT a FROM GRAPH_TABLE (base MATCH
        (t:tourists)-[:flew_on]->(f:flights) COLUMNS (f.airline AS a))", "has no starting key");
}

#[test]
fn spatial_radius_in_metres_and_its_refused_degree_form() {
    let (_dir, db) = bali();
    let rows = query(&db, readme("SELECT name FROM restaurants
        WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0)"));
    assert_eq!(sorted(texts(&rows, "name")), ["La Lucciola", "Warung Made"]);
    // Contrast: 1 km keeps only the nearer one; 20 km adds Uluwatu, not Ubud.
    let rows = query(&db, "SELECT name FROM restaurants WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 1000.0)");
    assert_eq!(texts(&rows, "name"), ["Warung Made"]);
    let rows = query(&db, "SELECT name FROM restaurants WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 20000.0)");
    assert_eq!(sorted(texts(&rows, "name")), ["La Lucciola", "Nasi Uluwatu", "Warung Made"]);
    // Nearest first from the centre.
    let rows = query(&db, "SELECT name FROM restaurants ORDER BY geometry <-> ST_MakePoint(115.168, -8.690)::geography LIMIT 4");
    assert_eq!(texts(&rows, "name"), ["Warung Made", "La Lucciola", "Nasi Uluwatu", "Bebek Tepi Sawah"]);
    // The PostGIS-degrees form is refused, never answered in metres.
    refused(&db, "SELECT name FROM restaurants WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690), 5000.0)", "needs geography");
}

#[test]
fn vector_order_and_its_contrast() {
    let (_dir, db) = bali();
    let rows = query(&db, readme("SELECT name FROM tourists
        ORDER BY taste <=> '[0.9, 0.1, 0.0, 0.0]'
        LIMIT 5"));
    assert_eq!(texts(&rows, "name"), ["Chloe", "Budi", "Aiym", "Dewi"]);
    // Contrast: Aiym's own taste puts her first and Dewi, orthogonal, last.
    let rows = query(&db, "SELECT name FROM tourists ORDER BY taste <=> '[0.1, 0.9, 0.0, 0.0]' LIMIT 5");
    assert_eq!(texts(&rows, "name")[0], "Aiym");
    assert_eq!(texts(&rows, "name")[3], "Dewi");
}

#[test]
fn text_match_and_ranking() {
    let (_dir, db) = bali();
    let rows = query(&db, readme("SELECT name FROM dishes
        WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled & chicken')
        ORDER BY bm25(description, 'grilled chicken') DESC"));
    assert_eq!(texts(&rows, "name"), ["Ayam Bakar"]);
    // Contrast: OR takes every grilled dish; AND with another word, one other.
    let rows = query(&db, "SELECT name FROM dishes WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled | chicken')");
    assert_eq!(rows.len(), 7);
    let rows = query(&db, "SELECT name FROM dishes WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled & pork')");
    assert_eq!(texts(&rows, "name"), ["Babi Guling"]);
}

#[test]
fn the_combined_query_and_each_condition_it_depends_on() {
    let (_dir, db) = bali();
    let combined = readme("SELECT name, price
        FROM dishes
        WHERE open_now = true
          AND price BETWEEN 40000 AND 90000                             -- price range (IDR)
          AND protein_g >= 25                                           -- enough protein
          AND ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0) -- within 5 km (metres)
          AND to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled & healthy') -- matches the craving
        ORDER BY 0.6 * bm25(description, 'grilled healthy')             -- text relevance
               + 0.4 * (1 - (embedding <=> '[0.7,0.3,0.0,0.0]'))        -- taste similarity
          DESC
        LIMIT 10");
    let rows = query(&db, combined);
    // The two dishes that pass every condition, the exact taste match first.
    assert_eq!(texts(&rows, "name"), ["Ayam Bakar", "Ikan Bakar"]);
    // Contrast: drop ONE condition and exactly its dish comes back.
    let conditions = [
        ("open_now = true", "Tempe Bakar"),
        ("price BETWEEN 40000 AND 90000", "Lobster Bakar"),
        ("protein_g >= 25", "Gado Gado"),
        ("ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0)", "Sate Lilit"),
        ("to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled & healthy')", "Babi Guling"),
    ];
    let order = "ORDER BY 0.6 * bm25(description, 'grilled healthy') + 0.4 * (1 - (embedding <=> '[0.7,0.3,0.0,0.0]')) DESC LIMIT 10";
    for (dropped, dish) in conditions {
        let kept: Vec<&str> = conditions
            .iter()
            .map(|(c, _)| *c)
            .filter(|c| *c != dropped)
            .collect();
        let sql = format!("SELECT name, price FROM dishes WHERE {} {order}", kept.join(" AND "));
        // Babi Guling has no "healthy", so without the text match it is
        // in the rows but bm25 gives it nothing; the ranking still answers.
        let names = texts(&query(&db, &sql), "name");
        assert_eq!(names.len(), 3, "without `{dropped}`: {names:?}");
        assert!(names.contains(&dish.to_owned()), "without `{dropped}`: {names:?}");
    }
    // Contrast: the taste vector decides between the two text ties.
    let flipped = format!(
        "SELECT name, price FROM dishes WHERE {} ORDER BY 0.6 * bm25(description, 'grilled healthy') + 0.4 * (1 - (embedding <=> '[0.6,0.4,0.0,0.0]')) DESC LIMIT 10",
        conditions.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(" AND ")
    );
    assert_eq!(texts(&query(&db, &flipped), "name"), ["Ikan Bakar", "Ayam Bakar"]);
}

#[test]
fn the_journal() {
    let (_dir, db) = bali();
    run(&db, readme("CREATE TABLE diary (
        _key TEXT PRIMARY KEY, author TEXT, place TEXT,
        logged_at TIMESTAMPTZ, reflection TEXT, mood VECTOR(4)
    )"));
    run(&db, readme("CREATE INDEX ON diary USING gin       (to_tsvector('simple', reflection))"));
    run(&db, readme("CREATE INDEX ON diary USING quantized (mood vector_cosine_ops)"));
    for (key, author, place, at, text, mood) in [
        ("d1", "chloe", "uluwatu", "2024-06-03", "the cliff made me feel small and still", "[0.2, 0.7, 0.1, 0.0]"),
        ("d2", "chloe", "ubud", "2024-06-05", "rice terraces, small and still morning", "[0.3, 0.6, 0.1, 0.0]"),
        ("d3", "chloe", "seminyak", "2024-06-04", "loud beach club, not still at all", "[0.9, 0.1, 0.0, 0.0]"),
        ("d4", "aiym", "ubud", "2024-06-02", "felt small and still too", "[0.2, 0.7, 0.1, 0.0]"),
    ] {
        run(&db, &format!(
            "INSERT INTO diary (_key, author, place, logged_at, reflection, mood) VALUES ('{key}', '{author}', '{place}', '{at}', '{text}', '{mood}')"
        ));
    }
    let rows = query(&db, readme("SELECT place, logged_at FROM diary
        WHERE author = 'chloe'
          AND to_tsvector('simple', reflection) @@ to_tsquery('simple', 'small & still')
        ORDER BY logged_at"));
    assert_eq!(texts(&rows, "place"), ["uluwatu", "ubud"]);
    // Contrast: another author; and one word instead of two.
    let rows = query(&db, "SELECT place FROM diary WHERE author = 'aiym' AND to_tsvector('simple', reflection) @@ to_tsquery('simple', 'small & still')");
    assert_eq!(texts(&rows, "place"), ["ubud"]);
    let rows = query(&db, "SELECT place FROM diary WHERE author = 'chloe' AND to_tsvector('simple', reflection) @@ to_tsquery('simple', 'still') ORDER BY logged_at");
    assert_eq!(texts(&rows, "place"), ["uluwatu", "seminyak", "ubud"]);

    let rows = query(&db, readme("SELECT place, reflection FROM diary
        WHERE author = 'chloe'
        ORDER BY mood <=> '[0.2, 0.7, 0.1, 0.0]' ASC
        LIMIT 1"));
    // d4 has the same mood exactly, and is Aiym's: the author filter holds.
    assert_eq!(texts(&rows, "place"), ["uluwatu"]);
    let rows = query(&db, "SELECT place FROM diary WHERE author = 'chloe' ORDER BY mood <=> '[0.9, 0.1, 0.0, 0.0]' ASC LIMIT 1");
    assert_eq!(texts(&rows, "place"), ["seminyak"]);
}

#[test]
fn the_sql_tour() {
    let (_dir, db) = bali();
    // The index block restates section 2: an index that is already there is
    // a notice, not a second copy.
    run(&db, readme("CREATE INDEX ON dishes   USING gin        (to_tsvector('simple', description))"));
    run(&db, readme("CREATE INDEX ON tourists USING quantized  (taste vector_cosine_ops)"));

    run(&db, readme("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, category TEXT, geometry GEOMETRY(Point,4326))"));
    run(&db, readme("ALTER TABLE places ADD COLUMN rating REAL"));
    run(&db, readme("INSERT INTO places (_key, name, category) VALUES ('uluwatu', 'Uluwatu Temple', 'temple')"));
    run(&db, readme("UPDATE places SET rating = 4.8 WHERE _key = 'uluwatu'"));
    run(&db, "INSERT INTO places (_key, name, category) VALUES ('old-bar', 'Old Bar', 'closed')");
    assert_eq!(db.execute(readme("DELETE FROM places WHERE category = 'closed'"), &[]).unwrap(), 1);
    // Contrast: nothing left to delete.
    assert_eq!(db.execute("DELETE FROM places WHERE category = 'closed'", &[]).unwrap(), 0);
    let rows = query(&db, "SELECT rating FROM places WHERE _key = 'uluwatu'");
    assert_eq!(column(&rows, "rating"), [json!(4.8)]);

    for (key, lon, lat) in [
        ("seminyak-beach", 115.155, -8.691),
        ("kuta", 115.168, -8.718),
        ("jimbaran", 115.165, -8.770),
        ("canggu", 115.137, -8.647),
    ] {
        run(&db, &format!(
            r#"INSERT INTO places (_key, name, category, geometry) VALUES ('{key}', '{key}', 'beach', '{{"type":"Point","coordinates":[{lon},{lat}]}}')"#
        ));
    }
    // near: seminyak-beach -> kuta -> jimbaran -> uluwatu, and canggu -> seminyak-beach.
    for (a, b) in [("seminyak-beach", "kuta"), ("kuta", "jimbaran"), ("jimbaran", "uluwatu"), ("canggu", "seminyak-beach")] {
        db.link(("places", a), "near", ("places", b)).unwrap();
    }
    let rows = query(&db, readme("SELECT place
        FROM GRAPH_TABLE (base MATCH
            (a:places WHERE a._key = 'seminyak-beach')-[:near]->{1,3}(dest:places)
            COLUMNS (dest._key AS place))"));
    assert_eq!(sorted(texts(&rows, "place")), ["jimbaran", "kuta", "uluwatu"]);
    // Contrast: two hops stop short of Uluwatu; backward finds Canggu.
    let rows = query(&db, "SELECT place FROM GRAPH_TABLE (base MATCH (a:places WHERE a._key = 'seminyak-beach')-[:near]->{1,2}(dest:places) COLUMNS (dest._key AS place))");
    assert_eq!(sorted(texts(&rows, "place")), ["jimbaran", "kuta"]);
    let rows = query(&db, "SELECT place FROM GRAPH_TABLE (base MATCH (a:places WHERE a._key = 'seminyak-beach')<-[:near]-(dest:places) COLUMNS (dest._key AS place))");
    assert_eq!(texts(&rows, "place"), ["canggu"]);

    for (who, rating) in [("chloe", 4.8), ("aiym", 4.5), ("budi", 4.2)] {
        db.link_with(("tourists", who), "visited", ("places", "uluwatu"), &json!({"rating": rating}))
            .unwrap();
    }
    run(&db, "UPDATE places SET rating = 4.0 WHERE _key = 'kuta'");
    let rows = query(&db, readme("SELECT category, COUNT(*) AS n, AVG(rating) AS avg_rating
        FROM places
        GROUP BY category
        ORDER BY n DESC"));
    assert_eq!(texts(&rows, "category"), ["beach", "temple"]);
    assert_eq!(column(&rows, "n"), [json!(4), json!(1)]);
    // AVG skips the beaches with no rating: kuta's 4.0 alone.
    assert_eq!(column(&rows, "avg_rating"), [json!(4.0), json!(4.8)]);
    // Contrast: the other order.
    let rows = query(&db, "SELECT category, COUNT(*) AS n FROM places GROUP BY category ORDER BY n ASC");
    assert_eq!(texts(&rows, "category"), ["temple", "beach"]);

    let rows = query(&db, readme("SELECT visitor, rating
        FROM GRAPH_TABLE (base MATCH
            (p:places WHERE p._key = 'uluwatu')<-[v:visited]-(t:tourists)
            COLUMNS (t.name AS visitor, v.rating AS rating))
        ORDER BY rating DESC"));
    assert_eq!(texts(&rows, "visitor"), ["Chloe", "Aiym", "Budi"]);
    // Contrast: the other order.
    let rows = query(&db, "SELECT visitor, rating FROM GRAPH_TABLE (base MATCH
        (p:places WHERE p._key = 'uluwatu')<-[v:visited]-(t:tourists)
        COLUMNS (t.name AS visitor, v.rating AS rating)) ORDER BY rating ASC");
    assert_eq!(texts(&rows, "visitor"), ["Budi", "Aiym", "Chloe"]);

    let rows = query(&db, readme("SELECT * FROM places   WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0)"));
    // seminyak-beach 1.4 km, kuta 3.1 km, canggu 5.3 km (just outside).
    assert_eq!(sorted(texts(&rows, "name")), ["kuta", "seminyak-beach"]);
    let rows = query(&db, readme("SELECT * FROM tourists ORDER BY taste <=> '[0.9, 0.1, 0.0, 0.0]' LIMIT 5"));
    assert_eq!(texts(&rows, "name")[0], "Chloe");

    let rows = query(&db, readme("SHOW TABLES"));
    assert_eq!(
        sorted(texts(&rows, "name")),
        ["dishes", "flights", "places", "restaurants", "tourists"]
    );
    let rows = query(&db, readme("SHOW EDGES"));
    assert!(texts(&rows, "edge_type").contains(&"visited".to_owned()));
}

#[test]
fn the_rust_example() {
    let (_dir, db) = bali();
    run(&db, "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)");
    run(&db, "INSERT INTO places (_key, name) VALUES ('uluwatu', 'Uluwatu Temple')");
    // The README's Rust block, statement for statement.
    let nearby = db
        .query(
            readme("SELECT name FROM restaurants WHERE ST_DWithin(geometry, ST_MakePoint($1, $2)::geography, $3)"),
            &[json!(115.168), json!(-8.690), json!(3000.0)],
        )
        .unwrap();
    assert_eq!(sorted(texts(&nearby, "name")), ["La Lucciola", "Warung Made"]);
    // Contrast: the same prepared text at 1 km.
    let nearer = db
        .query(
            "SELECT name FROM restaurants WHERE ST_DWithin(geometry, ST_MakePoint($1, $2)::geography, $3)",
            &[json!(115.168), json!(-8.690), json!(1000.0)],
        )
        .unwrap();
    assert_eq!(texts(&nearer, "name"), ["Warung Made"]);
    db.link(("tourists", "chloe"), "visited", ("places", "uluwatu")).unwrap();
    db.link_with(
        ("tourists", "aiym"), "visited", ("places", "uluwatu"),
        &json!({"rating": 4.8, "hours": 2}),
    )
    .unwrap();
    let visited = db
        .neighbours(("tourists", "chloe"), Some("visited"), Direction::Outgoing, 16)
        .unwrap();
    assert_eq!(visited.len(), 1);
    // Contrast: nothing points OUT of Uluwatu; two point in.
    assert!(db.neighbours(("places", "uluwatu"), Some("visited"), Direction::Outgoing, 16).unwrap().is_empty());
    assert_eq!(db.neighbours(("places", "uluwatu"), Some("visited"), Direction::Incoming, 16).unwrap().len(), 2);
}

#[test]
fn every_tested_query_is_written_in_the_readme() {
    // `readme()` panics on a query that is not there; this names the
    // guarantee as a test of its own. The README's own fences must still
    // hold what this file asserts: a statement edited in one place and not
    // the other fails here before it can mislead a reader.
    readme("CREATE INDEX ON dishes USING exact (embedding)");
    readme("-[:similar_taste]->{1,2}(t:tourists)");
    readme("SELECT name, price
    FROM dishes");
}

#[test]
fn a_ranked_text_query_over_an_empty_table_is_zero_rows_not_corruption() {
    let (_dir, db) = open();
    run(&db, "CREATE TABLE notes (body TEXT)");
    run(&db, "CREATE INDEX ON notes USING gin (to_tsvector('simple', body))");
    let ranked = "SELECT _key FROM notes
        WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'grilled')
        ORDER BY bm25(body, 'grilled') DESC";
    assert_eq!(query(&db, ranked).len(), 0);
    // Contrast: one matching row, and the same statement finds it.
    run(&db, "INSERT INTO notes (_key, body) VALUES ('n1', 'grilled fish')");
    assert_eq!(texts(&query(&db, ranked), "_key"), ["n1"]);
    // And emptied again, it is zero rows again rather than an error.
    run(&db, "DELETE FROM notes WHERE _key = 'n1'");
    assert_eq!(query(&db, ranked).len(), 0);
}

#[test]
fn an_unnamed_create_index_takes_the_generated_name() {
    let (_dir, db) = bali();
    let rows = query(&db, "SHOW INDEXES ON dishes");
    let names = texts(&rows, "name");
    for expected in ["dishes_description_gin", "dishes_embedding_exact"] {
        assert!(names.contains(&expected.to_owned()), "{names:?}");
    }
    // Contrast: a name given is the name kept.
    run(&db, "CREATE INDEX tastes ON tourists USING exact (taste)");
    assert!(texts(&query(&db, "SHOW INDEXES ON tourists"), "name").contains(&"tastes".to_owned()));
}

#[test]
fn the_command_line_example() {
    let dir = tempfile::tempdir().unwrap();
    let bali = dir.path().join("bali");
    let shell = env!("CARGO_BIN_EXE_sekejap");
    let run_shell = |sql: &str| {
        let out = std::process::Command::new(shell)
            .arg(&bali)
            .arg(sql)
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (ok, _, err) = run_shell(
        "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, category TEXT); \
         INSERT INTO places (_key, name, category) VALUES ('uluwatu', 'Uluwatu Temple', 'temple');",
    );
    assert!(ok, "{err}");
    // The README's line, verbatim after the path.
    assert!(squash(README).contains("sekejap ./bali \"SELECT * FROM places;\""));
    let (ok, out, err) = run_shell("SELECT * FROM places;");
    assert!(ok, "{err}");
    assert!(out.contains("Uluwatu Temple") && out.contains("1 row"), "{out}");
    // Contrast: a statement the engine refuses exits non-zero and says why.
    let (ok, _, err) = run_shell("SELECT * FROM nowhere;");
    assert!(!ok);
    assert!(err.contains("error"), "{err}");
}
