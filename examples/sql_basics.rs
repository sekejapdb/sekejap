//! Core SQL: CREATE / INSERT / SELECT / WHERE / GROUP BY on the sekejap engine.
//!
//!   cargo run --example sql_basics

use sekejap::CoreDB;

fn main() {
    let mut db = CoreDB::new(); // in-memory, ephemeral

    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)")
        .unwrap();
    for (key, name, area) in [
        ("uluwatu", "Uluwatu Temple", "south"),
        ("kuta", "Kuta Beach", "south"),
        ("ubud", "Ubud Center", "central"),
    ] {
        db.execute(&format!(
            "INSERT INTO places (_key, name, area) VALUES ('{key}', '{name}', '{area}')"
        ))
        .unwrap();
    }

    println!("places per area:");
    for hit in db
        .query("SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC")
        .unwrap()
        .collect()
    {
        println!("  {}", serde_json::to_string(&hit.payload.unwrap()).unwrap());
    }
}
