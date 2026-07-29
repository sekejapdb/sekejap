//! Graph traversal with `SELECT ... FROM MATCH`.
//!
//!   cargo run --example graph_match

use sekejap::CoreDB;

fn main() {
    let mut db = CoreDB::new();

    db.execute("CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("CREATE TABLE flights (_key TEXT PRIMARY KEY, airline TEXT)").unwrap();
    db.execute("INSERT INTO tourists (_key, name) VALUES ('chloe', 'Chloe')").unwrap();
    db.execute("INSERT INTO tourists (_key, name) VALUES ('aiym', 'Aiym')").unwrap();
    db.execute("INSERT INTO flights (_key, airline) VALUES ('qf-mel', 'Qantas')").unwrap();

    // An edge: chloe -[:flew_on]-> qf-mel
    db.execute("INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')").unwrap();

    println!("Chloe's flight:");
    for hit in db
        .query(
            "SELECT f.airline AS airline \
             FROM MATCH (t:tourists)-[:flew_on]->(f:flights) \
             WHERE t._key = 'chloe'",
        )
        .unwrap()
        .collect()
    {
        println!("  {}", serde_json::to_string(&hit.payload.unwrap()).unwrap());
    }
}
