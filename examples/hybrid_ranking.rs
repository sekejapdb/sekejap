//! Hybrid ranking: combine BM25 text relevance and vector similarity into one
//! `ORDER BY` score — the multi-model query sekejap is built for.
//!
//!   cargo run --example hybrid_ranking

use sekejap::CoreDB;

fn main() {
    let mut db = CoreDB::new();

    db.execute(
        "CREATE TABLE dishes (_key TEXT PRIMARY KEY, name TEXT, description TEXT, embedding VECTOR)",
    )
    .unwrap();
    db.execute("CREATE INDEX ON dishes USING bm25 (description)").unwrap();
    db.execute("CREATE INDEX ON dishes USING hnsw (embedding)").unwrap();

    for (key, name, desc, emb) in [
        ("a", "Grilled Chicken", "healthy grilled chicken with herbs", "[1.0, 0.0, 0.0]"),
        ("b", "Fried Rice", "classic fried rice street food", "[0.0, 1.0, 0.0]"),
        ("c", "Grilled Fish", "grilled fish, light and healthy", "[0.8, 0.2, 0.0]"),
    ] {
        db.execute(&format!(
            "INSERT INTO dishes (_key, name, description, embedding) \
             VALUES ('{key}', '{name}', '{desc}', {emb})"
        ))
        .unwrap();
    }

    // Text relevance (0.6) + taste similarity to [1,0,0] (0.4), best first.
    let sql = "SELECT name FROM dishes \
               WHERE BM25(description, 'grilled healthy') > 0.0 \
               ORDER BY BM25_NORM(description, 'grilled healthy') * 0.6 \
                      + VECTOR_COSINE(embedding, [1.0, 0.0, 0.0]) * 0.4 DESC";

    println!("ranked dishes:");
    for hit in db.query(sql).unwrap().collect() {
        println!("  {}", serde_json::to_string(&hit.payload.unwrap()).unwrap());
    }
}
