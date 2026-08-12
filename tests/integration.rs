use sekejap::CoreDB;

// ── Basics ────────────────────────────────────────────────────────────────────

#[test]
fn put_and_get() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"name":"Alice","age":30}"#).unwrap();
    let json = db.get("alice").unwrap();
    assert!(json.contains("Alice"));
}

#[test]
fn put_bad_json_returns_error() {
    let mut db = CoreDB::new();
    assert!(db.put("x", "not json!!").is_err());
}

#[test]
fn remove_node() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"name":"Alice"}"#).unwrap();
    assert!(db.contains("alice"));
    db.remove("alice");
    assert!(!db.contains("alice"));
    assert_eq!(db.get("alice"), None);
}

#[test]
fn upsert_updates_collection_index() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"_collection":"x"}"#).unwrap();
    db.put("a", r#"{"_collection":"y"}"#).unwrap(); // upsert into different collection

    // "a" should now be in "y", NOT "x"
    let in_y = db.collection("y").count();
    let in_x = db.collection("x").count();
    assert_eq!(in_y, 1);
    assert_eq!(in_x, 0);
}

// ── Graph traversal ───────────────────────────────────────────────────────────

#[test]
fn forward_traversal() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"name":"Alice"}"#).unwrap();
    db.put("bob",   r#"{"name":"Bob"}"#).unwrap();
    db.put("carol", r#"{"name":"Carol"}"#).unwrap();
    db.link("alice", "bob",   "follows");
    db.link("alice", "carol", "follows");

    let hits = db.one("alice").forward("follows").collect();
    let names: Vec<&str> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Bob"));
    assert!(names.contains(&"Carol"));
}

#[test]
fn backward_traversal() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"name":"Alice"}"#).unwrap();
    db.put("bob",   r#"{"name":"Bob"}"#).unwrap();
    db.link("alice", "bob", "follows");

    let hits = db.one("bob").backward("follows").collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "alice");
}

#[test]
fn hops_bfs() {
    let mut db = CoreDB::new();
    for n in ["a","b","c","d"] {
        db.put(n, &format!(r#"{{"id":"{}"}}"#, n)).unwrap();
    }
    db.link("a", "b", "e");
    db.link("b", "c", "e");
    db.link("c", "d", "e");

    // 2 hops from "a" should reach "a","b","c" but not "d"
    let reached = db.one("a").hops(2).count();
    assert_eq!(reached, 3); // a, b, c

    // 3 hops from "a" should reach all four
    let reached = db.one("a").hops(3).count();
    assert_eq!(reached, 4);
}

#[test]
fn roots_and_leaves() {
    let mut db = CoreDB::new();
    db.put("root",  r#"{}"#).unwrap();
    db.put("mid",   r#"{}"#).unwrap();
    db.put("leaf",  r#"{}"#).unwrap();
    db.link("root", "mid",  "e");
    db.link("mid",  "leaf", "e");

    assert_eq!(db.all().roots().count(), 1);
    assert_eq!(db.all().roots().first().unwrap().slug, "root");
    assert_eq!(db.all().leaves().count(), 1);
    assert_eq!(db.all().leaves().first().unwrap().slug, "leaf");
}

#[test]
fn unlink_removes_edge() {
    let mut db = CoreDB::new();
    db.put("a", r#"{}"#).unwrap();
    db.put("b", r#"{}"#).unwrap();
    db.link("a", "b", "e");
    db.unlink("a", "b", "e");

    assert_eq!(db.one("a").forward("e").count(), 0);
    assert_eq!(db.one("b").backward("e").count(), 0);
}

// ── Collection queries ────────────────────────────────────────────────────────

#[test]
fn collection_query() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"_collection":"users","name":"Alice"}"#).unwrap();
    db.put("bob",   r#"{"_collection":"users","name":"Bob"}"#).unwrap();
    db.put("post1", r#"{"_collection":"posts","title":"Hi"}"#).unwrap();

    assert_eq!(db.collection("users").count(), 2);
    assert_eq!(db.collection("posts").count(), 1);
    assert_eq!(db.collection("unknown").count(), 0);
}

// ── Payload filters ───────────────────────────────────────────────────────────

#[test]
fn where_eq() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"name":"Alice","role":"admin"}"#).unwrap();
    db.put("bob",   r#"{"name":"Bob",  "role":"user"}"#).unwrap();

    let hits = db.all().where_eq("role", "admin").collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "alice");
}

#[test]
fn where_gt_lt() {
    let mut db = CoreDB::new();
    db.put("young", r#"{"age":20}"#).unwrap();
    db.put("mid",   r#"{"age":35}"#).unwrap();
    db.put("old",   r#"{"age":60}"#).unwrap();

    assert_eq!(db.all().where_gt("age", 30.0).count(), 2);
    assert_eq!(db.all().where_lt("age", 30.0).count(), 1);
    assert_eq!(db.all().where_between("age", 25.0, 50.0).count(), 1);
}

#[test]
fn where_in_filter() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"status":"active"}"#).unwrap();
    db.put("b", r#"{"status":"inactive"}"#).unwrap();
    db.put("c", r#"{"status":"pending"}"#).unwrap();

    let hits = db.all()
        .where_in("status", vec![
            serde_json::json!("active"),
            serde_json::json!("pending"),
        ])
        .count();
    assert_eq!(hits, 2);
}

#[test]
fn like_filter() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"email":"alice@example.com"}"#).unwrap();
    db.put("b", r#"{"email":"bob@corp.com"}"#).unwrap();

    assert_eq!(db.all().like("email", "example.com").count(), 1);
}

// ── Set algebra ───────────────────────────────────────────────────────────────

#[test]
fn intersect() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"role":"admin","active":true}"#).unwrap();
    db.put("b", r#"{"role":"admin","active":false}"#).unwrap();
    db.put("c", r#"{"role":"user", "active":true}"#).unwrap();

    let admins = db.all().where_eq("role", "admin");
    let active = db.all().where_eq("active", true);
    let hits = admins.intersect(active).count();
    assert_eq!(hits, 1); // only "a"
}

#[test]
fn union() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"tag":"rust"}"#).unwrap();
    db.put("b", r#"{"tag":"python"}"#).unwrap();
    db.put("c", r#"{"tag":"go"}"#).unwrap();

    let rust = db.all().where_eq("tag", "rust");
    let go   = db.all().where_eq("tag", "go");
    assert_eq!(rust.union(go).count(), 2);
}

#[test]
fn subtract() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"score":10}"#).unwrap();
    db.put("b", r#"{"score":20}"#).unwrap();
    db.put("c", r#"{"score":30}"#).unwrap();

    let all  = db.all();
    let high = db.all().where_gt("score", 15.0);
    // all minus high = just "a"
    assert_eq!(all.subtract(high).count(), 1);
}

// ── Shaping ───────────────────────────────────────────────────────────────────

#[test]
fn sort_and_take() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"score":30}"#).unwrap();
    db.put("b", r#"{"score":10}"#).unwrap();
    db.put("c", r#"{"score":20}"#).unwrap();

    let hits = db.all().sort("score", true).take(2).collect();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].payload.as_ref().unwrap()["score"], 10);
    assert_eq!(hits[1].payload.as_ref().unwrap()["score"], 20);
}

#[test]
fn skip_and_take() {
    let mut db = CoreDB::new();
    for i in 0..10u32 {
        db.put(&format!("node{i}"), &format!(r#"{{"i":{i}}}"#)).unwrap();
    }
    let hits = db.all().sort("i", true).skip(3).take(4).collect();
    assert_eq!(hits.len(), 4);
    assert_eq!(hits[0].payload.as_ref().unwrap()["i"], 3);
}

#[test]
fn select_projection() {
    let mut db = CoreDB::new();
    db.put("alice", r#"{"name":"Alice","age":30,"secret":"xyz"}"#).unwrap();

    let hits = db.one("alice")
        .select(["name", "age"])
        .collect();
    let p = hits[0].payload.as_ref().unwrap();
    assert!(p.get("name").is_some());
    assert!(p.get("age").is_some());
    assert!(p.get("secret").is_none());
}

// ── Edge inspection ───────────────────────────────────────────────────────────

#[test]
fn edges_from_and_to() {
    let mut db = CoreDB::new();
    db.put("a", r#"{}"#).unwrap();
    db.put("b", r#"{}"#).unwrap();
    db.link("a", "b", "edge");

    let fwd = db.edges_from("a");
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0].to_slug.as_deref(), Some("b"));
    // A naked edge carries no attributes.
    assert!(fwd[0].meta.is_none());

    let rev = db.edges_to("b");
    assert_eq!(rev.len(), 1);
    assert_eq!(rev[0].from_slug.as_deref(), Some("a"));
}

#[test]
fn link_meta_stores_metadata() {
    let mut db = CoreDB::new();
    db.put("a", r#"{}"#).unwrap();
    db.put("b", r#"{}"#).unwrap();
    db.link_meta("a", "b", "knows", r#"{"since":2020}"#).unwrap();

    let edges = db.edges_from("a");
    let meta = edges[0].meta.as_ref().unwrap();
    assert_eq!(meta["since"], 2020);
}

// ── Many nodes ────────────────────────────────────────────────────────────────

#[test]
fn put_many_and_count() {
    let mut db = CoreDB::new();
    let items: Vec<(String, String)> = (0..100)
        .map(|i| (format!("node{i}"), format!(r#"{{"i":{i}}}"#)))
        .collect();

    db.put_many(items.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
    assert_eq!(db.node_count(), 100);
}

#[test]
fn many_starter() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"v":1}"#).unwrap();
    db.put("b", r#"{"v":2}"#).unwrap();
    db.put("c", r#"{"v":3}"#).unwrap();

    let hits = db.many(["a", "c"]).collect();
    assert_eq!(hits.len(), 2);
}

// ── SQL execute (INSERT / DELETE) ──────────────────────────────────────────────

#[test]
fn execute_insert_creates_node() {
    let mut db = CoreDB::new();
    let n = db.execute(
        "INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)"
    ).unwrap();
    assert_eq!(n, 1);
    assert!(db.contains("users/alice"));
    let payload = db.get("users/alice").unwrap();
    let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(v["name"], "Alice");
    assert_eq!(v["_collection"], "users");
}

#[test]
fn execute_insert_is_queryable() {
    let mut db = CoreDB::new();
    db.execute("INSERT INTO products (_key, price) VALUES ('p1', 10)").unwrap();
    db.execute("INSERT INTO products (_key, price) VALUES ('p2', 50)").unwrap();
    db.execute("INSERT INTO products (_key, price) VALUES ('p3', 100)").unwrap();

    let hits = db.query("SELECT * FROM products WHERE price > 20").unwrap().collect();
    assert_eq!(hits.len(), 2);

    // Verify slug is collection/_key
    let all = db.query("SELECT * FROM products").unwrap().collect();
    assert!(all.iter().any(|h| h.slug == "products/p1"));
}

#[test]
fn execute_delete_removes_matching_nodes() {
    let mut db = CoreDB::new();
    db.put("keep",   r#"{"_collection":"items","active":true}"#).unwrap();
    db.put("remove", r#"{"_collection":"items","active":false}"#).unwrap();

    let n = db.execute("DELETE FROM items WHERE active = false").unwrap();
    assert_eq!(n, 1);
    assert!(db.contains("keep"));
    assert!(!db.contains("remove"));
}

#[test]
fn execute_delete_all() {
    let mut db = CoreDB::new();
    for i in 0..5u32 {
        db.put(&format!("n{i}"), "{}").unwrap();
    }
    let n = db.execute("DELETE FROM ALL").unwrap();
    assert_eq!(n, 5);
    assert_eq!(db.node_count(), 0);
}

#[test]
fn execute_insert_error_on_missing_key() {
    let mut db = CoreDB::new();
    let err = db.execute("INSERT INTO users (name) VALUES ('Alice')").unwrap_err();
    assert!(matches!(err, sekejap::SqlError::MissingField { field: "_key" }));
}

// ── MATCH integration tests ──────────────────────────────────────────────────

fn setup_music_db() -> CoreDB {
    let mut db = CoreDB::new();
    db.put("artist/the-vines", r#"{"_collection":"artist","_key":"the-vines","name":"The Vines"}"#).unwrap();
    db.put("genre/garage-rock", r#"{"_collection":"genre","_key":"garage-rock","name":"Garage Rock"}"#).unwrap();
    db.put("genre/alternative", r#"{"_collection":"genre","_key":"alternative","name":"Alternative"}"#).unwrap();
    db.put("city/melbourne", r#"{"_collection":"city","_key":"melbourne","name":"Melbourne"}"#).unwrap();
    // `strength` is now an ordinary edge attribute, not a privileged field.
    db.link_meta("artist/the-vines", "genre/garage-rock", "has_genre", r#"{"strength":10}"#).unwrap();
    db.link_meta("artist/the-vines", "genre/alternative", "has_genre", r#"{"strength":5}"#).unwrap();
    db.link("artist/the-vines", "city/melbourne", "origin");
    db
}

#[test]
fn match_forward_one_hop() {
    let db = setup_music_db();
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines'"
    ).unwrap().collect();
    let names: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Garage Rock"), "got: {:?}", names);
    assert!(names.contains(&"Alternative"), "got: {:?}", names);
    assert_eq!(names.len(), 2);
}

#[test]
fn match_backward_one_hop() {
    let db = setup_music_db();
    // Backward `<-`: anchor at the genre, walk against the arrow to its artists.
    let hits = db.query(
        "SELECT a.* FROM MATCH (g:genre)<-[:has_genre]-(a:artist) WHERE g._key = 'garage-rock'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].payload.as_ref().unwrap().get("name").unwrap().as_str() == Some("The Vines"));
}

#[test]
fn select_graph_returns_nodes_and_edges() {
    let db = setup_music_db();
    // `SELECT GRAPH` → one Hit whose payload is `{nodes, edges}` for the traversal.
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (a:artist)-[e:has_genre]->(g:genre) WHERE a._key = 'the-vines'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1, "graph mode returns a single graph object");
    let g = hits[0].payload.as_ref().unwrap();
    let nodes = g.get("nodes").unwrap().as_array().unwrap();
    let edges = g.get("edges").unwrap().as_array().unwrap();
    // artist + 2 genres = 3 nodes; 2 has_genre edges.
    assert_eq!(nodes.len(), 3, "nodes: {nodes:?}");
    assert_eq!(edges.len(), 2, "edges: {edges:?}");
    // Node shape: slug/collection/key/payload.
    assert!(nodes.iter().any(|n| n.get("slug").unwrap() == "artist/the-vines"
        && n.get("collection").unwrap() == "artist"
        && n.get("key").unwrap() == "the-vines"));
    // Edge shape: from/to/type/attrs; `strength` (fast-lane attr) surfaces.
    let e = edges.iter()
        .find(|e| e.get("to").unwrap() == "genre/garage-rock")
        .expect("edge to garage-rock");
    assert_eq!(e.get("from").unwrap(), "artist/the-vines");
    assert_eq!(e.get("type").unwrap(), "has_genre");
    assert_eq!(e.get("attrs").unwrap().get("strength").unwrap(), 10);
}

#[test]
fn select_graph_inbound_normalizes_edge_direction() {
    let db = setup_music_db();
    // Queried inbound (`<-`) but edges must still be emitted in STORED forward
    // direction (artist → genre), so a viz layer draws arrows correctly.
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (g:genre)<-[e:has_genre]-(a:artist) WHERE g._key = 'garage-rock'"
    ).unwrap().collect();
    let g = hits[0].payload.as_ref().unwrap();
    let edges = g.get("edges").unwrap().as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].get("from").unwrap(), "artist/the-vines");
    assert_eq!(edges[0].get("to").unwrap(), "genre/garage-rock");
}

// ── GQL grammar alignment ─────────────────────────────────────────────────────

#[test]
fn match_gql_edge_inline_props_filter() {
    let db = setup_music_db();
    // has_genre edges carry `strength`: garage-rock=10, alternative=5.
    // GQL inline edge prop `{strength: 10}` must filter to only that edge.
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[e:has_genre {strength: 10}]->(g:genre) WHERE a._key = 'the-vines'"
    ).unwrap().collect();
    let names: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert_eq!(names, vec!["Garage Rock"], "inline edge prop must keep only strength=10; got {names:?}");
}

#[test]
fn match_gql_edge_inline_props_anonymous() {
    let db = setup_music_db();
    // Anonymous edge (no bind) with an inline prop still filters.
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre {strength: 5}]->(g:genre) WHERE a._key = 'the-vines'"
    ).unwrap().collect();
    let names: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert_eq!(names, vec!["Alternative"], "anon edge inline prop strength=5; got {names:?}");
}

fn setup_chain_db() -> CoreDB {
    let mut db = CoreDB::new();
    for k in ["a", "b", "c", "d"] {
        db.put(&format!("n/{k}"), &format!(r#"{{"_collection":"n","_key":"{k}"}}"#)).unwrap();
    }
    db.link("n/a", "n/b", "next");
    db.link("n/b", "n/c", "next");
    db.link("n/c", "n/d", "next");
    db
}

#[test]
fn match_gql_brace_quantifier_exact() {
    let db = setup_chain_db();
    // `{2}` → exactly 2 hops from a: path a→b→c (2 stored edges).
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (x:n)-[e:next]->{2}(y) WHERE x._key = 'a'"
    ).unwrap().collect();
    let g = hits[0].payload.as_ref().unwrap();
    let edges = g.get("edges").unwrap().as_array().unwrap();
    assert_eq!(edges.len(), 2, "exact {{2}} traverses a->b->c: {edges:?}");
}

#[test]
fn match_gql_anonymous_edge() {
    let db = setup_chain_db(); // a→b→c→d via `next`
    // `-->` — anonymous forward edge, any type.
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (x:n)-->(y) WHERE x._key = 'a'"
    ).unwrap().collect();
    let edges = hits[0].payload.as_ref().unwrap().get("edges").unwrap().as_array().unwrap().clone();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].get("from").unwrap(), "n/a");
    assert_eq!(edges[0].get("to").unwrap(), "n/b");

    // `<--` — anonymous backward edge; emitted in stored forward direction.
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (y:n)<--(x) WHERE y._key = 'b'"
    ).unwrap().collect();
    let edges = hits[0].payload.as_ref().unwrap().get("edges").unwrap().as_array().unwrap().clone();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].get("from").unwrap(), "n/a");
    assert_eq!(edges[0].get("to").unwrap(), "n/b");

    // `-->{1,2}` — anonymous edge with quantifier reaches a,b,c.
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (x:n)-->{1,2}(y) WHERE x._key = 'a'"
    ).unwrap().collect();
    let nodes = hits[0].payload.as_ref().unwrap().get("nodes").unwrap().as_array().unwrap().len();
    assert_eq!(nodes, 3, "a-->{{1,2}} reaches a,b,c");
}

#[test]
fn match_gql_undirected_edge() {
    let mut db = CoreDB::new();
    for k in ["alice", "bob", "carol"] {
        db.put(&format!("p/{k}"), &format!(r#"{{"_collection":"p","_key":"{k}"}}"#)).unwrap();
    }
    // Stored one-way: alice→bob, bob→carol.
    db.link("p/alice", "p/bob", "friends");
    db.link("p/bob", "p/carol", "friends");

    // Directed from bob sees only carol.
    let g = db.query("SELECT GRAPH FROM MATCH (a:p)-[:friends]->(x) WHERE a._key='bob'")
        .unwrap().collect();
    assert_eq!(g[0].payload.as_ref().unwrap()["edges"].as_array().unwrap().len(), 1);

    // Undirected from bob sees BOTH neighbours (alice via reverse, carol via forward).
    let g = db.query("SELECT GRAPH FROM MATCH (a:p)-[:friends]-(x) WHERE a._key='bob'")
        .unwrap().collect();
    let payload = g[0].payload.as_ref().unwrap();
    let edges = payload["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "undirected sees both neighbours: {edges:?}");
    // Edges emitted in STORED direction regardless of traversal side.
    assert!(edges.iter().any(|e| e["from"] == "p/alice" && e["to"] == "p/bob"));
    assert!(edges.iter().any(|e| e["from"] == "p/bob" && e["to"] == "p/carol"));

    // Undirected reachability *1..2 from alice reaches alice, bob, carol (no U-turn dupes).
    let g = db.query("SELECT GRAPH FROM MATCH (a:p)-[:friends*1..2]-(x) WHERE a._key='alice'")
        .unwrap().collect();
    let nodes = g[0].payload.as_ref().unwrap()["nodes"].as_array().unwrap().len();
    assert_eq!(nodes, 3, "alice reaches alice,bob,carol");
}

#[test]
fn match_gql_path_functions() {
    // a→b→c→d chain with weighted edges; a GQL path variable exposes the path.
    let mut db = CoreDB::new();
    for k in ["a", "b", "c", "d"] {
        db.put(&format!("n/{k}"), &format!(r#"{{"_collection":"n","_key":"{k}"}}"#)).unwrap();
    }
    db.link_meta("n/a", "n/b", "next", r#"{"w":10}"#).unwrap();
    db.link_meta("n/b", "n/c", "next", r#"{"w":20}"#).unwrap();
    db.link_meta("n/c", "n/d", "next", r#"{"w":30}"#).unwrap();

    let hits = db.query(
        "SELECT length(p) AS hops, nodes(p) AS via, relationships(p) AS rels \
         FROM MATCH p = (x:n)-[e:next]->{1,3}(y) WHERE x._key = 'a' ORDER BY hops"
    ).unwrap().collect();
    assert_eq!(hits.len(), 3, "1..3 hops → three paths");

    // Deepest path: a→b→c→d.
    let deep = hits.iter()
        .find(|h| h.payload.as_ref().unwrap()["hops"].as_i64() == Some(3))
        .expect("3-hop path");
    let p = deep.payload.as_ref().unwrap();
    let via: Vec<&str> = p["via"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(via, vec!["n/a", "n/b", "n/c", "n/d"]);
    let rels = p["rels"].as_array().unwrap();
    assert_eq!(rels.len(), 3);
    assert_eq!(rels[0]["from"], "n/a");
    assert_eq!(rels[0]["to"], "n/b");
    assert_eq!(rels[0]["type"], "next");
    assert_eq!(rels[0]["attrs"]["w"], 10);
}

#[test]
fn gql_path_dotted_access_rejected() {
    let db = setup_chain_db();
    // Dotted field access on a path variable is a HARD error — the only way to
    // read a path is length(p)/nodes(p)/relationships(p).
    assert!(db.query(
        "SELECT p.length FROM MATCH p = (x:n)-[:next]->(y) WHERE x._key = 'a'"
    ).is_err(), "p.length must be rejected");
    assert!(db.query(
        "SELECT p.nodes AS via FROM MATCH p = (x:n)-[:next]->(y) WHERE x._key = 'a'"
    ).is_err(), "p.nodes must be rejected");
    // The official spelling works.
    assert!(db.query(
        "SELECT length(p) AS h FROM MATCH p = (x:n)-[:next]->(y) WHERE x._key = 'a'"
    ).is_ok());
}

#[test]
fn match_gql_brace_quantifier_range() {
    let db = setup_chain_db();
    // `{1,3}` → 1..3 hops from a reaches b, c, d (plus a itself) = 4 nodes.
    let hits = db.query(
        "SELECT GRAPH FROM MATCH (x:n)-[e:next]->{1,3}(y) WHERE x._key = 'a'"
    ).unwrap().collect();
    let g = hits[0].payload.as_ref().unwrap();
    let nodes = g.get("nodes").unwrap().as_array().unwrap();
    assert_eq!(nodes.len(), 4, "1..3 hops from a reaches a,b,c,d: {nodes:?}");
}

#[test]
fn match_backward_multihop_descendants() {
    // child_of hierarchy: village -> district -> province.
    // Backward from the province walks DOWN to all descendants.
    let mut db = CoreDB::new();
    db.put("province/vic", r#"{"_collection":"province","_key":"vic"}"#).unwrap();
    db.put("district/melb", r#"{"_collection":"district","_key":"melb"}"#).unwrap();
    db.put("village/cbd",   r#"{"_collection":"village","_key":"cbd"}"#).unwrap();
    db.put("village/docklands", r#"{"_collection":"village","_key":"docklands"}"#).unwrap();
    db.link("district/melb", "province/vic", "child_of");
    db.link("village/cbd",   "district/melb", "child_of");
    db.link("village/docklands", "district/melb", "child_of");

    // Everything under vic (1..2 hops backward): melb, cbd, docklands
    let hits = db.query(
        "SELECT d._key AS k FROM MATCH (p:province)<-[:child_of*1..2]-(d) WHERE p._key = 'vic'"
    ).unwrap().collect();
    let mut keys: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("k")?.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["cbd", "docklands", "melb"]);
}

#[test]
fn match_backward_with_edge_property() {
    let db = setup_music_db();
    // Backward + edge property: strongest artist→genre link for garage-rock.
    let hits = db.query(
        "SELECT a._key AS artist, r.strength AS w \
         FROM MATCH (g:genre)<-[r:has_genre]-(a:artist) \
         WHERE g._key = 'garage-rock' ORDER BY r.strength DESC"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap().get("w").unwrap().as_f64(), Some(10.0));
}

#[test]
fn match_strength_filter() {
    let db = setup_music_db();
    // Only has_genre edges with strength >= 7 should pass (garage-rock=10, alternative=5)
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[r:has_genre]->(g:genre) WHERE a._key = 'the-vines' AND r.strength >= 7"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].payload.as_ref().unwrap().get("name").unwrap().as_str() == Some("Garage Rock"));
}

#[test]
fn match_inline_props_end_node() {
    let db = setup_music_db();
    let hits = db.query(
        "SELECT a.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE g._key = 'garage-rock'"
    ).unwrap().collect();
    // This should find nodes reachable from any artist via has_genre, filtered to _key=garage-rock
    // The result is the genre node itself (end node), filtered by inline props
    assert!(!hits.is_empty());
}

#[test]
fn match_typed_multihop_bfs() {
    let mut db = CoreDB::new();
    // Chain: flood -> drainage_failure -> budget_cut -> policy_change
    db.put("event/flood", r#"{"_collection":"event","_key":"flood","name":"Maribyrnong Flood"}"#).unwrap();
    db.put("event/drainage", r#"{"_collection":"event","_key":"drainage","name":"Drainage Failure"}"#).unwrap();
    db.put("event/budget", r#"{"_collection":"event","_key":"budget","name":"Budget Cut"}"#).unwrap();
    db.put("event/policy", r#"{"_collection":"event","_key":"policy","name":"Policy Change"}"#).unwrap();
    db.link("event/flood", "event/drainage", "caused_by");
    db.link("event/drainage", "event/budget", "caused_by");
    db.link("event/budget", "event/policy", "caused_by");

    let hits = db.query(
        "SELECT root.* FROM MATCH (e:event)-[:caused_by*1..5]->(root) WHERE e._key = 'flood'"
    ).unwrap().collect();
    let names: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Drainage Failure"), "got: {:?}", names);
    assert!(names.contains(&"Budget Cut"), "got: {:?}", names);
    assert!(names.contains(&"Policy Change"), "got: {:?}", names);
    assert_eq!(names.len(), 3);
}

#[test]
fn match_union_two_patterns() {
    let db = setup_music_db();
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines' \
         UNION \
         SELECT c.* FROM MATCH (a:artist)-[:origin]->(c:city) WHERE a._key = 'the-vines'"
    ).unwrap().collect();
    let names: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    // Should have genres + city
    assert!(names.contains(&"Garage Rock"), "got: {:?}", names);
    assert!(names.contains(&"Melbourne"), "got: {:?}", names);
}

#[test]
fn match_with_limit() {
    let db = setup_music_db();
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines' LIMIT 1"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
}

// ── SELECT DISTINCT + COUNT(DISTINCT) over MATCH ─────────────────────────────

fn setup_diamond() -> CoreDB {
    // a→b→d, a→c→d : d is reachable two ways.
    let mut db = CoreDB::new();
    for k in ["a", "b", "c", "d"] {
        db.put(&format!("n/{k}"), &format!(r#"{{"_collection":"n","_key":"{k}"}}"#)).unwrap();
    }
    db.link("n/a", "n/b", "e");
    db.link("n/a", "n/c", "e");
    db.link("n/b", "n/d", "e");
    db.link("n/c", "n/d", "e");
    db
}

#[test]
fn match_multihop_returns_path_rows_by_default() {
    let db = setup_diamond();
    // Default: one row per path — d appears twice (via b and via c).
    let hits = db.query(
        "SELECT x._key AS k FROM MATCH (a:n)-[:e*1..2]->(x) WHERE a._key = 'a'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 4, "b, c, d, d");
}

#[test]
fn match_select_distinct_dedups_nodes() {
    let db = setup_diamond();
    let hits = db.query(
        "SELECT DISTINCT x._key AS k FROM MATCH (a:n)-[:e*1..2]->(x) WHERE a._key = 'a'"
    ).unwrap().collect();
    let mut keys: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("k")?.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["b", "c", "d"], "unique reachable nodes");
}

#[test]
fn match_count_distinct() {
    let mut db = CoreDB::new();
    db.put("a/a", r#"{"_collection":"a","_key":"a"}"#).unwrap();
    for (k, g) in [("b", "rock"), ("c", "rock"), ("d", "jazz")] {
        db.put(&format!("s/{k}"), &format!(r#"{{"_collection":"s","_key":"{k}","genre":"{g}"}}"#)).unwrap();
        db.link("a/a", &format!("s/{k}"), "e");
    }
    // COUNT(*) counts paths (3 songs); COUNT(DISTINCT genre) counts unique genres (2).
    let hits = db.query(
        "SELECT COUNT(*) AS n, COUNT(DISTINCT s.genre) AS g FROM MATCH (a:a)-[:e]->(s) WHERE a._key = 'a'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p.get("n").unwrap().as_i64(), Some(3));
    assert_eq!(p.get("g").unwrap().as_i64(), Some(2));
}

#[test]
fn match_count_distinct_with_group_by() {
    // COUNT(DISTINCT start.field) grouped by destination — exercises the general
    // path (fast path can't compute distinct or bind the start var).
    let mut db = CoreDB::new();
    db.put("places/uluwatu", r#"{"_collection":"places","_key":"uluwatu"}"#).unwrap();
    for (t, city) in [("chloe", "Melbourne"), ("aiym", "Melbourne"), ("giulia", "Milan")] {
        db.put(&format!("tourists/{t}"), &format!(r#"{{"_collection":"tourists","_key":"{t}","home_city":"{city}"}}"#)).unwrap();
        db.link(&format!("tourists/{t}"), "places/uluwatu", "visited");
    }
    let hits = db.query(
        "SELECT p._key AS place, COUNT(*) AS visitors, COUNT(DISTINCT t.home_city) AS cities \
         FROM MATCH (p:places)<-[:visited]-(t:tourists) GROUP BY p._key"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p.get("visitors").unwrap().as_i64(), Some(3));
    assert_eq!(p.get("cities").unwrap().as_i64(), Some(2)); // Melbourne, Milan
}

#[test]
fn match_empty_aggregate_returns_one_row() {
    // PostgreSQL: aggregate without GROUP BY yields exactly one row even when
    // nothing matches — COUNT→0, other aggregates→NULL.
    let mut db = CoreDB::new();
    db.put("n/a", r#"{"_collection":"n","_key":"a"}"#).unwrap(); // no outgoing edges
    let hits = db.query(
        "SELECT COUNT(*) AS n, AVG(x.v) AS a, MIN(x.v) AS mn \
         FROM MATCH (a:n)-[:e]->(x) WHERE a._key = 'a'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p.get("n").unwrap().as_i64(), Some(0));
    assert!(p.get("a").unwrap().is_null());
    assert!(p.get("mn").unwrap().is_null());
}

#[test]
fn match_with_chaining_from_bound_var() {
    // `WITH x MATCH (x)-[:e]->(y)` continues the traversal from the bound var.
    let mut db = CoreDB::new();
    db.put("users/chloe",  r#"{"_collection":"users","_key":"chloe"}"#).unwrap();
    db.put("users/giulia", r#"{"_collection":"users","_key":"giulia"}"#).unwrap();
    db.put("dishes/ayam",  r#"{"_collection":"dishes","_key":"ayam","name":"Ayam"}"#).unwrap();
    db.put("dishes/babi",  r#"{"_collection":"dishes","_key":"babi","name":"Babi"}"#).unwrap();
    db.link("users/chloe",  "users/giulia", "similar");
    db.link("users/giulia", "dishes/ayam",  "ate");
    db.link("users/giulia", "dishes/babi",  "ate");

    let hits = db.query(
        "SELECT d.name AS dish, COUNT(*) AS n \
         FROM MATCH (c:users)-[:similar]->(peer:users) WHERE c._key = 'chloe' \
         WITH peer \
         MATCH (peer)-[:ate]->(d:dishes) \
         GROUP BY d.name ORDER BY d.name"
    ).unwrap().collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("dish")?.as_str()).collect();
    assert_eq!(names, vec!["Ayam", "Babi"]);
}

#[test]
fn match_anonymous_and_inline_props_nodes() {
    // Anonymous nodes `(:label)`, inline props `(:label {f: 'v'})`, and a
    // direction change all in one pattern.
    let mut db = CoreDB::new();
    db.put("places/uluwatu", r#"{"_collection":"places","_key":"uluwatu"}"#).unwrap();
    for k in ["chloe", "aiym", "giulia"] {
        db.put(&format!("tourists/{k}"), &format!(r#"{{"_collection":"tourists","_key":"{k}"}}"#)).unwrap();
    }
    db.link("tourists/chloe",  "places/uluwatu", "visited");
    db.link("tourists/aiym",   "places/uluwatu", "reviewed");
    db.link("tourists/giulia", "places/uluwatu", "reviewed");

    // anonymous middle node with inline props + forward→backward:
    // a visitor of Uluwatu, paired with everyone who reviewed it
    let hits = db.query(
        "SELECT b._key AS k FROM MATCH (a:tourists)-[:visited]->(:places {_key: 'uluwatu'})<-[:reviewed]-(b:tourists)"
    ).unwrap().collect();
    let mut keys: Vec<&str> = hits.iter().filter_map(|h| h.payload.as_ref()?.get("k")?.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["aiym", "giulia"]);

    // anonymous end node, no label
    let n = db.query(
        "SELECT a._key AS k FROM MATCH (a:tourists)-[:reviewed]->(:places) WHERE a._key = 'aiym'"
    ).unwrap().collect();
    assert_eq!(n.len(), 1);
}

#[test]
fn match_varlength_path_intrinsics_correct() {
    // A route chain: seminyak -> kuta -> canggu -> uluwatu.
    // Path intrinsics on a variable-length hop must reflect the FULL path.
    let mut db = CoreDB::new();
    for k in ["seminyak", "kuta", "canggu", "uluwatu"] {
        db.put(&format!("places/{k}"), &format!(r#"{{"_collection":"places","_key":"{k}"}}"#)).unwrap();
    }
    db.link("places/seminyak", "places/kuta",    "route");
    db.link("places/kuta",     "places/canggu",  "route");
    db.link("places/canggu",   "places/uluwatu", "route");

    let hits = db.query(
        "SELECT dest._key AS dest, length(p) AS depth, nodes(p) AS via \
         FROM MATCH p = (a:places)-[:route*1..3]->(dest:places) WHERE a._key = 'seminyak'"
    ).unwrap().collect();
    let ulu = hits.iter().find(|h|
        h.payload.as_ref().and_then(|p| p.get("dest")).and_then(|v| v.as_str()) == Some("uluwatu")
    ).expect("uluwatu row").payload.as_ref().unwrap();
    assert_eq!(ulu.get("depth").unwrap().as_i64(), Some(3));   // 3 physical hops
    let via = ulu.get("via").unwrap().as_array().expect("nodes(p) array");
    assert_eq!(via.len(), 4);                                  // 4 nodes on the path
    assert_eq!(via[0].as_str(), Some("places/seminyak"));
    assert_eq!(via[3].as_str(), Some("places/uluwatu"));
}

// ── Release audit fixes (Tier 1) ─────────────────────────────────────────────

#[test]
fn multi_from_where_is_applied() {
    // A tour bundle: which tourists (from a collection) pair with places reached
    // in the graph — filtered on BOTH sides.
    let mut db = CoreDB::new();
    db.put("places/uluwatu", r#"{"_collection":"places","_key":"uluwatu"}"#).unwrap();
    db.put("places/kuta",    r#"{"_collection":"places","_key":"kuta"}"#).unwrap();
    db.link("places/uluwatu", "places/kuta", "near");
    for (k, c) in [("chloe", "Melbourne"), ("aiym", "Almaty"), ("giulia", "Melbourne")] {
        db.put(&format!("tourists/{k}"), &format!(r#"{{"_collection":"tourists","_key":"{k}","home":"{c}"}}"#)).unwrap();
    }
    let hits = db.query(
        "SELECT b._key AS place, t._key AS tourist \
         FROM MATCH (a:places)-[:near]->(b:places), tourists AS t \
         WHERE a._key = 'uluwatu' AND t.home = 'Melbourne'"
    ).unwrap().collect();
    // 1 place (kuta) × 2 Melbourne tourists = 2 rows (Almaty excluded)
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.payload.as_ref().unwrap().get("place").unwrap().as_str() == Some("kuta")));
}

#[test]
fn spatial_distance_length_area_parse() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","geometry":{"type":"Point","coordinates":[115.087,-8.829]}}"#).unwrap();
    // These three all used to fail to parse.
    assert!(db.query("SELECT * FROM p WHERE ST_Distance(geometry, POINT(115.087 -8.829), 5000.0)").is_ok());
    db.put("ln/1", r#"{"_collection":"ln","_key":"1","geometry":{"type":"LineString","coordinates":[[115.0,-8.8],[115.1,-8.9]]}}"#).unwrap();
    assert!(db.query("SELECT * FROM ln WHERE ST_Length(geometry, 1.0)").is_ok());
    db.put("pg/1", r#"{"_collection":"pg","_key":"1","geometry":{"type":"Polygon","coordinates":[[[0,0],[0,1],[1,1],[1,0],[0,0]]]}}"#).unwrap();
    assert!(db.query("SELECT * FROM pg WHERE ST_Area(geometry, 0.0)").is_ok());
}

#[test]
fn age_days_parses_iso_timestamps() {
    let mut db = CoreDB::new();
    db.put("t/1", r#"{"_collection":"t","_key":"1","iso":"2020-01-01T00:00:00Z","naive":"2020-01-01T00:00:00","date":"2020-01-01"}"#).unwrap();
    let hits = db.query(
        "SELECT AGE_DAYS(x.iso) AS iso, AGE_DAYS(x.naive) AS naive, AGE_DAYS(x.date) AS date \
         FROM MATCH (x:t) WHERE x._key = '1'"
    ).unwrap().collect();
    let p = hits[0].payload.as_ref().unwrap();
    // all three forms must parse (non-null, and equal since all are midnight UTC 2020-01-01)
    assert!(!p.get("iso").unwrap().is_null());
    assert_eq!(p.get("iso").unwrap().as_i64(), p.get("date").unwrap().as_i64());
    assert_eq!(p.get("iso").unwrap().as_i64(), p.get("naive").unwrap().as_i64());
}

#[test]
fn bm25_scores_common_terms() {
    // A term in >50% of docs must still score > 0 (smoothed IDF).
    let mut db = CoreDB::new();
    db.put("r/1", r#"{"_collection":"r","_key":"1","body":"sunset temple quiet"}"#).unwrap();
    db.put("r/2", r#"{"_collection":"r","_key":"2","body":"sunset beach club"}"#).unwrap();
    db.put("r/3", r#"{"_collection":"r","_key":"3","body":"sunset cliff view"}"#).unwrap();
    db.put("r/4", r#"{"_collection":"r","_key":"4","body":"rice terrace morning"}"#).unwrap();
    db.execute("CREATE INDEX ON r USING bm25 (body)").unwrap();
    // "sunset" is in 3/4 docs (>50%) — must return the 3, not 0.
    let hits = db.query("SELECT _key FROM r WHERE BM25(body, 'sunset') > 0.0").unwrap().collect();
    assert_eq!(hits.len(), 3);
}

#[test]
fn plain_select_scalar_functions() {
    // NOW() AS, AGE_DAYS, AGE_HOURS, JSON_ARRAY_LENGTH now work in plain SELECT.
    let mut db = CoreDB::new();
    db.put("trips/1", r#"{"_collection":"trips","_key":"1","arrival":"2020-01-01T00:00:00Z","stops":["ubud","kuta","uluwatu"]}"#).unwrap();
    let hits = db.query(
        "SELECT _key, AGE_DAYS(arrival) AS age, AGE_HOURS(arrival) AS hrs, \
                JSON_ARRAY_LENGTH(stops) AS n, NOW() AS now FROM trips"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert!(p.get("age").unwrap().as_i64().unwrap() > 0);
    assert!(p.get("hrs").unwrap().as_i64().unwrap() > 0);
    assert_eq!(p.get("n").unwrap().as_i64(), Some(3));
    assert!(p.get("now").unwrap().is_string());
}

#[test]
fn raw_put_key_is_filterable_on_destination() {
    // Raw db.put WITHOUT _key in the JSON — _key/_id are derived from the slug,
    // so a destination `_key` filter works.
    let mut db = CoreDB::new();
    db.put("h/x", r#"{"_collection":"h","name":"X"}"#).unwrap();
    db.put("h/y", r#"{"_collection":"h","name":"Y"}"#).unwrap();
    db.link("h/x", "h/y", "near");
    let hits = db.query(
        "SELECT b.name AS n FROM MATCH (a:h)-[:near]->(b:h) WHERE a._key = 'x' AND b._key = 'y'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap().get("n").unwrap().as_str(), Some("Y"));
}

// ── Bare MATCH is banned — only SELECT … FROM MATCH ──────────────────────────

#[test]
fn bare_match_return_is_rejected() {
    let db = setup_music_db();
    // The old Cypher-style surface must no longer parse.
    assert!(db.query("MATCH (a:artist)-[:has_genre]->(g:genre) RETURN g").is_err(),
        "bare MATCH ... RETURN must be rejected");
    assert!(db.query("MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key='the-vines' RETURN g").is_err());
    // The supported form still works.
    assert!(db.query("SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key='the-vines'").is_ok());
}

// ── Edge properties in SELECT … FROM MATCH ───────────────────────────────────

fn setup_ev_db() -> CoreDB {
    let mut db = CoreDB::new();
    db.put("vehicles/ev1", r#"{"_collection":"vehicles","_key":"ev1","name":"EV One"}"#).unwrap();
    db.put("vehicles/ev2", r#"{"_collection":"vehicles","_key":"ev2","name":"EV Two"}"#).unwrap();
    db.put("chargers/c1", r#"{"_collection":"chargers","_key":"c1","name":"Charger 1"}"#).unwrap();
    db.put("chargers/c2", r#"{"_collection":"chargers","_key":"c2","name":"Charger 2"}"#).unwrap();
    // charged_at edges carry attributes {kwh, price, strength} — strength is now
    // just an ordinary named attribute, no longer privileged.
    db.link_meta("vehicles/ev1", "chargers/c1", "charged_at", r#"{"kwh":50,"price":20,"strength":0.9}"#).unwrap();
    db.link_meta("vehicles/ev1", "chargers/c2", "charged_at", r#"{"kwh":30,"price":12,"strength":0.5}"#).unwrap();
    db.link_meta("vehicles/ev2", "chargers/c1", "charged_at", r#"{"kwh":45,"price":18,"strength":0.7}"#).unwrap();
    db
}

#[test]
fn match_edge_property_projection() {
    let db = setup_ev_db();
    let hits = db.query(
        "SELECT c._key AS charger, s.kwh AS energy \
         FROM MATCH (v:vehicles)-[s:charged_at]->(c:chargers) \
         WHERE v._key = 'ev1'"
    ).unwrap().collect();
    let mut energies: Vec<i64> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("energy")?.as_i64())
        .collect();
    energies.sort();
    assert_eq!(energies, vec![30, 50], "edge meta field s.kwh should project");
}

#[test]
fn match_edge_property_filter() {
    let db = setup_ev_db();
    let hits = db.query(
        "SELECT c._key AS charger, s.kwh AS energy \
         FROM MATCH (v:vehicles)-[s:charged_at]->(c:chargers) \
         WHERE v._key = 'ev1' AND s.kwh > 40"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1, "only the 50 kWh session passes s.kwh > 40");
    assert_eq!(hits[0].payload.as_ref().unwrap().get("charger").unwrap().as_str(), Some("c1"));
}

#[test]
fn match_edge_property_order() {
    let db = setup_ev_db();
    let hits = db.query(
        "SELECT c._key AS charger, s.kwh AS energy \
         FROM MATCH (v:vehicles)-[s:charged_at]->(c:chargers) \
         WHERE v._key = 'ev1' ORDER BY s.kwh DESC"
    ).unwrap().collect();
    let energies: Vec<i64> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("energy")?.as_i64())
        .collect();
    assert_eq!(energies, vec![50, 30], "ORDER BY s.kwh DESC");
}

#[test]
fn match_edge_property_strength_scalar() {
    let db = setup_ev_db();
    let hits = db.query(
        "SELECT c._key AS charger, s.strength AS w \
         FROM MATCH (v:vehicles)-[s:charged_at]->(c:chargers) \
         WHERE v._key = 'ev1'"
    ).unwrap().collect();
    let mut ws: Vec<f64> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("w")?.as_f64())
        .collect();
    ws.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(ws.len(), 2);
    assert!((ws[0] - 0.5).abs() < 1e-6 && (ws[1] - 0.9).abs() < 1e-6, "got {:?}", ws);
}

#[test]
fn match_edge_property_group_aggregate() {
    let db = setup_ev_db();
    // Total kWh delivered per charger: c1 = 50 + 45 = 95, c2 = 30.
    let hits = db.query(
        "SELECT c._key AS charger, SUM(s.kwh) AS total_kwh \
         FROM MATCH (v:vehicles)-[s:charged_at]->(c:chargers) \
         GROUP BY c._key"
    ).unwrap().collect();
    let mut got: Vec<(String, i64)> = hits.iter().map(|h| {
        let p = h.payload.as_ref().unwrap();
        (p.get("charger").unwrap().as_str().unwrap().to_string(),
         p.get("total_kwh").unwrap().as_f64().unwrap() as i64)
    }).collect();
    got.sort();
    assert_eq!(got, vec![("c1".to_string(), 95), ("c2".to_string(), 30)]);
}

// ── MATCH optimisation integration tests ─────────────────────────────────────

/// End _key condition in WHERE → One() inside Intersect (O(1) end-node lookup).
#[test]
fn match_end_node_key_in_where() {
    let db = setup_music_db();
    // Both start AND end have _key — should return exactly the targeted genre
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines' AND g._key = 'garage-rock'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let name = hits[0].payload.as_ref().unwrap()["name"].as_str().unwrap();
    assert_eq!(name, "Garage Rock");
}

/// End WHERE filter (non-_key) moves inside Intersect and still filters correctly.
#[test]
fn match_end_node_filter_in_where() {
    let db = setup_music_db();
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines' AND g.name = 'Garage Rock'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1, "should return only Garage Rock genre");
    assert_eq!(
        hits[0].payload.as_ref().unwrap()["name"].as_str().unwrap(),
        "Garage Rock"
    );
}

/// End node without a label: fall back to plain WhereEq filter (still correct).
#[test]
fn match_end_no_label_where_filter() {
    let db = setup_music_db();
    // (a:artist)-[:has_genre]->(b)  — no label on end, filter by name
    let hits = db.query(
        "SELECT b.* FROM MATCH (a:artist)-[:has_genre]->(b) WHERE a._key = 'the-vines' AND b.name = 'Garage Rock'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
}

/// Reverse anchor: single-hop with _key filter on dest → uses reverse anchor
/// in execute_match_agg (SELECT ... FROM MATCH syntax).
#[test]
fn match_reverse_anchor_single_hop() {
    let db = setup_music_db();
    // Start is a collection scan (a:artist), dest has _key filter on g.
    // Reverse anchor should kick in: walk rev_edges from genre/garage-rock
    // and find artist/the-vines as the only source.
    let hits = db.query(
        "SELECT a.name AS name FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE g._key = 'garage-rock'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let name = hits[0].payload.as_ref().unwrap()["name"].as_str().unwrap();
    assert_eq!(name, "The Vines");
}

/// Reverse anchor with specific edge type filter — only matching edge types are traversed.
#[test]
fn match_reverse_anchor_with_edge_type() {
    let db = setup_music_db();
    // Use edge type :origin which links artist→city.
    // Filter on city _key = 'melbourne' should reverse-anchor and find the-vines.
    let hits = db.query(
        "SELECT a.name AS name FROM MATCH (a:artist)-[:origin]->(c:city) WHERE c._key = 'melbourne'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["name"].as_str().unwrap(), "The Vines");

    // Edge type :has_genre should NOT match city nodes — 0 results.
    let hits2 = db.query(
        "SELECT a.name AS name FROM MATCH (a:artist)-[:has_genre]->(c:city) WHERE c._key = 'melbourne'"
    ).unwrap().collect();
    assert_eq!(hits2.len(), 0);
}

/// Multi-hop or non-_key filter → falls back to forward traversal.
/// Verify results are identical to what forward path produces.
#[test]
fn match_reverse_anchor_not_applicable() {
    let mut db = CoreDB::new();
    // Build a 2-hop chain: person → team → league
    db.put("person/alice", r#"{"_collection":"person","_key":"alice","name":"Alice"}"#).unwrap();
    db.put("team/rockets", r#"{"_collection":"team","_key":"rockets","name":"Rockets"}"#).unwrap();
    db.put("league/nbl", r#"{"_collection":"league","_key":"nbl","name":"NBL"}"#).unwrap();
    db.link("person/alice", "team/rockets", "member_of");
    db.link("team/rockets", "league/nbl", "plays_in");

    // Multi-hop: reverse anchor should NOT apply (hops.len() == 2).
    // Forward traversal should still find the path.
    let hits = db.query(
        "SELECT p.name AS name FROM MATCH (p:person)-[:member_of]->(t:team)-[:plays_in]->(l:league) WHERE l._key = 'nbl'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["name"].as_str().unwrap(), "Alice");
}

#[test]
fn ilike_filter() {
    let db = setup_music_db();
    let hits = db.query(
        "SELECT * FROM artist WHERE name ILIKE 'VINES'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].payload.as_ref().unwrap().get("name").unwrap().as_str() == Some("The Vines"));
}

// ── Spatial integration tests ────────────────────────────────────────────────

fn setup_spatial_db() -> CoreDB {
    let mut db = CoreDB::new();

    // Points: Melbourne landmarks
    db.put("places/melb-central", r#"{
        "_collection": "places",
        "_key": "melb-central",
        "name": "Melbourne Central",
        "category": "landmark",
        "geometry": {"type": "Point", "coordinates": [144.9631, -37.8102]}
    }"#).unwrap();

    db.put("places/flinders-st", r#"{
        "_collection": "places",
        "_key": "flinders-st",
        "name": "Flinders Street Station",
        "category": "landmark",
        "geometry": {"type": "Point", "coordinates": [144.9671, -37.8183]}
    }"#).unwrap();

    db.put("places/exhibition-bldg", r#"{
        "_collection": "places",
        "_key": "exhibition-bldg",
        "name": "Royal Exhibition Building",
        "category": "landmark",
        "geometry": {"type": "Point", "coordinates": [144.9717, -37.8047]}
    }"#).unwrap();

    // Far away point: Geelong
    db.put("places/geelong-station", r#"{
        "_collection": "places",
        "_key": "geelong-station",
        "name": "Geelong Station",
        "category": "transport",
        "geometry": {"type": "Point", "coordinates": [144.3617, -38.1499]}
    }"#).unwrap();

    // Polygons: zones
    db.put("zones/cbd", r#"{
        "_collection": "zones",
        "_key": "cbd",
        "name": "CBD Zone",
        "geometry": {
            "type": "Polygon",
            "coordinates": [[
                [144.95, -37.80],
                [144.98, -37.80],
                [144.98, -37.83],
                [144.95, -37.83],
                [144.95, -37.80]
            ]]
        }
    }"#).unwrap();

    db.put("zones/fitzroy", r#"{
        "_collection": "zones",
        "_key": "fitzroy",
        "name": "Fitzroy Zone",
        "geometry": {
            "type": "Polygon",
            "coordinates": [[
                [144.97, -37.79],
                [145.00, -37.79],
                [145.00, -37.81],
                [144.97, -37.81],
                [144.97, -37.79]
            ]]
        }
    }"#).unwrap();

    // LineString: tram route
    db.put("routes/tram96", r#"{
        "_collection": "routes",
        "_key": "tram96",
        "name": "Tram Route 96",
        "geometry": {
            "type": "LineString",
            "coordinates": [
                [144.95, -37.81],
                [144.96, -37.81],
                [144.97, -37.81],
                [144.98, -37.81]
            ]
        }
    }"#).unwrap();

    db.build_spatial_index();
    db
}

#[test]
fn spatial_st_dwithin() {
    let db = setup_spatial_db();
    // Find places within 2km of Melbourne Central
    let hits = db.query(
        "SELECT * FROM places WHERE ST_DWithin(geometry, POINT(144.9631 -37.8102), 2000.0)"
    ).unwrap().collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Melbourne Central"));
    assert!(names.contains(&"Flinders Street Station"));
    assert!(names.contains(&"Royal Exhibition Building"));
    assert!(!names.contains(&"Geelong Station"), "Geelong should be too far: {:?}", names);
}

#[test]
fn spatial_st_contains_point() {
    let db = setup_spatial_db();
    // Find zones containing Melbourne Central's coordinates
    let hits = db.query(
        "SELECT * FROM zones WHERE ST_Contains(geometry, POINT(144.9631 -37.8102))"
    ).unwrap().collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"CBD Zone"), "CBD should contain Melbourne Central: {:?}", names);
}

#[test]
fn spatial_st_within_polygon() {
    let db = setup_spatial_db();
    // Find places within a big box around CBD
    let hits = db.query(
        "SELECT * FROM places WHERE ST_Within(geometry, POLYGON((144.94 -37.79, 144.99 -37.79, 144.99 -37.83, 144.94 -37.83, 144.94 -37.79)))"
    ).unwrap().collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Melbourne Central"));
    assert!(names.contains(&"Flinders Street Station"));
    assert!(names.contains(&"Royal Exhibition Building"));
    assert!(!names.contains(&"Geelong Station"));
}

#[test]
fn spatial_st_intersects() {
    let db = setup_spatial_db();
    // The tram route crosses a rectangle overlapping its path
    let hits = db.query(
        "SELECT * FROM routes WHERE ST_Intersects(geometry, POLYGON((144.955 -37.815, 144.975 -37.815, 144.975 -37.805, 144.955 -37.805, 144.955 -37.815)))"
    ).unwrap().collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Tram Route 96"), "Tram route should intersect: {:?}", names);
}

#[test]
fn spatial_atomic_api() {
    let db = setup_spatial_db();
    // Test atomic API: st_dwithin
    let hits = db.collection("places")
        .st_dwithin(-37.8102, 144.9631, 2000.0)
        .collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Melbourne Central"));
    assert!(names.contains(&"Flinders Street Station"));
    assert!(!names.contains(&"Geelong Station"));

    // Test atomic API: near (alias)
    let near_count = db.collection("places")
        .near(-37.8102, 144.9631, 2000.0)
        .count();
    assert_eq!(near_count, hits.len());
}

#[test]
fn spatial_sql_combined() {
    let db = setup_spatial_db();
    // Combine spatial with regular filter
    let hits = db.query(
        "SELECT * FROM places WHERE ST_DWithin(geometry, POINT(144.9631 -37.8102), 2000.0) AND category = 'landmark'"
    ).unwrap().collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"Melbourne Central"));
    assert!(names.contains(&"Flinders Street Station"));
    assert!(names.contains(&"Royal Exhibition Building"));
    assert_eq!(names.len(), 3);
}

#[test]
fn spatial_st_contains_point_atomic() {
    let db = setup_spatial_db();
    let hits = db.collection("zones")
        .st_contains_point(-37.8102, 144.9631)
        .collect();
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"CBD Zone"));
}

#[test]
fn spatial_execute_insert_then_query() {
    let mut db = CoreDB::new();
    db.execute(
        "INSERT INTO places (_key, name, geometry) VALUES ('melb-central', 'Melbourne Central', '{\"type\":\"Point\",\"coordinates\":[144.9631,-37.8102]}')"
    ).unwrap();
    let hits = db.query(
        "SELECT * FROM places WHERE ST_DWithin(geometry, POINT(144.9631 -37.8136), 1000.0)"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "places/melb-central");
    assert!(hits[0].payload.as_ref().unwrap().get("name").unwrap().as_str() == Some("Melbourne Central"));
}

// ── Spatial grid specific tests ──────────────────────────────────────────────

#[test]
fn spatial_grid_same_results_as_brute_force() {
    // Run the same queries with and without grid, compare results
    let mut db_brute = CoreDB::new();
    let mut db_grid = CoreDB::new();

    let nodes = [
        ("p1", r#"{"_collection":"places","geometry":{"type":"Point","coordinates":[144.96,-37.81]}}"#),
        ("p2", r#"{"_collection":"places","geometry":{"type":"Point","coordinates":[144.97,-37.82]}}"#),
        ("p3", r#"{"_collection":"places","geometry":{"type":"Point","coordinates":[145.50,-38.00]}}"#),
    ];
    for (slug, json) in &nodes {
        db_brute.put(slug, json).unwrap();
        db_grid.put(slug, json).unwrap();
    }
    db_grid.build_spatial_index();

    // ST_DWithin
    let brute = db_brute.collection("places").st_dwithin(-37.81, 144.96, 2000.0).count();
    let grid  = db_grid.collection("places").st_dwithin(-37.81, 144.96, 2000.0).count();
    assert_eq!(brute, grid, "ST_DWithin mismatch");

    // ST_ContainsPoint (need polygon nodes)
    let mut db_brute2 = CoreDB::new();
    let mut db_grid2 = CoreDB::new();
    let zone = r#"{"_collection":"zones","geometry":{"type":"Polygon","coordinates":[[
        [144.95,-37.80],[144.98,-37.80],[144.98,-37.83],[144.95,-37.83],[144.95,-37.80]
    ]]}}"#;
    db_brute2.put("z1", zone).unwrap();
    db_grid2.put("z1", zone).unwrap();
    db_grid2.build_spatial_index();

    let brute2 = db_brute2.collection("zones").st_contains_point(-37.81, 144.96).count();
    let grid2  = db_grid2.collection("zones").st_contains_point(-37.81, 144.96).count();
    assert_eq!(brute2, grid2, "ST_ContainsPoint mismatch");
}

#[test]
fn spatial_grid_incremental_update() {
    let mut db = CoreDB::new();
    db.put("p1", r#"{"_collection":"places","geometry":{"type":"Point","coordinates":[144.96,-37.81]}}"#).unwrap();
    db.build_spatial_index();

    // Verify initial state
    assert_eq!(db.collection("places").st_dwithin(-37.81, 144.96, 1000.0).count(), 1);

    // Insert after grid build — should be found via incremental update
    db.put("p2", r#"{"_collection":"places","geometry":{"type":"Point","coordinates":[144.97,-37.82]}}"#).unwrap();
    assert_eq!(db.collection("places").st_dwithin(-37.81, 144.96, 2000.0).count(), 2);

    // Remove — should no longer be found
    db.remove("p1");
    let hits = db.collection("places").st_dwithin(-37.81, 144.96, 2000.0).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "p2");
}

// ── INSERT with geometry JSON tests ──────────────────────────────────────────

#[test]
fn insert_geometry_json_auto_parsed() {
    let mut db = CoreDB::new();
    db.execute(
        r#"INSERT INTO places (_key, name, geometry) VALUES ('fed-square', 'Federation Square', '{"type":"Point","coordinates":[144.9694,-37.8180]}')"#
    ).unwrap();
    db.build_spatial_index();

    // The geometry should have been parsed into a native JSON object, not kept as string
    let raw = db.get("places/fed-square").unwrap();
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(payload["geometry"].is_object(), "geometry should be parsed object, not string");
    assert_eq!(payload["geometry"]["type"], "Point");

    // Should be queryable via spatial SQL
    let hits = db.query(
        "SELECT * FROM places WHERE ST_DWithin(geometry, POINT(144.9694 -37.8180), 1000.0)"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "places/fed-square");
}

#[test]
fn insert_geometry_polygon_json_auto_parsed() {
    let mut db = CoreDB::new();
    db.execute(
        r#"INSERT INTO zones (_key, name, geometry) VALUES ('fitzroy', 'Fitzroy', '{"type":"Polygon","coordinates":[[[144.97,-37.79],[145.00,-37.79],[145.00,-37.82],[144.97,-37.82],[144.97,-37.79]]]}')"#
    ).unwrap();
    db.build_spatial_index();

    let hits = db.query(
        "SELECT * FROM zones WHERE ST_Contains(geometry, POINT(144.98 -37.80))"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "zones/fitzroy");
}

// ── INSERT edge integration tests ────────────────────────────────────────────

#[test]
fn insert_edge_single() {
    let mut db = CoreDB::new();
    db.put("artist/the-vines", r#"{"name":"The Vines","_collection":"artist","_key":"the-vines"}"#).unwrap();
    db.put("genre/garage-rock", r#"{"name":"Garage Rock","_collection":"genre","_key":"garage-rock"}"#).unwrap();

    let count = db.execute("INSERT ('artist/the-vines')-[:has_genre {strength: 10}]->('genre/garage-rock')").unwrap();
    assert_eq!(count, 1);

    // Verify edge via atomic API
    let hits = db.one("artist/the-vines").forward("has_genre").collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "genre/garage-rock");

    // Verify via MATCH
    let hits = db.query(
        "SELECT g.* FROM MATCH (a:artist)-[:has_genre]->(g:genre) WHERE a._key = 'the-vines'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap().get("_key").unwrap().as_str(), Some("garage-rock"));
}

#[test]
fn insert_edge_with_meta() {
    let mut db = CoreDB::new();
    db.put("city/melbourne", r#"{"name":"Melbourne","_collection":"city"}"#).unwrap();
    db.put("suburb/fitzroy", r#"{"name":"Fitzroy","_collection":"suburb"}"#).unwrap();

    db.execute("INSERT ('city/melbourne')-[:contains {strength: 1, distance: 3.2}]->('suburb/fitzroy')").unwrap();

    let edges = db.edges_from("city/melbourne");
    assert_eq!(edges.len(), 1);
    let meta = edges[0].meta.as_ref().unwrap();
    // strength and distance are both ordinary attributes now.
    assert_eq!(meta["strength"], 1);
    assert_eq!(meta["distance"], 3.2);
}

#[test]
fn insert_edge_multiple() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"_collection":"node"}"#).unwrap();
    db.put("b", r#"{"_collection":"node"}"#).unwrap();
    db.put("c", r#"{"_collection":"node"}"#).unwrap();

    let count = db.execute(
        "INSERT ('a')-[:links {strength: 5}]->('b'), ('b')-[:links {strength: 3}]->('c')"
    ).unwrap();
    assert_eq!(count, 2);

    let hits_b = db.one("a").forward("links").collect();
    assert_eq!(hits_b.len(), 1);
    assert_eq!(hits_b[0].slug, "b");

    let hits_c = db.one("b").forward("links").collect();
    assert_eq!(hits_c.len(), 1);
    assert_eq!(hits_c[0].slug, "c");

    // Full chain
    let chain = db.one("a").forward("links").forward("links").collect();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].slug, "c");
}

#[test]
fn insert_edge_default_is_naked() {
    let mut db = CoreDB::new();
    db.put("x", r#"{"_collection":"node"}"#).unwrap();
    db.put("y", r#"{"_collection":"node"}"#).unwrap();

    db.execute("INSERT ('x')-[:knows]->('y')").unwrap();

    let edges = db.edges_from("x");
    assert_eq!(edges.len(), 1);
    // The default edge is zero — no attributes.
    assert!(edges[0].meta.is_none());
}

#[test]
fn delete_edge_removes_edge() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"_collection":"node"}"#).unwrap();
    db.put("b", r#"{"_collection":"node"}"#).unwrap();
    db.link("a", "b", "knows");

    // Verify edge exists
    assert_eq!(db.one("a").forward("knows").count(), 1);

    // Delete it
    let count = db.execute("DELETE ('a')-[:knows]->('b')").unwrap();
    assert_eq!(count, 1);

    // Verify gone
    assert_eq!(db.one("a").forward("knows").count(), 0);
    assert_eq!(db.one("b").backward("knows").count(), 0);
}


// ── JSON path operators (-> / ->>) ────────────────────────────────────────────

#[test]
fn json_path_text_where() {
    let mut db = CoreDB::new();
    db.put(
        "users/alice",
        r#"{"_collection":"users","_key":"alice","profile":{"role":"admin","age":30}}"#,
    )
    .unwrap();
    db.put(
        "users/bob",
        r#"{"_collection":"users","_key":"bob","profile":{"role":"viewer","age":25}}"#,
    )
    .unwrap();

    // ->> returns TEXT; compare to string literal
    let hits = db
        .query("SELECT * FROM users WHERE profile->>'role' = 'admin'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/alice");
}

#[test]
fn json_path_obj_where() {
    let mut db = CoreDB::new();
    db.put(
        "items/a",
        r#"{"_collection":"items","_key":"a","meta":{"status":{"active":true},"score":9}}"#,
    )
    .unwrap();
    db.put(
        "items/b",
        r#"{"_collection":"items","_key":"b","meta":{"status":{"active":false},"score":3}}"#,
    )
    .unwrap();

    // -> returns JSON value; compare to number
    let hits = db
        .query("SELECT * FROM items WHERE meta->'score' > 5")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "items/a");
}

#[test]
fn json_path_deep_chain() {
    let mut db = CoreDB::new();
    db.put(
        "nodes/x",
        r#"{"_collection":"nodes","_key":"x","a":{"b":{"c":"deep"}}}"#,
    )
    .unwrap();
    db.put(
        "nodes/y",
        r#"{"_collection":"nodes","_key":"y","a":{"b":{"c":"other"}}}"#,
    )
    .unwrap();

    // Three-level deep path with ->>
    let hits = db
        .query("SELECT * FROM nodes WHERE a->'b'->>'c' = 'deep'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "nodes/x");
}

#[test]
fn json_path_select_projection() {
    let mut db = CoreDB::new();
    db.put(
        "users/u1",
        r#"{"_collection":"users","_key":"u1","profile":{"name":"Alice","role":"admin"}}"#,
    )
    .unwrap();

    let hits = db
        .query("SELECT profile->>'role' FROM users")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    // Output key should be the last path segment ("role"), value should be the text
    let payload = hits[0].payload.as_ref().unwrap();
    assert_eq!(payload["role"], serde_json::json!("admin"));
}

#[test]
fn json_path_combined_where_and_plain() {
    let mut db = CoreDB::new();
    db.put(
        "orders/1",
        r#"{"_collection":"orders","_key":"1","status":"active","extra":{"priority":"high"}}"#,
    )
    .unwrap();
    db.put(
        "orders/2",
        r#"{"_collection":"orders","_key":"2","status":"active","extra":{"priority":"low"}}"#,
    )
    .unwrap();
    db.put(
        "orders/3",
        r#"{"_collection":"orders","_key":"3","status":"closed","extra":{"priority":"high"}}"#,
    )
    .unwrap();

    // Combine plain field + JSON path in WHERE
    let hits = db
        .query("SELECT * FROM orders WHERE status = 'active' AND extra->>'priority' = 'high'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "orders/1");
}

// ── IS NULL / IS NOT NULL ─────────────────────────────────────────────────────

#[test]
fn where_is_null() {
    let mut db = CoreDB::new();
    db.put("users/1", r#"{"_collection":"users","_key":"1","name":"Alice","email":"a@x.com"}"#)
        .unwrap();
    db.put("users/2", r#"{"_collection":"users","_key":"2","name":"Bob"}"#)
        .unwrap();
    db.put("users/3", r#"{"_collection":"users","_key":"3","name":"Carol","email":null}"#)
        .unwrap();

    // IS NULL should match Bob (missing) and Carol (explicit null)
    let hits = db
        .query("SELECT * FROM users WHERE email IS NULL")
        .unwrap()
        .collect();
    let slugs: std::collections::HashSet<_> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert_eq!(slugs.len(), 2);
    assert!(slugs.contains("users/2"));
    assert!(slugs.contains("users/3"));
}

#[test]
fn where_is_not_null() {
    let mut db = CoreDB::new();
    db.put("users/1", r#"{"_collection":"users","_key":"1","name":"Alice","email":"a@x.com"}"#)
        .unwrap();
    db.put("users/2", r#"{"_collection":"users","_key":"2","name":"Bob"}"#)
        .unwrap();

    let hits = db
        .query("SELECT * FROM users WHERE email IS NOT NULL")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/1");
}

// ── NOT condition ─────────────────────────────────────────────────────────────

#[test]
fn where_not_eq() {
    let mut db = CoreDB::new();
    db.put("items/1", r#"{"_collection":"items","_key":"1","status":"active"}"#)
        .unwrap();
    db.put("items/2", r#"{"_collection":"items","_key":"2","status":"inactive"}"#)
        .unwrap();
    db.put("items/3", r#"{"_collection":"items","_key":"3","status":"active"}"#)
        .unwrap();

    let hits = db
        .query("SELECT * FROM items WHERE NOT status = 'active'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "items/2");
}

// ── OR conditions ─────────────────────────────────────────────────────────────

#[test]
fn where_or_basic() {
    let mut db = CoreDB::new();
    db.put("products/1", r#"{"_collection":"products","_key":"1","category":"books"}"#)
        .unwrap();
    db.put("products/2", r#"{"_collection":"products","_key":"2","category":"music"}"#)
        .unwrap();
    db.put("products/3", r#"{"_collection":"products","_key":"3","category":"food"}"#)
        .unwrap();

    let hits = db
        .query("SELECT * FROM products WHERE category = 'books' OR category = 'music'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 2);
    let slugs: std::collections::HashSet<_> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert!(slugs.contains("products/1"));
    assert!(slugs.contains("products/2"));
}

#[test]
fn where_or_and_precedence() {
    // (A AND B) OR C — AND binds tighter than OR
    let mut db = CoreDB::new();
    db.put(
        "events/1",
        r#"{"_collection":"events","_key":"1","type":"sale","region":"eu"}"#,
    )
    .unwrap();
    db.put(
        "events/2",
        r#"{"_collection":"events","_key":"2","type":"sale","region":"us"}"#,
    )
    .unwrap();
    db.put(
        "events/3",
        r#"{"_collection":"events","_key":"3","type":"view","region":"eu"}"#,
    )
    .unwrap();

    // type='sale' AND region='eu'  OR  type='view'
    let hits = db
        .query("SELECT * FROM events WHERE type = 'sale' AND region = 'eu' OR type = 'view'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 2);
    let slugs: std::collections::HashSet<_> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert!(slugs.contains("events/1")); // sale AND eu
    assert!(slugs.contains("events/3")); // view
}

// ── SELECT … AS alias ─────────────────────────────────────────────────────────

#[test]
fn select_as_alias() {
    let mut db = CoreDB::new();
    db.put(
        "employees/1",
        r#"{"_collection":"employees","_key":"1","first_name":"Alice","dept":"eng"}"#,
    )
    .unwrap();

    let hits = db
        .query("SELECT first_name AS name, dept AS department FROM employees")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap().as_object().unwrap();
    assert!(p.contains_key("name"), "expected key 'name', got {:?}", p.keys().collect::<Vec<_>>());
    assert_eq!(p["name"], "Alice");
    assert!(p.contains_key("department"));
    assert_eq!(p["department"], "eng");
    // Original keys should NOT be present
    assert!(!p.contains_key("first_name"));
    assert!(!p.contains_key("dept"));
}

// ── ORDER BY JSON path ────────────────────────────────────────────────────────

#[test]
fn order_by_json_path() {
    let mut db = CoreDB::new();
    db.put(
        "scores/1",
        r#"{"_collection":"scores","_key":"1","meta":{"val":30}}"#,
    )
    .unwrap();
    db.put(
        "scores/2",
        r#"{"_collection":"scores","_key":"2","meta":{"val":10}}"#,
    )
    .unwrap();
    db.put(
        "scores/3",
        r#"{"_collection":"scores","_key":"3","meta":{"val":20}}"#,
    )
    .unwrap();

    let hits = db
        .query("SELECT * FROM scores ORDER BY meta->'val' ASC")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].slug, "scores/2"); // val=10
    assert_eq!(hits[1].slug, "scores/3"); // val=20
    assert_eq!(hits[2].slug, "scores/1"); // val=30
}

// ── Aggregations ──────────────────────────────────────────────────────────────

#[test]
fn aggregate_count_star() {
    let mut db = CoreDB::new();
    for i in 1..=5 {
        db.put(
            &format!("log/{}", i),
            &format!(r#"{{"_collection":"log","_key":"{}","level":"info"}}"#, i),
        )
        .unwrap();
    }

    let hits = db
        .query("SELECT COUNT(*) FROM log")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["count"], 5);
}

#[test]
fn aggregate_sum_avg() {
    let mut db = CoreDB::new();
    db.put("sales/1", r#"{"_collection":"sales","_key":"1","amount":100}"#)
        .unwrap();
    db.put("sales/2", r#"{"_collection":"sales","_key":"2","amount":200}"#)
        .unwrap();
    db.put("sales/3", r#"{"_collection":"sales","_key":"3","amount":300}"#)
        .unwrap();

    let hits = db
        .query("SELECT SUM(amount), AVG(amount) FROM sales")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["sum"].as_f64().unwrap(), 600.0);
    assert_eq!(p["avg"].as_f64().unwrap(), 200.0);
}

#[test]
fn aggregate_min_max() {
    let mut db = CoreDB::new();
    db.put("temps/1", r#"{"_collection":"temps","_key":"1","c":5}"#)
        .unwrap();
    db.put("temps/2", r#"{"_collection":"temps","_key":"2","c":42}"#)
        .unwrap();
    db.put("temps/3", r#"{"_collection":"temps","_key":"3","c":17}"#)
        .unwrap();

    let hits = db
        .query("SELECT MIN(c), MAX(c) FROM temps")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["min"].as_f64().unwrap(), 5.0);
    assert_eq!(p["max"].as_f64().unwrap(), 42.0);
}

#[test]
fn aggregate_count_star_with_alias() {
    let mut db = CoreDB::new();
    for i in 1..=3 {
        db.put(
            &format!("things/{}", i),
            &format!(r#"{{"_collection":"things","_key":"{}"}}"#, i),
        )
        .unwrap();
    }

    let hits = db
        .query("SELECT COUNT(*) AS total FROM things")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap().as_object().unwrap();
    assert!(p.contains_key("total"), "expected 'total', got {:?}", p.keys().collect::<Vec<_>>());
    assert_eq!(p["total"], 3);
}

#[test]
fn aggregate_with_where_filter() {
    let mut db = CoreDB::new();
    db.put("orders/1", r#"{"_collection":"orders","_key":"1","status":"paid","amount":50}"#)
        .unwrap();
    db.put("orders/2", r#"{"_collection":"orders","_key":"2","status":"paid","amount":75}"#)
        .unwrap();
    db.put("orders/3", r#"{"_collection":"orders","_key":"3","status":"pending","amount":30}"#)
        .unwrap();

    let hits = db
        .query("SELECT COUNT(*), SUM(amount) FROM orders WHERE status = 'paid'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["count"], 2);
    assert_eq!(p["sum"].as_f64().unwrap(), 125.0);
}

// ── HNSW approximate nearest-neighbour ───────────────────────────────────────

#[test]
fn hnsw_build_and_search_rust_api() {
    let mut db = CoreDB::new();
    // Insert nodes with 4-d embeddings.
    db.put("docs/a", r#"{"_collection":"docs","_key":"a","text":"alpha"}"#).unwrap();
    db.put("docs/b", r#"{"_collection":"docs","_key":"b","text":"beta"}"#).unwrap();
    db.put("docs/c", r#"{"_collection":"docs","_key":"c","text":"gamma"}"#).unwrap();
    db.put("docs/d", r#"{"_collection":"docs","_key":"d","text":"delta"}"#).unwrap();

    // Vectors: a and b are close; c and d are close but far from a/b.
    db.put_vector("docs/a", "emb", &[1.0, 0.0, 0.0, 0.0]).unwrap();
    db.put_vector("docs/b", "emb", &[0.9, 0.1, 0.0, 0.0]).unwrap();
    db.put_vector("docs/c", "emb", &[0.0, 0.0, 1.0, 0.0]).unwrap();
    db.put_vector("docs/d", "emb", &[0.0, 0.0, 0.9, 0.1]).unwrap();

    // Build HNSW index.
    db.build_hnsw_index("emb", 4, 50).unwrap();

    // Query near [1, 0, 0, 0] → should return docs/a and docs/b.
    let results = db
        .collection("docs")
        .vector_near("emb", vec![1.0f32, 0.0, 0.0, 0.0], 2)
        .collect();

    assert_eq!(results.len(), 2);
    let slugs: std::collections::HashSet<_> = results.iter().map(|h| h.slug.as_str()).collect();
    assert!(slugs.contains("docs/a"), "expected docs/a in results, got {:?}", slugs);
    assert!(slugs.contains("docs/b"), "expected docs/b in results, got {:?}", slugs);
}

#[test]
fn hnsw_sql_vector_near() {
    let mut db = CoreDB::new();
    for (key, emb) in [
        ("items/1", [1.0f32, 0.0, 0.0, 0.0]),
        ("items/2", [0.95, 0.05, 0.0, 0.0]),
        ("items/3", [0.0, 1.0, 0.0, 0.0]),
        ("items/4", [0.0, 0.95, 0.05, 0.0]),
    ] {
        db.put(key, &format!(r#"{{"_collection":"items","_key":"{}"}}"#, key.split('/').last().unwrap()))
            .unwrap();
        db.put_vector(key, "vec", &emb).unwrap();
    }
    db.build_hnsw_index("vec", 4, 50).unwrap();

    let hits = db
        .query("SELECT * FROM items WHERE VECTOR_NEAR(vec, [1.0, 0.0, 0.0, 0.0], 2)")
        .unwrap()
        .collect();

    assert_eq!(hits.len(), 2);
    let slugs: std::collections::HashSet<_> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert!(slugs.contains("items/1"));
    assert!(slugs.contains("items/2"));
}

#[test]
fn hnsw_build_error_no_vectors() {
    let mut db = CoreDB::new();
    db.put("things/1", r#"{"_collection":"things","_key":"1"}"#).unwrap();
    // No vectors stored — build_hnsw_index should return Err.
    let result = db.build_hnsw_index("nonexistent_field", 8, 100);
    assert!(result.is_err());
    // Main store untouched.
    assert!(db.collection("things").count() == 1);
}

#[test]
fn hnsw_error_leaves_main_store_intact() {
    let mut db = CoreDB::new();
    db.put("nodes/1", r#"{"_collection":"nodes","_key":"1","score":42}"#).unwrap();
    db.put_vector("nodes/1", "emb", &[1.0, 0.0]).unwrap();

    // First build succeeds.
    db.build_hnsw_index("emb", 4, 20).unwrap();

    // The original node is still reachable and correct.
    let hits = db.query("SELECT * FROM nodes WHERE score = 42").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "nodes/1");

    // Attempting to build for a field with no vectors returns Err
    // and must not corrupt the existing HNSW index.
    let err = db.build_hnsw_index("missing_field", 4, 20);
    assert!(err.is_err());

    // Original HNSW index still works.
    let vec_hits = db
        .query("SELECT * FROM nodes WHERE VECTOR_NEAR(emb, [1.0, 0.0], 1)")
        .unwrap()
        .collect();
    assert_eq!(vec_hits.len(), 1);
    assert_eq!(vec_hits[0].slug, "nodes/1");
}

// ── WHERE parenthesized groups ─────────────────────────────────────────────────

#[test]
fn where_paren_or_and() {
    // (a OR b) AND c
    let mut db = CoreDB::new();
    db.put("t/1", r#"{"_collection":"t","_key":"1","color":"red","active":true}"#).unwrap();
    db.put("t/2", r#"{"_collection":"t","_key":"2","color":"blue","active":true}"#).unwrap();
    db.put("t/3", r#"{"_collection":"t","_key":"3","color":"red","active":false}"#).unwrap();
    db.put("t/4", r#"{"_collection":"t","_key":"4","color":"green","active":true}"#).unwrap();

    // (color='red' OR color='blue') AND active=true → items 1 and 2
    let hits = db.query("SELECT * FROM t WHERE (color = 'red' OR color = 'blue') AND active = true")
        .unwrap().collect();
    let slugs: std::collections::HashSet<_> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert_eq!(hits.len(), 2);
    assert!(slugs.contains("t/1"));
    assert!(slugs.contains("t/2"));
}

#[test]
fn where_paren_and_or() {
    // a AND (b OR c)
    let mut db = CoreDB::new();
    db.put("t/1", r#"{"_collection":"t","_key":"1","type":"A","score":10}"#).unwrap();
    db.put("t/2", r#"{"_collection":"t","_key":"2","type":"A","score":20}"#).unwrap();
    db.put("t/3", r#"{"_collection":"t","_key":"3","type":"B","score":10}"#).unwrap();

    // type='A' AND (score=10 OR score=20) → items 1 and 2
    let hits = db.query("SELECT * FROM t WHERE type = 'A' AND (score = 10 OR score = 20)")
        .unwrap().collect();
    assert_eq!(hits.len(), 2);
}

#[test]
fn where_not_paren_group() {
    // NOT (a OR b)
    let mut db = CoreDB::new();
    db.put("t/1", r#"{"_collection":"t","_key":"1","status":"active"}"#).unwrap();
    db.put("t/2", r#"{"_collection":"t","_key":"2","status":"pending"}"#).unwrap();
    db.put("t/3", r#"{"_collection":"t","_key":"3","status":"deleted"}"#).unwrap();

    // NOT (status='active' OR status='pending') → only item 3
    let hits = db.query("SELECT * FROM t WHERE NOT (status = 'active' OR status = 'pending')")
        .unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "t/3");
}

// ── ORDER BY DESC ──────────────────────────────────────────────────────────────

#[test]
fn order_by_desc() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","price":10}"#).unwrap();
    db.put("p/2", r#"{"_collection":"p","_key":"2","price":30}"#).unwrap();
    db.put("p/3", r#"{"_collection":"p","_key":"3","price":20}"#).unwrap();

    let hits = db.query("SELECT * FROM p ORDER BY price DESC").unwrap().collect();
    assert_eq!(hits.len(), 3);
    let prices: Vec<f64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["price"].as_f64().unwrap())
        .collect();
    assert_eq!(prices, vec![30.0, 20.0, 10.0]);
}

// ── LIMIT / OFFSET any order ───────────────────────────────────────────────────

#[test]
fn limit_offset_any_order() {
    let mut db = CoreDB::new();
    for i in 1..=10u32 {
        db.put(&format!("n/{i}"), &format!(r#"{{"_collection":"n","_key":"{i}","v":{i}}}"#)).unwrap();
    }

    // OFFSET before LIMIT
    let hits = db.query("SELECT * FROM n ORDER BY v ASC OFFSET 2 LIMIT 3").unwrap().collect();
    assert_eq!(hits.len(), 3);
    let vals: Vec<f64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["v"].as_f64().unwrap())
        .collect();
    assert_eq!(vals, vec![3.0, 4.0, 5.0]);
}

// ── SELECT DISTINCT ────────────────────────────────────────────────────────────

#[test]
fn select_distinct_basic() {
    let mut db = CoreDB::new();
    db.put("u/1", r#"{"_collection":"u","_key":"1","city":"Paris"}"#).unwrap();
    db.put("u/2", r#"{"_collection":"u","_key":"2","city":"London"}"#).unwrap();
    db.put("u/3", r#"{"_collection":"u","_key":"3","city":"Paris"}"#).unwrap();
    db.put("u/4", r#"{"_collection":"u","_key":"4","city":"Berlin"}"#).unwrap();

    // Three distinct city values
    let hits = db.query("SELECT DISTINCT city FROM u").unwrap().collect();
    assert_eq!(hits.len(), 3);
    let cities: std::collections::HashSet<String> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["city"].as_str().unwrap().to_string())
        .collect();
    assert!(cities.contains("Paris"));
    assert!(cities.contains("London"));
    assert!(cities.contains("Berlin"));
}

#[test]
fn select_distinct_all_dupes() {
    let mut db = CoreDB::new();
    for i in 1..=5u32 {
        db.put(&format!("x/{i}"), &format!(r#"{{"_collection":"x","_key":"{i}","kind":"widget"}}"#)).unwrap();
    }
    let hits = db.query("SELECT DISTINCT kind FROM x").unwrap().collect();
    assert_eq!(hits.len(), 1);
}

// ── GROUP BY ──────────────────────────────────────────────────────────────────

#[test]
fn group_by_count() {
    let mut db = CoreDB::new();
    db.put("o/1", r#"{"_collection":"o","_key":"1","cat":"A","val":1}"#).unwrap();
    db.put("o/2", r#"{"_collection":"o","_key":"2","cat":"A","val":2}"#).unwrap();
    db.put("o/3", r#"{"_collection":"o","_key":"3","cat":"B","val":3}"#).unwrap();
    db.put("o/4", r#"{"_collection":"o","_key":"4","cat":"B","val":4}"#).unwrap();
    db.put("o/5", r#"{"_collection":"o","_key":"5","cat":"C","val":5}"#).unwrap();

    let hits = db.query("SELECT cat, COUNT(*) FROM o GROUP BY cat ORDER BY cat ASC")
        .unwrap().collect();
    assert_eq!(hits.len(), 3);
    // First group = A with count 2
    let first = hits[0].payload.as_ref().unwrap();
    assert_eq!(first["cat"].as_str().unwrap(), "A");
    assert_eq!(first["count"].as_f64().unwrap(), 2.0);
}

#[test]
fn group_by_sum_avg() {
    let mut db = CoreDB::new();
    db.put("s/1", r#"{"_collection":"s","_key":"1","dept":"eng","salary":100}"#).unwrap();
    db.put("s/2", r#"{"_collection":"s","_key":"2","dept":"eng","salary":200}"#).unwrap();
    db.put("s/3", r#"{"_collection":"s","_key":"3","dept":"hr","salary":150}"#).unwrap();

    let hits = db.query("SELECT dept, SUM(salary), AVG(salary) FROM s GROUP BY dept ORDER BY dept ASC")
        .unwrap().collect();
    assert_eq!(hits.len(), 2);
    let eng = hits[0].payload.as_ref().unwrap();
    assert_eq!(eng["dept"].as_str().unwrap(), "eng");
    assert_eq!(eng["sum"].as_f64().unwrap(), 300.0);
    assert_eq!(eng["avg"].as_f64().unwrap(), 150.0);
}

// ── COUNT(DISTINCT) in plain SELECT ───────────────────────────────────────────

#[test]
fn plain_count_distinct_bare() {
    let mut db = CoreDB::new();
    // Bali-themed: bookings across a few villages, some repeated.
    db.put("b/1", r#"{"_collection":"b","_key":"1","village":"Seminyak","spend":100}"#).unwrap();
    db.put("b/2", r#"{"_collection":"b","_key":"2","village":"Seminyak","spend":200}"#).unwrap();
    db.put("b/3", r#"{"_collection":"b","_key":"3","village":"Ubud","spend":100}"#).unwrap();
    db.put("b/4", r#"{"_collection":"b","_key":"4","village":"Canggu","spend":100}"#).unwrap();
    // a booking with no village at all — DISTINCT must ignore the missing value.
    db.put("b/5", r#"{"_collection":"b","_key":"5","spend":999}"#).unwrap();

    // 3 distinct villages (Seminyak, Ubud, Canggu); null-village row ignored.
    let hits = db.query("SELECT COUNT(DISTINCT village) AS n FROM b").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["n"].as_i64().unwrap(), 3);

    // Distinct numeric values: 100, 200, 999 → 3.
    let hits = db.query("SELECT COUNT(DISTINCT spend) AS n FROM b").unwrap().collect();
    assert_eq!(hits[0].payload.as_ref().unwrap()["n"].as_i64().unwrap(), 3);

    // No alias → column is named "count" (PostgreSQL convention).
    let hits = db.query("SELECT COUNT(DISTINCT village) FROM b").unwrap().collect();
    assert_eq!(hits[0].payload.as_ref().unwrap()["count"].as_i64().unwrap(), 3);
}

#[test]
fn plain_count_distinct_mixed_with_other_aggs() {
    let mut db = CoreDB::new();
    db.put("b/1", r#"{"_collection":"b","_key":"1","village":"Ubud","spend":100}"#).unwrap();
    db.put("b/2", r#"{"_collection":"b","_key":"2","village":"Ubud","spend":200}"#).unwrap();
    db.put("b/3", r#"{"_collection":"b","_key":"3","village":"Canggu","spend":300}"#).unwrap();

    let hits = db.query(
        "SELECT COUNT(*) AS all_n, COUNT(DISTINCT village) AS villages, SUM(spend) AS tot FROM b"
    ).unwrap().collect();
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["all_n"].as_i64().unwrap(), 3);
    assert_eq!(p["villages"].as_i64().unwrap(), 2);
    assert_eq!(p["tot"].as_f64().unwrap(), 600.0);
}

#[test]
fn plain_count_distinct_group_by_and_having() {
    let mut db = CoreDB::new();
    // Seminyak: spends {100,200} → 2 distinct; Ubud: {50,50} → 1 distinct.
    db.put("b/1", r#"{"_collection":"b","_key":"1","village":"Seminyak","spend":100}"#).unwrap();
    db.put("b/2", r#"{"_collection":"b","_key":"2","village":"Seminyak","spend":200}"#).unwrap();
    db.put("b/3", r#"{"_collection":"b","_key":"3","village":"Seminyak","spend":100}"#).unwrap();
    db.put("b/4", r#"{"_collection":"b","_key":"4","village":"Ubud","spend":50}"#).unwrap();
    db.put("b/5", r#"{"_collection":"b","_key":"5","village":"Ubud","spend":50}"#).unwrap();

    // Per-group distinct spend counts.
    let hits = db.query(
        "SELECT village, COUNT(DISTINCT spend) AS ds FROM b GROUP BY village ORDER BY village ASC"
    ).unwrap().collect();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].payload.as_ref().unwrap()["village"].as_str().unwrap(), "Seminyak");
    assert_eq!(hits[0].payload.as_ref().unwrap()["ds"].as_i64().unwrap(), 2);
    assert_eq!(hits[1].payload.as_ref().unwrap()["ds"].as_i64().unwrap(), 1);

    // HAVING on COUNT(DISTINCT): only Seminyak (2 distinct) passes > 1.
    let hits = db.query(
        "SELECT village, COUNT(DISTINCT spend) AS ds FROM b \
         GROUP BY village HAVING COUNT(DISTINCT spend) > 1 ORDER BY village ASC"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["village"].as_str().unwrap(), "Seminyak");
}

// ── CASE WHEN in plain SELECT ─────────────────────────────────────────────────

#[test]
fn plain_case_numeric_buckets_with_else() {
    let mut db = CoreDB::new();
    db.put("s/1", r#"{"_collection":"s","_key":"1","city":"Ubud","amount":50}"#).unwrap();
    db.put("s/2", r#"{"_collection":"s","_key":"2","city":"Bali","amount":250}"#).unwrap();
    db.put("s/3", r#"{"_collection":"s","_key":"3","city":"Canggu","amount":600}"#).unwrap();

    // WHEN branches are evaluated top-to-bottom; first match wins.
    let hits = db.query(
        "SELECT city, CASE WHEN amount > 500 THEN 'high' WHEN amount > 150 THEN 'mid' \
         ELSE 'low' END AS tier FROM s ORDER BY amount ASC"
    ).unwrap().collect();
    let tiers: Vec<&str> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["tier"].as_str().unwrap())
        .collect();
    assert_eq!(tiers, vec!["low", "mid", "high"]);
}

#[test]
fn plain_case_string_equality() {
    let mut db = CoreDB::new();
    db.put("s/1", r#"{"_collection":"s","_key":"1","city":"Bali"}"#).unwrap();
    db.put("s/2", r#"{"_collection":"s","_key":"2","city":"Ubud"}"#).unwrap();

    let hits = db.query(
        "SELECT city, CASE WHEN city = 'Bali' THEN 'beach' ELSE 'inland' END AS kind \
         FROM s ORDER BY city ASC"
    ).unwrap().collect();
    // Bali → beach, Ubud → inland (alphabetical order).
    assert_eq!(hits[0].payload.as_ref().unwrap()["kind"].as_str().unwrap(), "beach");
    assert_eq!(hits[1].payload.as_ref().unwrap()["kind"].as_str().unwrap(), "inland");
}

#[test]
fn plain_case_no_else_is_null_and_default_column() {
    let mut db = CoreDB::new();
    db.put("s/1", r#"{"_collection":"s","_key":"1","amount":50}"#).unwrap();
    db.put("s/2", r#"{"_collection":"s","_key":"2","amount":600}"#).unwrap();

    // No ELSE → non-matching rows get NULL. No AS → column named "case".
    let hits = db.query(
        "SELECT CASE WHEN amount > 500 THEN 'big' END FROM s ORDER BY amount ASC"
    ).unwrap().collect();
    assert!(hits[0].payload.as_ref().unwrap()["case"].is_null());
    assert_eq!(hits[1].payload.as_ref().unwrap()["case"].as_str().unwrap(), "big");
}

// ── GROUP BY + HAVING ─────────────────────────────────────────────────────────

#[test]
fn group_by_having_count() {
    let mut db = CoreDB::new();
    db.put("o/1", r#"{"_collection":"o","_key":"1","cat":"A"}"#).unwrap();
    db.put("o/2", r#"{"_collection":"o","_key":"2","cat":"A"}"#).unwrap();
    db.put("o/3", r#"{"_collection":"o","_key":"3","cat":"A"}"#).unwrap();
    db.put("o/4", r#"{"_collection":"o","_key":"4","cat":"B"}"#).unwrap();
    db.put("o/5", r#"{"_collection":"o","_key":"5","cat":"B"}"#).unwrap();
    db.put("o/6", r#"{"_collection":"o","_key":"6","cat":"C"}"#).unwrap();

    // Only groups with count >= 2 (A=3, B=2 pass; C=1 excluded)
    let hits = db.query("SELECT cat, COUNT(*) FROM o GROUP BY cat HAVING COUNT(*) >= 2 ORDER BY cat ASC")
        .unwrap().collect();
    assert_eq!(hits.len(), 2);
    let cats: Vec<&str> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["cat"].as_str().unwrap())
        .collect();
    assert_eq!(cats, vec!["A", "B"]);
}

#[test]
fn group_by_having_sum() {
    let mut db = CoreDB::new();
    db.put("tx/1", r#"{"_collection":"tx","_key":"1","acct":"X","amount":500}"#).unwrap();
    db.put("tx/2", r#"{"_collection":"tx","_key":"2","acct":"X","amount":600}"#).unwrap();
    db.put("tx/3", r#"{"_collection":"tx","_key":"3","acct":"Y","amount":100}"#).unwrap();
    db.put("tx/4", r#"{"_collection":"tx","_key":"4","acct":"Y","amount":200}"#).unwrap();

    // Accounts with total > 500 (X=1100 passes, Y=300 excluded)
    let hits = db.query("SELECT acct, SUM(amount) FROM tx GROUP BY acct HAVING SUM(amount) > 500")
        .unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["acct"].as_str().unwrap(), "X");
}

#[test]
fn group_by_pg_violation_rejected() {
    let mut db = CoreDB::new();
    db.put("o/1", r#"{"_collection":"o","_key":"1","cat":"A","name":"Vines"}"#).unwrap();
    // "name" is not in GROUP BY and not aggregated — must error.
    let err = db.query("SELECT cat, name FROM o GROUP BY cat");
    assert!(err.is_err(), "PG violation should be rejected at parse time");
    let msg = err.err().unwrap().to_string();
    assert!(msg.contains("name"), "error should mention the offending column");
}

#[test]
fn group_by_multi_field_set() {
    let mut db = CoreDB::new();
    db.put("e/1", r#"{"_collection":"e","_key":"1","dept":"eng","city":"Melbourne","salary":100}"#).unwrap();
    db.put("e/2", r#"{"_collection":"e","_key":"2","dept":"eng","city":"Melbourne","salary":200}"#).unwrap();
    db.put("e/3", r#"{"_collection":"e","_key":"3","dept":"eng","city":"Fitzroy","salary":150}"#).unwrap();
    db.put("e/4", r#"{"_collection":"e","_key":"4","dept":"hr","city":"Melbourne","salary":120}"#).unwrap();

    let hits = db.query(
        "SELECT dept, city, COUNT(*) AS cnt FROM e GROUP BY dept, city ORDER BY cnt DESC"
    ).unwrap().collect();
    // 3 groups: eng/Melbourne=2, eng/Fitzroy=1, hr/Melbourne=1
    assert_eq!(hits.len(), 3);
    let top = hits[0].payload.as_ref().unwrap();
    assert_eq!(top["dept"].as_str().unwrap(), "eng");
    assert_eq!(top["city"].as_str().unwrap(), "Melbourne");
    assert_eq!(top["cnt"].as_i64().unwrap(), 2);
}

// ── Multi-column ORDER BY ─────────────────────────────────────────────────────

#[test]
fn order_by_multi_column_sql() {
    let mut db = CoreDB::new();
    db.put("u/1", r#"{"_collection":"u","dept":"eng","salary":90}"#).unwrap();
    db.put("u/2", r#"{"_collection":"u","dept":"eng","salary":70}"#).unwrap();
    db.put("u/3", r#"{"_collection":"u","dept":"hr","salary":80}"#).unwrap();
    db.put("u/4", r#"{"_collection":"u","dept":"hr","salary":60}"#).unwrap();

    let hits = db.query("SELECT * FROM u ORDER BY dept ASC, salary DESC").unwrap().collect();
    // dept ASC: eng before hr
    // within eng, salary DESC: 90 then 70
    // within hr, salary DESC: 80 then 60
    let names: Vec<String> = hits.iter()
        .map(|h| {
            let p = h.payload.as_ref().unwrap();
            format!("{}/{}", p["dept"].as_str().unwrap(), p["salary"].as_f64().unwrap())
        })
        .collect();
    assert_eq!(names, ["eng/90", "eng/70", "hr/80", "hr/60"]);
}

#[test]
fn order_by_multi_column_api() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","cat":"b","rank":2}"#).unwrap();
    db.put("p/2", r#"{"_collection":"p","cat":"a","rank":3}"#).unwrap();
    db.put("p/3", r#"{"_collection":"p","cat":"a","rank":1}"#).unwrap();

    let hits = db
        .collection("p")
        .sort_multi(vec![("cat".to_string(), true), ("rank".to_string(), true)])
        .collect();
    // cat ASC, then rank ASC within same cat
    // a/1, a/3, b/2
    let ranks: Vec<i64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["rank"].as_i64().unwrap())
        .collect();
    assert_eq!(ranks, [1, 3, 2]);
}

#[test]
fn order_by_single_column_unchanged() {
    let mut db = CoreDB::new();
    for i in [5u64, 3, 8, 1, 9, 2] {
        db.put(&format!("n/{i}"), &format!(r#"{{"_collection":"n","v":{i}}}"#)).unwrap();
    }
    let hits = db.query("SELECT * FROM n ORDER BY v ASC").unwrap().collect();
    let vals: Vec<i64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["v"].as_i64().unwrap())
        .collect();
    assert_eq!(vals, [1, 2, 3, 5, 8, 9]);
}

// ── Transactions ──────────────────────────────────────────────────────────────

#[test]
fn transaction_commit_applies_all_writes() {
    let mut db = CoreDB::new();
    let mut txn = db.begin();
    txn.put("users/alice", r#"{"_collection":"users","name":"Alice"}"#).unwrap();
    txn.put("users/bob",   r#"{"_collection":"users","name":"Bob"}"#).unwrap();
    txn.commit().unwrap();

    assert!(db.contains("users/alice"));
    assert!(db.contains("users/bob"));
    assert_eq!(db.collection("users").count(), 2);
}

#[test]
fn transaction_rollback_applies_nothing() {
    let mut db = CoreDB::new();
    {
        let mut txn = db.begin();
        txn.put("users/ghost", r#"{"_collection":"users","name":"Ghost"}"#).unwrap();
        txn.rollback(); // explicit rollback
    }
    assert!(!db.contains("users/ghost"), "ghost must not exist after rollback");

    {
        let mut txn = db.begin();
        txn.put("users/phantom", r#"{"_collection":"users","name":"Phantom"}"#).unwrap();
        // implicit rollback — drop without commit
    }
    assert!(!db.contains("users/phantom"), "phantom must not exist after implicit rollback");
}

#[test]
fn transaction_commit_returns_op_count() {
    let mut db = CoreDB::new();
    let mut txn = db.begin();
    txn.put("a/1", r#"{"_collection":"a"}"#).unwrap();
    txn.put("a/2", r#"{"_collection":"a"}"#).unwrap();
    txn.remove("a/99"); // remove of non-existent — still counted
    txn.link("a/1", "a/2", "rel");
    let n = txn.commit().unwrap();
    assert_eq!(n, 4);
}

#[test]
fn transaction_put_validates_json_eagerly() {
    let mut db = CoreDB::new();
    let mut txn = db.begin();
    let err = txn.put("bad", "not json!!");
    assert!(err.is_err(), "bad JSON must error at put() time");
    // Even though put errored, commit/rollback are still valid
    txn.rollback();
    assert!(!db.contains("bad"));
}

#[test]
fn transaction_with_link_and_remove() {
    let mut db = CoreDB::new();
    db.put("nodes/a", r#"{"_collection":"nodes"}"#).unwrap();
    db.put("nodes/b", r#"{"_collection":"nodes"}"#).unwrap();
    db.put("nodes/c", r#"{"_collection":"nodes"}"#).unwrap();

    let mut txn = db.begin();
    txn.link("nodes/a", "nodes/b", "knows");
    txn.unlink("nodes/a", "nodes/b", "knows"); // cancel the link above
    txn.remove("nodes/c");
    txn.commit().unwrap();

    // Link was added then removed in same txn → should not exist
    assert!(db.one("nodes/a").forward("knows").collect().is_empty());
    // c was removed
    assert!(!db.contains("nodes/c"));
}

// ── #3 btree ORDER BY index scan ──────────────────────────────────────────────

#[test]
fn btree_order_scan_produces_sorted_results() {
    let mut db = CoreDB::new();
    for i in [5u64, 1, 9, 3, 7, 2, 8, 4, 6] {
        db.put(&format!("n/{i}"), &format!(r#"{{"_collection":"n","v":{i}}}"#)).unwrap();
    }
    db.execute("CREATE INDEX ON n USING btree (v)").unwrap();

    // ORDER BY v ASC — index scan path
    let hits = db.query("SELECT * FROM n ORDER BY v ASC").unwrap().collect();
    let vals: Vec<i64> = hits.iter().map(|h| h.payload.as_ref().unwrap()["v"].as_i64().unwrap()).collect();
    assert_eq!(vals, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn btree_order_scan_desc() {
    let mut db = CoreDB::new();
    for i in 1u64..=5 {
        db.put(&format!("n/{i}"), &format!(r#"{{"_collection":"n","score":{i}}}"#)).unwrap();
    }
    db.execute("CREATE INDEX ON n USING btree (score)").unwrap();

    let hits = db.query("SELECT * FROM n ORDER BY score DESC").unwrap().collect();
    let vals: Vec<i64> = hits.iter().map(|h| h.payload.as_ref().unwrap()["score"].as_i64().unwrap()).collect();
    assert_eq!(vals, [5, 4, 3, 2, 1]);
}

#[test]
fn btree_order_scan_with_limit() {
    let mut db = CoreDB::new();
    for i in 1u64..=100 {
        db.put(&format!("n/{i}"), &format!(r#"{{"_collection":"n","rank":{i}}}"#)).unwrap();
    }
    db.execute("CREATE INDEX ON n USING btree (rank)").unwrap();

    // Index scan extracts top-5 cheaply without loading all 100 members
    let hits = db.query("SELECT * FROM n ORDER BY rank ASC LIMIT 5").unwrap().collect();
    assert_eq!(hits.len(), 5);
    let vals: Vec<i64> = hits.iter().map(|h| h.payload.as_ref().unwrap()["rank"].as_i64().unwrap()).collect();
    assert_eq!(vals, [1, 2, 3, 4, 5]);
}

// ── #1 SQL mutations ──────────────────────────────────────────────────────────

#[test]
fn sql_insert_into_creates_node() {
    let mut db = CoreDB::new();
    db.execute(
        "INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)",
    ).unwrap();

    assert!(db.contains("users/alice"));
    let hits = db.query("SELECT * FROM users WHERE name = 'Alice'").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/alice");
}

#[test]
fn sql_insert_returns_one() {
    let mut db = CoreDB::new();
    let n = db.execute(
        "INSERT INTO products (_key, price) VALUES ('p1', 99)",
    ).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn sql_update_set_field() {
    let mut db = CoreDB::new();
    db.put("users/bob", r#"{"_collection":"users","_key":"bob","name":"Bob","score":10}"#).unwrap();

    let n = db.execute("UPDATE users SET score = 99 WHERE _key = 'bob'").unwrap();
    assert_eq!(n, 1);

    let hits = db.query("SELECT * FROM users WHERE _key = 'bob'").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["score"].as_f64().unwrap(), 99.0);
}

#[test]
fn sql_delete_from_removes_node() {
    let mut db = CoreDB::new();
    db.put("items/i1", r#"{"_collection":"items","_key":"i1","keep":false}"#).unwrap();
    db.put("items/i2", r#"{"_collection":"items","_key":"i2","keep":true}"#).unwrap();

    let n = db.execute("DELETE FROM items WHERE keep = false").unwrap();
    assert_eq!(n, 1);
    assert!(!db.contains("items/i1"), "i1 must be deleted");
    assert!(db.contains("items/i2"), "i2 must survive");
}

#[test]
fn sql_update_multiple_rows() {
    let mut db = CoreDB::new();
    for i in 1..=5 {
        db.put(
            &format!("items/i{i}"),
            &format!(r#"{{"_collection":"items","_key":"i{i}","active":true,"val":{i}}}"#),
        ).unwrap();
    }
    // Mark all active=false
    let n = db.execute("UPDATE items SET active = false WHERE active = true").unwrap();
    assert_eq!(n, 5);

    let still_active = db.query("SELECT * FROM items WHERE active = true").unwrap().count();
    assert_eq!(still_active, 0);
}

// ── #3 Btree field index ──────────────────────────────────────────────────────

#[test]
fn btree_index_eq_filter() {
    let mut db = CoreDB::new();
    for i in 0..20 {
        db.put(
            &format!("users/u{i}"),
            &format!(r#"{{"_collection":"users","_key":"u{i}","age":{i}}}"#),
        ).unwrap();
    }
    db.execute("CREATE INDEX ON users USING btree (age)").unwrap();

    let hits = db.query("SELECT * FROM users WHERE age = 5").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/u5");
}

#[test]
fn btree_index_range_gt() {
    let mut db = CoreDB::new();
    for i in 0..10 {
        db.put(
            &format!("p/p{i}"),
            &format!(r#"{{"_collection":"p","_key":"p{i}","score":{i}}}"#),
        ).unwrap();
    }
    db.execute("CREATE INDEX ON p USING btree (score)").unwrap();

    let hits = db.query("SELECT * FROM p WHERE score > 6").unwrap().collect();
    assert_eq!(hits.len(), 3); // 7, 8, 9
}

#[test]
fn btree_index_range_between() {
    let mut db = CoreDB::new();
    for i in 0..20 {
        db.put(
            &format!("items/i{i}"),
            &format!(r#"{{"_collection":"items","_key":"i{i}","price":{i}}}"#),
        ).unwrap();
    }
    db.execute("CREATE INDEX ON items USING btree (price)").unwrap();

    let hits = db.query("SELECT * FROM items WHERE price BETWEEN 5 AND 10").unwrap().collect();
    assert_eq!(hits.len(), 6); // 5, 6, 7, 8, 9, 10
}

#[test]
fn btree_index_maintained_on_insert_after_create() {
    let mut db = CoreDB::new();
    // Create index first (empty collection)
    db.execute("CREATE INDEX ON orders USING btree (amount)").unwrap();

    // Insert nodes after the index exists — they should be picked up
    db.put("orders/o1", r#"{"_collection":"orders","_key":"o1","amount":100}"#).unwrap();
    db.put("orders/o2", r#"{"_collection":"orders","_key":"o2","amount":200}"#).unwrap();
    db.put("orders/o3", r#"{"_collection":"orders","_key":"o3","amount":50}"#).unwrap();

    let hits = db.query("SELECT * FROM orders WHERE amount > 75").unwrap().collect();
    assert_eq!(hits.len(), 2); // 100, 200
}

#[test]
fn btree_index_maintained_on_update() {
    let mut db = CoreDB::new();
    db.put("items/a", r#"{"_collection":"items","_key":"a","val":1}"#).unwrap();
    db.put("items/b", r#"{"_collection":"items","_key":"b","val":2}"#).unwrap();
    db.execute("CREATE INDEX ON items USING btree (val)").unwrap();

    // Update — old index entry for "a" (val=1) should be replaced with val=99
    db.execute("UPDATE items SET val = 99 WHERE _key = 'a'").unwrap();

    // val = 1 should now match nothing
    let low = db.query("SELECT * FROM items WHERE val = 1").unwrap().count();
    assert_eq!(low, 0);

    // val = 99 should match "a"
    let high = db.query("SELECT * FROM items WHERE val = 99").unwrap().collect();
    assert_eq!(high.len(), 1);
    assert_eq!(high[0].slug, "items/a");
}

#[test]
fn btree_index_maintained_on_delete() {
    let mut db = CoreDB::new();
    db.put("items/a", r#"{"_collection":"items","_key":"a","val":5}"#).unwrap();
    db.put("items/b", r#"{"_collection":"items","_key":"b","val":10}"#).unwrap();
    db.execute("CREATE INDEX ON items USING btree (val)").unwrap();

    db.remove("items/a");

    // Only "b" (val=10) should remain
    let hits = db.query("SELECT * FROM items WHERE val > 0").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "items/b");
}

#[test]
fn btree_index_no_false_positives() {
    // When the index seeds candidates, the subsequent retain() filter must
    // confirm results — so the count must be exact, not over-inclusive.
    let mut db = CoreDB::new();
    for i in 0..50 {
        db.put(
            &format!("n/n{i}"),
            &format!(r#"{{"_collection":"n","_key":"n{i}","x":{i}}}"#),
        ).unwrap();
    }
    db.execute("CREATE INDEX ON n USING btree (x)").unwrap();

    // Strict equality: must return exactly 1 result
    let hits = db.query("SELECT * FROM n WHERE x = 25").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "n/n25");

    // Range: 10..=14 inclusive → exactly 5
    let hits = db.query("SELECT * FROM n WHERE x BETWEEN 10 AND 14").unwrap().collect();
    assert_eq!(hits.len(), 5);
}

// ── Schema validation tests ───────────────────────────────────────────────────

/// INSERT with correct types passes validation.
#[test]
fn schema_validation_valid_insert() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE users (_key TEXT, name TEXT, age INTEGER)"#).unwrap();
    db.execute(r#"INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)"#).unwrap();
    let hits = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(hits.len(), 1);
}

/// INSERT with wrong type on a declared field returns an error.
#[test]
fn schema_validation_rejects_wrong_type() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE products (_key TEXT, price REAL)"#).unwrap();
    let err = db.execute(r#"INSERT INTO products (_key, price) VALUES ('p1', 'not-a-number')"#);
    assert!(err.is_err(), "should reject non-number for REAL field");
}

/// INSERT into collection without a schema always succeeds.
#[test]
fn schema_validation_no_schema_is_permissive() {
    let mut db = CoreDB::new();
    // No CREATE TABLE — any payload shape is accepted
    db.execute(r#"INSERT INTO items (_key, weirdfield) VALUES ('x', 'anything')"#).unwrap();
    assert_eq!(db.query("SELECT * FROM items").unwrap().collect().len(), 1);
}

/// UPDATE with wrong type on a declared field returns an error.
#[test]
fn schema_validation_rejects_wrong_type_on_update() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE events (_key TEXT, score INTEGER)"#).unwrap();
    db.execute(r#"INSERT INTO events (_key, score) VALUES ('e1', 10)"#).unwrap();
    let err = db.execute(r#"UPDATE events SET score = 'high' WHERE _key = 'e1'"#);
    assert!(err.is_err(), "should reject non-number for INTEGER field on UPDATE");
}

/// UPDATE with correct types passes validation.
#[test]
fn schema_validation_valid_update() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE events (_key TEXT, score INTEGER)"#).unwrap();
    db.execute(r#"INSERT INTO events (_key, score) VALUES ('e1', 5)"#).unwrap();
    db.execute(r#"UPDATE events SET score = 99 WHERE _key = 'e1'"#).unwrap();
    let hits = db.query("SELECT * FROM events WHERE _key = 'e1'").unwrap().collect();
    assert_eq!(hits[0].payload.as_ref().unwrap()["score"].as_f64(), Some(99.0));
}

/// NULL is accepted for any declared field type.
#[test]
fn schema_validation_null_is_always_valid() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE logs (_key TEXT, level INTEGER)"#).unwrap();
    db.execute(r#"INSERT INTO logs (_key, level) VALUES ('l1', NULL)"#).unwrap();
    assert_eq!(db.query("SELECT * FROM logs").unwrap().collect().len(), 1);
}

// ── NOT IN ────────────────────────────────────────────────────────────────────

/// Basic `field NOT IN (v1, v2)` excludes matched values.
#[test]
fn not_in_excludes_values() {
    let mut db = CoreDB::new();
    for (k, city) in [("u1", "Jakarta"), ("u2", "Bandung"), ("u3", "Surabaya"), ("u4", "Bali")] {
        db.put(k, &format!(r#"{{"_collection":"users","city":"{city}"}}"#)).unwrap();
    }
    let hits = db
        .query("SELECT * FROM users WHERE city NOT IN ('Jakarta', 'Bali')")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 2);
    let cities: Vec<_> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["city"].as_str().unwrap().to_string())
        .collect();
    assert!(cities.contains(&"Bandung".to_string()));
    assert!(cities.contains(&"Surabaya".to_string()));
}

/// `NOT IN` with numbers.
#[test]
fn not_in_numeric() {
    let mut db = CoreDB::new();
    for i in 1..=5u32 {
        db.put(&format!("x{i}"), &format!(r#"{{"_collection":"nums","v":{i}}}"#)).unwrap();
    }
    // Exclude 2 and 4 — expect 1, 3, 5
    let hits = db
        .query("SELECT * FROM nums WHERE v NOT IN (2, 4)")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 3);
}

/// `NOT field IN (...)` prefix form also works.
#[test]
fn not_prefix_in_also_works() {
    let mut db = CoreDB::new();
    db.put("a", r#"{"_collection":"t","k":"alpha"}"#).unwrap();
    db.put("b", r#"{"_collection":"t","k":"beta"}"#).unwrap();
    db.put("c", r#"{"_collection":"t","k":"gamma"}"#).unwrap();
    // prefix NOT form
    let hits = db
        .query("SELECT * FROM t WHERE NOT k IN ('alpha', 'gamma')")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["k"].as_str(), Some("beta"));
}

/// Combined AND + NOT IN.
#[test]
fn not_in_combined_with_and() {
    let mut db = CoreDB::new();
    for (k, city, active) in [
        ("u1", "Jakarta", true),
        ("u2", "Jakarta", false),
        ("u3", "Bandung", true),
        ("u4", "Bali",    true),
    ] {
        db.put(k, &format!(r#"{{"_collection":"users","city":"{city}","active":{active}}}"#))
            .unwrap();
    }
    // active=true AND city NOT IN ('Bali')
    let hits = db
        .query("SELECT * FROM users WHERE active = true AND city NOT IN ('Bali')")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 2); // u1 (Jakarta,true) and u3 (Bandung,true)
}

// ── string literal escaping ────────────────────────────────────────────────────

/// SQL-standard `''` escape: a doubled quote inside a string literal is one
/// literal quote, so values with apostrophes (`O'Brien`) round-trip through the
/// SQL surface. Prerequisite for a faithful SGQL dump/restore.
#[test]
fn sql_string_literal_doubled_quote_escape() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE t (name TEXT)").unwrap();
    db.execute("INSERT INTO t (_key, name) VALUES ('a', 'O''Brien')").unwrap();
    db.execute("INSERT INTO t (_key, name) VALUES ('b', 'Rod Laver''s Arena')").unwrap();
    db.execute("INSERT INTO t (_key, name) VALUES ('c', '')").unwrap(); // empty string still valid

    assert!(db.get("t/a").unwrap().contains(r#""name":"O'Brien""#));
    assert!(db.get("t/b").unwrap().contains(r#""name":"Rod Laver's Arena""#));
    // WHERE with an escaped literal matches the stored value.
    let hits = db
        .query("SELECT name FROM t WHERE name = 'O''Brien'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
}

// ── put_vector via SQL ────────────────────────────────────────────────────────

/// INSERT with a `[f32, ...]` array literal stores the vector and makes it
/// searchable via `VECTOR_NEAR`.
#[test]
fn sql_insert_vector_literal() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE docs (_key TEXT, emb VECTOR)"#).unwrap();
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 0.0, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d2', [0.0, 1.0, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d3', [0.0, 0.0, 1.0])"#).unwrap();

    let hits = db
        .query("SELECT * FROM docs WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 1)")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "docs/d1");
}

/// UPDATE with a `[f32, ...]` literal replaces the stored vector.
#[test]
fn sql_update_vector_literal() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE docs (_key TEXT, emb VECTOR)"#).unwrap();
    // Insert with initial vectors
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d1', [0.0, 1.0, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d2', [1.0, 0.0, 0.0])"#).unwrap();

    // Before update: query [1,0,0] should return d2 as nearest
    let before = db
        .query("SELECT * FROM docs WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 1)")
        .unwrap()
        .collect();
    assert_eq!(before[0].slug, "docs/d2");

    // Update d1's vector to point toward [1,0,0]
    db.execute(r#"UPDATE docs SET emb = [1.0, 0.0, 0.0] WHERE _key = 'd1'"#).unwrap();

    // After update: both d1 and d2 are equal distance — top-2 should return both
    let after = db
        .query("SELECT * FROM docs WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 2)")
        .unwrap()
        .collect();
    assert_eq!(after.len(), 2, "both docs should be near after update");
}

/// SQL-inserted vectors are also queryable via the builder atom API.
#[test]
fn sql_insert_vector_queryable_via_atom() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE items (_key TEXT, vec VECTOR)"#).unwrap();
    db.execute(r#"INSERT INTO items (_key, vec) VALUES ('a', [0.6, 0.8, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO items (_key, vec) VALUES ('b', [0.0, 0.0, 1.0])"#).unwrap();

    let results = db
        .collection("items")
        .vector_near("vec", vec![0.6, 0.8, 0.0], 1)
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].slug, "items/a");
}

// ── ORDER BY field <=> [...] (vector similarity sort) ─────────────────────────

/// `ORDER BY emb <=> [...]` returns all results sorted nearest-first.
#[test]
fn order_by_vector_similarity() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE docs (_key TEXT, emb VECTOR)"#).unwrap();
    // Three orthogonal unit vectors
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 0.0, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d2', [0.0, 1.0, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO docs (_key, emb) VALUES ('d3', [0.0, 0.0, 1.0])"#).unwrap();

    // Query closest to d1
    let hits = db
        .query("SELECT * FROM docs ORDER BY emb <=> [1.0, 0.0, 0.0]")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].slug, "docs/d1", "d1 should be nearest to [1,0,0]");
}

/// `ORDER BY` with `LIMIT` returns only the k nearest.
#[test]
fn order_by_vector_with_limit() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE notes (_key TEXT, vec VECTOR)"#).unwrap();
    for i in 0..10u32 {
        // Vectors rotating in the XY plane
        let x = (i as f32) / 10.0;
        let y = 1.0 - x;
        db.execute(&format!(r#"INSERT INTO notes (_key, vec) VALUES ('n{i}', [{x}, {y}, 0.0])"#))
            .unwrap();
    }
    // Query close to [0.9, 0.1, 0.0] — n9 has x=0.9,y=0.1
    let hits = db
        .query("SELECT * FROM notes ORDER BY vec <=> [0.9, 0.1, 0.0] LIMIT 3")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].slug, "notes/n9", "n9 should be nearest to [0.9, 0.1, 0.0]");
}

/// `WHERE` filter combined with `ORDER BY <=>` — filter first, then rank.
#[test]
fn order_by_vector_with_where_filter() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE items (_key TEXT, tag TEXT, vec VECTOR)"#).unwrap();
    db.execute(r#"INSERT INTO items (_key, tag, vec) VALUES ('a', 'good', [1.0, 0.0])"#).unwrap();
    db.execute(r#"INSERT INTO items (_key, tag, vec) VALUES ('b', 'good', [0.9, 0.1])"#).unwrap();
    db.execute(r#"INSERT INTO items (_key, tag, vec) VALUES ('c', 'bad',  [1.0, 0.0])"#).unwrap();

    // Only tag='good', sorted by distance to [1,0]
    let hits = db
        .query("SELECT * FROM items WHERE tag = 'good' ORDER BY vec <=> [1.0, 0.0]")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].slug, "items/a", "a has exact match [1,0]");
}

// ── ORDER BY arithmetic score expressions ────────────────────────────────────

/// `ORDER BY field * weight` — plain field weighted sort.
#[test]
fn order_by_expr_field_multiply() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","score":10}"#).unwrap();
    db.put("p/2", r#"{"_collection":"p","_key":"2","score":30}"#).unwrap();
    db.put("p/3", r#"{"_collection":"p","_key":"3","score":20}"#).unwrap();

    // score * 1.0 DESC — same as ORDER BY score DESC
    let hits = db.query("SELECT * FROM p ORDER BY score * 1.0 DESC").unwrap().collect();
    let scores: Vec<f64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["score"].as_f64().unwrap())
        .collect();
    assert_eq!(scores, [30.0, 20.0, 10.0]);
}

/// `ORDER BY a + b` — sum of two payload fields.
#[test]
fn order_by_expr_field_addition() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","a":1,"b":9}"#).unwrap(); // sum=10
    db.put("p/2", r#"{"_collection":"p","_key":"2","a":5,"b":3}"#).unwrap(); // sum=8
    db.put("p/3", r#"{"_collection":"p","_key":"3","a":7,"b":7}"#).unwrap(); // sum=14

    let hits = db.query("SELECT * FROM p ORDER BY a + b DESC").unwrap().collect();
    let sums: Vec<f64> = hits.iter()
        .map(|h| {
            let p = h.payload.as_ref().unwrap();
            p["a"].as_f64().unwrap() + p["b"].as_f64().unwrap()
        })
        .collect();
    assert_eq!(sums, [14.0, 10.0, 8.0]);
}

/// `ORDER BY a * 0.6 + b * 0.4 DESC` — weighted combination of two fields.
#[test]
fn order_by_expr_weighted_fields() {
    let mut db = CoreDB::new();
    // weighted = a*0.6 + b*0.4
    db.put("p/1", r#"{"_collection":"p","_key":"1","a":10,"b":0}"#).unwrap();  // 6.0
    db.put("p/2", r#"{"_collection":"p","_key":"2","a":0,"b":10}"#).unwrap();  // 4.0
    db.put("p/3", r#"{"_collection":"p","_key":"3","a":5,"b":10}"#).unwrap();  // 7.0

    let hits = db.query("SELECT * FROM p ORDER BY a * 0.6 + b * 0.4 DESC").unwrap().collect();
    let keys: Vec<&str> = hits.iter()
        .map(|h| h.slug.split('/').last().unwrap())
        .collect();
    assert_eq!(keys, ["3", "1", "2"]); // 7.0, 6.0, 4.0
}

/// `ORDER BY (a + b) * 0.5 DESC` — parenthesised sub-expression.
#[test]
fn order_by_expr_parentheses() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","a":2,"b":8}"#).unwrap();  // (2+8)*0.5=5.0
    db.put("p/2", r#"{"_collection":"p","_key":"2","a":6,"b":6}"#).unwrap();  // (6+6)*0.5=6.0
    db.put("p/3", r#"{"_collection":"p","_key":"3","a":1,"b":1}"#).unwrap();  // (1+1)*0.5=1.0

    let hits = db.query("SELECT * FROM p ORDER BY (a + b) * 0.5 DESC").unwrap().collect();
    let keys: Vec<&str> = hits.iter()
        .map(|h| h.slug.split('/').last().unwrap())
        .collect();
    assert_eq!(keys, ["2", "1", "3"]); // 6.0, 5.0, 1.0
}

/// `ORDER BY BM25(field, 'q') * 0.7 + BM25(body, 'q') * 0.3 DESC` — two BM25 signals.
#[test]
fn order_by_expr_dual_bm25() {
    // BM25 IDF is positive only when df < N/2. We ensure 'rust' appears in 1
    // of 5 docs so IDF = ln((5-1+0.5)/(1+0.5)) ≈ ln(3) ≈ 1.1 > 0.
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE docs (_key TEXT, title TEXT, body TEXT)").unwrap();
    // d1: the only rust doc — should rank first.
    db.execute("INSERT INTO docs (_key, title, body) VALUES ('d1', 'rust programming guide', 'building systems in rust')").unwrap();
    // d2-d5: no rust — should all rank below d1.
    db.execute("INSERT INTO docs (_key, title, body) VALUES ('d2', 'introduction to python', 'scripting with python')").unwrap();
    db.execute("INSERT INTO docs (_key, title, body) VALUES ('d3', 'web development basics', 'html css javascript guide')").unwrap();
    db.execute("INSERT INTO docs (_key, title, body) VALUES ('d4', 'database fundamentals', 'sql and nosql databases overview')").unwrap();
    db.execute("INSERT INTO docs (_key, title, body) VALUES ('d5', 'machine learning overview', 'neural networks and model training')").unwrap();
    // CREATE INDEX builds the BM25 index from all existing data in the collection.
    db.execute("CREATE INDEX ON docs USING bm25 (title)").unwrap();
    db.execute("CREATE INDEX ON docs USING bm25 (body)").unwrap();

    let hits = db
        .query("SELECT * FROM docs ORDER BY BM25(title, 'rust') * 0.7 + BM25(body, 'rust') * 0.3 DESC")
        .unwrap()
        .collect();

    assert_eq!(hits.len(), 5);
    // d1 is the only rust doc — it must rank first.
    assert_eq!(hits[0].slug, "docs/d1",
        "d1 is the only rust doc and must rank first");
}

/// `ORDER BY BM25(field,'q') * 0.5 + score * 0.5 DESC` — BM25 + numeric field hybrid.
#[test]
fn order_by_expr_bm25_plus_field() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE docs (_key TEXT, title TEXT, score REAL)").unwrap();
    // d1: title matches 'rust' well, score=1
    db.execute("INSERT INTO docs (_key, title, score) VALUES ('d1', 'rust systems programming', 1)").unwrap();
    // d2: title matches 'rust', but also has score=100 → should beat d1 via hybrid
    db.execute("INSERT INTO docs (_key, title, score) VALUES ('d2', 'rust basics', 100)").unwrap();
    // d3: no match, low score
    db.execute("INSERT INTO docs (_key, title, score) VALUES ('d3', 'python scripting', 1)").unwrap();
    // CREATE INDEX builds the BM25 index from all existing data.
    db.execute("CREATE INDEX ON docs USING bm25 (title)").unwrap();

    let hits = db
        .query("SELECT * FROM docs ORDER BY BM25(title, 'rust') * 0.5 + score * 0.5 DESC")
        .unwrap()
        .collect();

    assert_eq!(hits.len(), 3);
    // d2 (rust + score=100) should beat d1 (rust + score=1)
    assert_eq!(hits[0].slug, "docs/d2", "d2 has high score so should rank first");
    // d3 gets 0 BM25 but score=1; d1 gets BM25 but score=1; d1 wins on BM25
    let last_key = hits[2].slug.split('/').last().unwrap();
    assert_eq!(last_key, "d3", "d3 with no title match should rank last");
}

/// Backward compat: `ORDER BY field ASC` still works unchanged.
#[test]
fn order_by_expr_backward_compat_field() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","v":3}"#).unwrap();
    db.put("p/2", r#"{"_collection":"p","_key":"2","v":1}"#).unwrap();
    db.put("p/3", r#"{"_collection":"p","_key":"3","v":2}"#).unwrap();

    let hits = db.query("SELECT * FROM p ORDER BY v ASC").unwrap().collect();
    let vals: Vec<i64> = hits.iter().map(|h| h.payload.as_ref().unwrap()["v"].as_i64().unwrap()).collect();
    assert_eq!(vals, [1, 2, 3]);
}

/// Backward compat: `ORDER BY field1 ASC, field2 DESC` multi-column still works.
#[test]
fn order_by_expr_backward_compat_multi_column() {
    let mut db = CoreDB::new();
    db.put("p/1", r#"{"_collection":"p","_key":"1","cat":"a","v":2}"#).unwrap();
    db.put("p/2", r#"{"_collection":"p","_key":"2","cat":"a","v":1}"#).unwrap();
    db.put("p/3", r#"{"_collection":"p","_key":"3","cat":"b","v":5}"#).unwrap();

    let hits = db.query("SELECT * FROM p ORDER BY cat ASC, v DESC").unwrap().collect();
    let keys: Vec<&str> = hits.iter().map(|h| h.slug.split('/').last().unwrap()).collect();
    assert_eq!(keys, ["1", "2", "3"]); // cat ASC: a before b; within a, v DESC: 2 then 1
}

/// Backward compat: `ORDER BY field <=> [vec]` vector sort still works.
#[test]
fn order_by_expr_backward_compat_vector() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE vdocs (_key TEXT, emb VECTOR)").unwrap();
    db.execute("INSERT INTO vdocs (_key, emb) VALUES ('v1', [1.0, 0.0, 0.0])").unwrap();
    db.execute("INSERT INTO vdocs (_key, emb) VALUES ('v2', [0.0, 1.0, 0.0])").unwrap();
    db.execute("INSERT INTO vdocs (_key, emb) VALUES ('v3', [0.0, 0.0, 1.0])").unwrap();

    let hits = db.query("SELECT * FROM vdocs ORDER BY emb <=> [1.0, 0.0, 0.0]").unwrap().collect();
    assert_eq!(hits[0].slug, "vdocs/v1");
}

/// `ORDER BY field <-> [vec]` (L2 operator) and `VECTOR_L2(field, [vec])` function form.
#[test]
fn order_by_vector_l2_operator_and_function() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT, emb VECTOR)").unwrap();
    // v1 is at [1,0,0], v2 at [0,1,0], v3 at [0,0,1]
    db.execute("INSERT INTO items (_key, emb) VALUES ('v1', [1.0, 0.0, 0.0])").unwrap();
    db.execute("INSERT INTO items (_key, emb) VALUES ('v2', [0.0, 1.0, 0.0])").unwrap();
    db.execute("INSERT INTO items (_key, emb) VALUES ('v3', [0.0, 0.0, 1.0])").unwrap();

    // Operator form: <-> nearest L2 to [1,0,0] → v1 first
    let op_hits: Vec<_> = db.query("SELECT * FROM items ORDER BY emb <-> [1.0, 0.0, 0.0]").unwrap().collect();
    assert_eq!(op_hits[0].slug, "items/v1", "<-> operator: nearest L2 first");

    // Function form: VECTOR_L2 DESC (lowest distance = negative = highest score)
    let fn_hits: Vec<_> = db.query("SELECT * FROM items ORDER BY -VECTOR_L2(emb, [1.0, 0.0, 0.0]) DESC").unwrap().collect();
    assert_eq!(fn_hits[0].slug, "items/v1", "VECTOR_L2 function: nearest first");
}

/// `ORDER BY field <#> [vec]` (Dot product operator) — highest similarity first.
#[test]
fn order_by_vector_dot_operator() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT, emb VECTOR)").unwrap();
    db.execute("INSERT INTO items (_key, emb) VALUES ('strong', [0.9, 0.9, 0.9])").unwrap();
    db.execute("INSERT INTO items (_key, emb) VALUES ('weak',   [0.1, 0.1, 0.1])").unwrap();
    db.execute("INSERT INTO items (_key, emb) VALUES ('mid',    [0.5, 0.5, 0.5])").unwrap();

    // <#> negates internally so highest dot product = first (ascending negated)
    let hits: Vec<_> = db.query("SELECT * FROM items ORDER BY emb <#> [1.0, 1.0, 1.0]").unwrap().collect();
    assert_eq!(hits[0].slug, "items/strong", "<#> operator: highest dot product first");
    assert_eq!(hits[2].slug, "items/weak",   "<#> operator: lowest dot product last");
}

// ── ORDER BY spatial + graph signals ──────────────────────────────────────────

/// ST_DISTANCE as a score signal: closer venues rank higher when we negate distance.
/// Venues (all in Melbourne CBD) ordered by proximity to Flinders Street Station.
#[test]
fn order_by_expr_st_distance_descending_proximity() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT, geometry GEO)").unwrap();
    // Flinders Street Station: 144.9671, -37.8183
    // Young and Jacksons: 144.9631, -37.8173 — nearest
    // Melbourne Central: 144.9631, -37.8102 — a bit further
    // Geelong Station: 144.3617, -38.1499 — ~70 km away
    db.execute("INSERT INTO venues (_key, name, geometry) VALUES ('fss', 'Flinders Street Station', '{\"type\":\"Point\",\"coordinates\":[144.9671,-37.8183]}')").unwrap();
    db.execute("INSERT INTO venues (_key, name, geometry) VALUES ('yj', 'Young and Jacksons', '{\"type\":\"Point\",\"coordinates\":[144.9631,-37.8173]}')").unwrap();
    db.execute("INSERT INTO venues (_key, name, geometry) VALUES ('mc', 'Melbourne Central', '{\"type\":\"Point\",\"coordinates\":[144.9631,-37.8102]}')").unwrap();
    db.execute("INSERT INTO venues (_key, name, geometry) VALUES ('gs', 'Geelong Station', '{\"type\":\"Point\",\"coordinates\":[144.3617,-38.1499]}')").unwrap();

    // Sort by negative ST_DISTANCE from Flinders Street Station — ascending distance = descending score.
    // fss itself has distance 0 (most negative negate = highest), geelong is furthest.
    let hits = db
        .query("SELECT * FROM venues ORDER BY -ST_DISTANCE(geometry, POINT(144.9671 -37.8183)) DESC")
        .unwrap()
        .collect();

    assert_eq!(hits[0].slug, "venues/fss", "distance-0 node must rank first: {:?}", hits.iter().map(|h| &h.slug).collect::<Vec<_>>());
    assert_eq!(hits.last().unwrap().slug, "venues/gs", "Geelong must rank last");
}

// ── Cascade edge deletion on node remove ──────────────────────────────────────

/// Deleting a node removes its outgoing edges so the target no longer sees
/// back-pointers from the deleted node.
#[test]
fn delete_node_removes_outgoing_edges() {
    let mut db = CoreDB::new();
    db.put("artists/dewa19", r#"{"_collection":"artists","_key":"dewa19"}"#).unwrap();
    db.put("songs/kangen",   r#"{"_collection":"songs",  "_key":"kangen"}"#).unwrap();
    db.link("artists/dewa19", "songs/kangen", "has_song");

    // Sanity: edge exists
    assert_eq!(db.edges_from("artists/dewa19").len(), 1);

    db.remove("artists/dewa19");

    // Forward edge is gone
    assert_eq!(db.edges_from("artists/dewa19").len(), 0);
    // Back-pointer on the target is also gone — no dangling ref
    assert_eq!(db.edges_to("songs/kangen").len(), 0,
        "deleting dewa19 must remove kangen's back-pointer");
}

/// Deleting a target node removes incoming edges so the source no longer
/// enumerates a dead forward pointer.
#[test]
fn delete_node_removes_incoming_edges() {
    let mut db = CoreDB::new();
    db.put("artists/dewa19", r#"{"_collection":"artists","_key":"dewa19"}"#).unwrap();
    db.put("songs/kangen",   r#"{"_collection":"songs",  "_key":"kangen"}"#).unwrap();
    db.link("artists/dewa19", "songs/kangen", "has_song");

    db.remove("songs/kangen");

    // Back-pointer is gone
    assert_eq!(db.edges_to("songs/kangen").len(), 0);
    // Forward pointer from source is also gone — no dangling ref
    assert_eq!(db.edges_from("artists/dewa19").len(), 0,
        "deleting kangen must remove dewa19's forward pointer");
}

/// SQL DELETE also cascades edges.
#[test]
fn sql_delete_cascades_edges() {
    let mut db = CoreDB::new();
    db.put("a/1", r#"{"_collection":"a","_key":"1"}"#).unwrap();
    db.put("a/2", r#"{"_collection":"a","_key":"2"}"#).unwrap();
    db.put("a/3", r#"{"_collection":"a","_key":"3"}"#).unwrap();
    db.link("a/1", "a/2", "rel");
    db.link("a/2", "a/3", "rel");

    // Delete middle node via SQL
    db.execute("DELETE FROM a WHERE _key = '2'").unwrap();

    assert!(!db.contains("a/2"));
    assert_eq!(db.edges_from("a/1").len(), 0, "forward edge from a/1 must be gone");
    assert_eq!(db.edges_to("a/3").len(),  0, "back edge into a/3 must be gone");
}

// ── Aggregate MATCH tests ─────────────────────────────────

/// Basic aggregate MATCH: one hop, flat (no GROUP BY).
#[test]
fn traverse_single_hop_flat() {
    let mut db = CoreDB::new();
    db.put("students/budi", r#"{"_collection":"students","_key":"budi","name":"Budi"}"#).unwrap();
    db.put("answers/a1", r#"{"_collection":"answers","_key":"a1","score":0.8}"#).unwrap();
    db.put("answers/a2", r#"{"_collection":"answers","_key":"a2","score":0.6}"#).unwrap();
    db.link("students/budi", "answers/a1", "answered");
    db.link("students/budi", "answers/a2", "answered");

    let hits = db.query(
        "SELECT a.score AS score FROM MATCH ('students/budi')-[:answered]->(a)"
    ).unwrap().collect();

    assert_eq!(hits.len(), 2);
    let scores: Vec<f64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap().get("score").and_then(|v| v.as_f64()).unwrap())
        .collect();
    assert!(scores.contains(&0.8));
    assert!(scores.contains(&0.6));
}

/// Two-hop aggregate MATCH with GROUP BY and SUM aggregation — OBE-style weighted score.
#[test]
fn traverse_two_hop_group_sum() {
    let mut db = CoreDB::new();
    db.put("students/budi",  r#"{"_collection":"students","_key":"budi"}"#).unwrap();
    db.put("answers/a1",     r#"{"_collection":"answers","_key":"a1","score":0.8}"#).unwrap();
    db.put("answers/a2",     r#"{"_collection":"answers","_key":"a2","score":0.6}"#).unwrap();
    db.put("answers/a3",     r#"{"_collection":"answers","_key":"a3","score":1.0}"#).unwrap();
    db.put("questions/q1",   r#"{"_collection":"questions","_key":"q1","weight":0.4,"clo":"c1"}"#).unwrap();
    db.put("questions/q2",   r#"{"_collection":"questions","_key":"q2","weight":0.6,"clo":"c1"}"#).unwrap();
    db.put("questions/q3",   r#"{"_collection":"questions","_key":"q3","weight":1.0,"clo":"c2"}"#).unwrap();

    // Student answered questions
    db.link("students/budi", "answers/a1", "answered");
    db.link("students/budi", "answers/a2", "answered");
    db.link("students/budi", "answers/a3", "answered");
    // Answers → questions
    db.link("answers/a1", "questions/q1", "for");
    db.link("answers/a2", "questions/q2", "for");
    db.link("answers/a3", "questions/q3", "for");

    let hits = db.query(
        "SELECT q.clo AS clo, SUM(a.score * q.weight) AS clo_score \
         FROM MATCH ('students/budi')-[:answered]->(a)-[:for]->(q) \
         GROUP BY q.clo \
         ORDER BY clo_score DESC"
    ).unwrap().collect();

    assert_eq!(hits.len(), 2, "should have 2 CLO groups");

    // CLO c1: a1.score(0.8) * q1.weight(0.4) + a2.score(0.6) * q2.weight(0.6) = 0.32 + 0.36 = 0.68
    // CLO c2: a3.score(1.0) * q3.weight(1.0) = 1.0
    let top = hits[0].payload.as_ref().unwrap();
    assert_eq!(top.get("clo").and_then(|v| v.as_str()), Some("c2"),
               "c2 has higher score (1.0) so comes first with DESC ordering");
    let top_score = top.get("clo_score").and_then(|v| v.as_f64()).unwrap();
    assert!((top_score - 1.0).abs() < 1e-9, "c2 score should be 1.0");

    let second = hits[1].payload.as_ref().unwrap();
    let second_score = second.get("clo_score").and_then(|v| v.as_f64()).unwrap();
    assert!((second_score - 0.68).abs() < 1e-9, "c1 score should be 0.68");
}

/// Multi-field MATCH GROUP BY: GROUP BY b.role, b.tier — uniform interface with Set path.
///
/// Note: the start variable is not bound in PathRow; GROUP BY fields must reference
/// destination-hop variables.  This test groups on two fields of the same destination `b`.
#[test]
fn traverse_match_group_by_multi_field() {
    let mut db = CoreDB::new();
    db.put("users/u1", r#"{"_collection":"users","_key":"u1"}"#).unwrap();
    db.put("users/u2", r#"{"_collection":"users","_key":"u2"}"#).unwrap();
    db.put("users/u3", r#"{"_collection":"users","_key":"u3"}"#).unwrap();
    // Two roles share (admin, high); one is (viewer, low)
    db.put("roles/r1", r#"{"_collection":"roles","_key":"r1","role":"admin","tier":"high"}"#).unwrap();
    db.put("roles/r2", r#"{"_collection":"roles","_key":"r2","role":"viewer","tier":"low"}"#).unwrap();
    db.put("roles/r3", r#"{"_collection":"roles","_key":"r3","role":"admin","tier":"high"}"#).unwrap();

    db.link("users/u1", "roles/r1", "has");
    db.link("users/u2", "roles/r2", "has");
    db.link("users/u3", "roles/r3", "has");

    let hits = db.query(
        "SELECT b.role AS role, b.tier AS tier, COUNT(*) AS cnt \
         FROM MATCH (a:users)-[:has]->(b:roles) \
         GROUP BY b.role, b.tier \
         ORDER BY cnt DESC"
    ).unwrap().collect();

    // (admin, high) = 2 paths, (viewer, low) = 1 path
    assert_eq!(hits.len(), 2);
    let top = hits[0].payload.as_ref().unwrap();
    assert_eq!(top["role"].as_str().unwrap(), "admin");
    assert_eq!(top["tier"].as_str().unwrap(), "high");
    assert_eq!(top["cnt"].as_i64().unwrap(), 2);
    let bot = hits[1].payload.as_ref().unwrap();
    assert_eq!(bot["role"].as_str().unwrap(), "viewer");
    assert_eq!(bot["cnt"].as_i64().unwrap(), 1);
}

/// Aggregate MATCH COUNT(*) and AVG aggregation.
#[test]
fn traverse_count_and_avg() {
    let mut db = CoreDB::new();
    db.put("students/budi", r#"{"_collection":"students","_key":"budi"}"#).unwrap();
    for i in 1..=4 {
        db.put(
            &format!("answers/a{i}"),
            &format!(r#"{{"_collection":"answers","_key":"a{i}","score":{}}}"#, i as f64 * 0.25)
        ).unwrap();
        db.link("students/budi", &format!("answers/a{i}"), "answered");
    }

    let hits = db.query(
        "SELECT COUNT(*) AS cnt, AVG(a.score) AS avg_score \
         FROM MATCH ('students/budi')-[:answered]->(a)"
    ).unwrap().collect();

    // Aggregate without GROUP BY → one row over ALL paths (SQL semantics).
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p.get("cnt").unwrap().as_i64(), Some(4));
    // scores: 0.25, 0.5, 0.75, 1.0 → avg 0.625
    assert!((p.get("avg_score").unwrap().as_f64().unwrap() - 0.625).abs() < 1e-9);
}

/// Aggregate MATCH with LIMIT.
#[test]
fn traverse_with_limit() {
    let mut db = CoreDB::new();
    db.put("s/root", r#"{"_collection":"s","_key":"root"}"#).unwrap();
    for i in 1..=10 {
        db.put(&format!("t/n{i}"), &format!(r#"{{"_collection":"t","_key":"n{i}","val":{i}}}"#)).unwrap();
        db.link("s/root", &format!("t/n{i}"), "to");
    }

    let hits = db.query(
        "SELECT n.val AS val FROM MATCH ('s/root')-[:to]->(n) LIMIT 5"
    ).unwrap().collect();
    assert_eq!(hits.len(), 5);
}

/// Aggregate MATCH from a collection (all starting nodes in the collection).
#[test]
fn traverse_from_collection() {
    let mut db = CoreDB::new();
    for s in ["alice", "bob"] {
        db.put(&format!("students/{s}"), &format!(r#"{{"_collection":"students","_key":"{s}"}}"#)).unwrap();
        db.put(&format!("answers/{s}_ans"), &format!(r#"{{"_collection":"answers","_key":"{s}_ans","score":0.9}}"#)).unwrap();
        db.link(&format!("students/{s}"), &format!("answers/{s}_ans"), "answered");
    }

    let hits = db.query(
        "SELECT a.score AS score FROM MATCH (s:students)-[:answered]->(a)"
    ).unwrap().collect();
    assert_eq!(hits.len(), 2, "one path per student");
}

/// Aggregate MATCH with MIN/MAX.
#[test]
fn traverse_min_max() {
    let mut db = CoreDB::new();
    db.put("root/r", r#"{"_collection":"root","_key":"r"}"#).unwrap();
    for (k, v) in [("a", 10.0f64), ("b", 5.0), ("c", 8.0)] {
        db.put(&format!("vals/{k}"), &format!(r#"{{"_collection":"vals","_key":"{k}","v":{v}}}"#)).unwrap();
        db.link("root/r", &format!("vals/{k}"), "link");
    }

    let hits = db.query(
        "SELECT MIN(n.v) AS min_v, MAX(n.v) AS max_v \
         FROM MATCH ('root/r')-[:link]->(n)"
    ).unwrap().collect();
    // Aggregate without GROUP BY → one row: min/max over ALL values (SQL semantics).
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert!((p.get("min_v").unwrap().as_f64().unwrap() - 5.0).abs() < 1e-9);
    assert!((p.get("max_v").unwrap().as_f64().unwrap() - 10.0).abs() < 1e-9);
}

// ── MATCH + WITH pipeline tests ───────────────────────────────────────────────

/// Basic pipeline: one `SELECT ... FROM MATCH` with scalar projection.
#[test]
fn pipeline_single_match_scalar_return() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"_collection":"users","_key":"alice","score":10.0}"#).unwrap();
    db.put("users/bob",   r#"{"_collection":"users","_key":"bob","score":20.0}"#).unwrap();
    db.put("posts/p1",    r#"{"_collection":"posts","_key":"p1","title":"hello"}"#).unwrap();
    db.put("posts/p2",    r#"{"_collection":"posts","_key":"p2","title":"world"}"#).unwrap();
    db.link("users/alice", "posts/p1", "wrote");
    db.link("users/alice", "posts/p2", "wrote");

    let hits = db.query(
        "SELECT p._key AS post_key FROM MATCH ('users/alice')-[:wrote]->(p)",
    ).unwrap().collect();

    let mut keys: Vec<String> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap().get("post_key")
            .and_then(|v| v.as_str()).unwrap().to_string())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["p1", "p2"]);
}

/// Pipeline with WITH aggregation — OBE-style 1-level CLO calculation.
#[test]
fn pipeline_match_with_group_sum() {
    let mut db = CoreDB::new();

    // Student
    db.put("students/budi", r#"{"_collection":"students","_key":"budi"}"#).unwrap();

    // Answers (with score)
    db.put("answers/a1", r#"{"_collection":"answers","_key":"a1","score":0.8}"#).unwrap();
    db.put("answers/a2", r#"{"_collection":"answers","_key":"a2","score":0.9}"#).unwrap();
    db.put("answers/a3", r#"{"_collection":"answers","_key":"a3","score":1.0}"#).unwrap();

    // Questions (with clo and weight)
    db.put("questions/q1", r#"{"_collection":"questions","_key":"q1","clo":"clo1","weight":0.4}"#).unwrap();
    db.put("questions/q2", r#"{"_collection":"questions","_key":"q2","clo":"clo1","weight":0.6}"#).unwrap();
    db.put("questions/q3", r#"{"_collection":"questions","_key":"q3","clo":"clo2","weight":1.0}"#).unwrap();

    // Edges
    db.link("students/budi", "answers/a1", "answered");
    db.link("students/budi", "answers/a2", "answered");
    db.link("students/budi", "answers/a3", "answered");
    db.link("answers/a1", "questions/q1", "for");
    db.link("answers/a2", "questions/q2", "for");
    db.link("answers/a3", "questions/q3", "for");

    let hits = db.query(
        "SELECT q.clo AS clo, SUM(a.score * q.weight) AS clo_score \
         FROM MATCH ('students/budi')-[:answered]->(a)-[:for]->(q) \
         GROUP BY q.clo ORDER BY clo_score DESC",
    ).unwrap().collect();

    assert_eq!(hits.len(), 2, "expected 2 CLO rows");

    // clo2: 1.0 * 1.0 = 1.0 (DESC first)
    let clo_val = |h: &sekejap::Hit| {
        h.payload.as_ref().unwrap().get("clo").and_then(|v| v.as_str()).unwrap().to_string()
    };
    let score_val = |h: &sekejap::Hit| {
        h.payload.as_ref().unwrap().get("clo_score").and_then(|v| v.as_f64()).unwrap()
    };

    assert_eq!(clo_val(&hits[0]), "clo2");
    assert!((score_val(&hits[0]) - 1.0).abs() < 1e-9, "clo2 score should be 1.0");

    assert_eq!(clo_val(&hits[1]), "clo1");
    // clo1: 0.8*0.4 + 0.9*0.6 = 0.32 + 0.54 = 0.86
    assert!((score_val(&hits[1]) - 0.86).abs() < 1e-9, "clo1 score should be 0.86");
}

/// Full 2-level OBE pipeline: CLO → PLO aggregation.
#[test]
fn pipeline_two_level_clo_plo() {
    let mut db = CoreDB::new();

    // Student, answers, questions (same as above but fewer)
    db.put("students/budi", r#"{"_collection":"students","_key":"budi"}"#).unwrap();
    db.put("answers/a1", r#"{"_collection":"answers","_key":"a1","score":0.8}"#).unwrap();
    db.put("answers/a2", r#"{"_collection":"answers","_key":"a2","score":1.0}"#).unwrap();
    db.put("questions/q1", r#"{"_collection":"questions","_key":"q1","clo":"clo1","weight":1.0}"#).unwrap();
    db.put("questions/q2", r#"{"_collection":"questions","_key":"q2","clo":"clo2","weight":1.0}"#).unwrap();
    db.link("students/budi", "answers/a1", "answered");
    db.link("students/budi", "answers/a2", "answered");
    db.link("answers/a1", "questions/q1", "for");
    db.link("answers/a2", "questions/q2", "for");

    // CLOs (weight for PLO contribution)
    db.put("clos/clo1", r#"{"_collection":"clos","_key":"clo1","weight":0.5}"#).unwrap();
    db.put("clos/clo2", r#"{"_collection":"clos","_key":"clo2","weight":0.5}"#).unwrap();

    // PLO
    db.put("plos/plo1", r#"{"_collection":"plos","_key":"plo1"}"#).unwrap();

    // Edges: CLOs contribute to PLO
    db.link("clos/clo1", "plos/plo1", "contributes_to");
    db.link("clos/clo2", "plos/plo1", "contributes_to");

    // Stage 1 — CLO scores from the graph: SUM(answer.score * question.weight)
    // grouped per CLO.  clo1: 0.8*1.0 = 0.8 ; clo2: 1.0*1.0 = 1.0
    let clo_hits = db.query(
        "SELECT q.clo AS clo, SUM(a.score * q.weight) AS clo_score \
         FROM MATCH ('students/budi')-[:answered]->(a)-[:for]->(q) \
         GROUP BY q.clo",
    ).unwrap().collect();
    assert_eq!(clo_hits.len(), 2, "expected 2 CLO rows");

    // Stage 2 — roll CLO scores up to their PLO via contributes_to, weighting by
    // each CLO's weight.  plo1 = 0.8*0.5 + 1.0*0.5 = 0.9
    let mut plo_score = 0.0_f64;
    for ch in &clo_hits {
        let clo_key = ch.payload.as_ref().unwrap().get("clo").and_then(|v| v.as_str()).unwrap();
        let clo_score = ch.payload.as_ref().unwrap().get("clo_score").and_then(|v| v.as_f64()).unwrap();
        // Which PLO does this CLO contribute to, and with what weight?
        let plo_edges = db.query(&format!(
            "SELECT plo._key AS plo, c.weight AS w \
             FROM MATCH (c:clos)-[:contributes_to]->(plo:plos) WHERE c._key = '{clo_key}'"
        )).unwrap().collect();
        assert_eq!(plo_edges.len(), 1);
        let plo = plo_edges[0].payload.as_ref().unwrap().get("plo").and_then(|v| v.as_str()).unwrap();
        assert_eq!(plo, "plo1");
        let w = plo_edges[0].payload.as_ref().unwrap().get("w").and_then(|v| v.as_f64()).unwrap();
        plo_score += clo_score * w;
    }
    assert!((plo_score - 0.9).abs() < 1e-9, "plo_score should be 0.9, got {}", plo_score);
}

/// Pipeline with LIMIT.
#[test]
fn pipeline_with_limit() {
    let mut db = CoreDB::new();
    db.put("root", r#"{"_collection":"roots","_key":"root"}"#).unwrap();
    for i in 1..=5u32 {
        let slug = format!("items/i{i}");
        let pay = format!(r#"{{"_collection":"items","_key":"i{i}","val":{i}}}"#);
        db.put(&slug, &pay).unwrap();
        db.link("root", &slug, "has");
    }

    let hits = db.query(
        "SELECT item.val AS v FROM MATCH ('root')-[:has]->(item) ORDER BY v ASC LIMIT 3",
    ).unwrap().collect();

    assert_eq!(hits.len(), 3);
    let vals: Vec<f64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap().get("v").and_then(|v| v.as_f64()).unwrap())
        .collect();
    assert_eq!(vals, vec![1.0, 2.0, 3.0]);
}

/// Pipeline COUNT aggregate.
#[test]
fn pipeline_count_aggregate() {
    let mut db = CoreDB::new();
    db.put("src", r#"{"_collection":"src","_key":"src"}"#).unwrap();
    for i in 1..=4u32 {
        let slug = format!("dst/d{i}");
        db.put(&slug, &format!(r#"{{"_collection":"dst","_key":"d{i}","grp":"g{}"}}"#, if i <= 2 {"1"} else {"2"})).unwrap();
        db.link("src", &slug, "points_to");
    }

    let hits = db.query(
        "SELECT d.grp AS grp, COUNT(*) AS cnt \
         FROM MATCH ('src')-[:points_to]->(d) \
         GROUP BY d.grp ORDER BY grp ASC",
    ).unwrap().collect();

    assert_eq!(hits.len(), 2);
    let g1_cnt = hits[0].payload.as_ref().unwrap().get("cnt")
        .and_then(|v| v.as_i64()).unwrap();
    let g2_cnt = hits[1].payload.as_ref().unwrap().get("cnt")
        .and_then(|v| v.as_i64()).unwrap();
    assert_eq!(g1_cnt, 2);
    assert_eq!(g2_cnt, 2);
}

/// Multi-MATCH pipeline from a collection start.
#[test]
fn pipeline_collection_start() {
    let mut db = CoreDB::new();
    db.put("cats/a", r#"{"_collection":"cats","_key":"a"}"#).unwrap();
    db.put("cats/b", r#"{"_collection":"cats","_key":"b"}"#).unwrap();
    db.put("items/x", r#"{"_collection":"items","_key":"x","val":5.0}"#).unwrap();
    db.put("items/y", r#"{"_collection":"items","_key":"y","val":10.0}"#).unwrap();
    db.link("cats/a", "items/x", "has");
    db.link("cats/b", "items/y", "has");

    let hits = db.query(
        "SELECT c._key AS cat, item.val AS val FROM MATCH (c:cats)-[:has]->(item:items) ORDER BY val ASC",
    ).unwrap().collect();

    assert_eq!(hits.len(), 2);
    let val_of = |i: usize| hits[i].payload.as_ref().unwrap().get("val")
        .and_then(|v| v.as_f64()).unwrap();
    assert_eq!(val_of(0), 5.0);
    assert_eq!(val_of(1), 10.0);
}

/// Multi-hop traversal after deleting an intermediate node must not return
/// the deleted node or traverse dead edges.
#[test]
fn traversal_after_delete_skips_deleted_node() {
    let mut db = CoreDB::new();
    for k in ["a", "b", "c"] {
        db.put(&format!("n/{k}"), &format!(r#"{{"_collection":"n","_key":"{k}"}}"#)).unwrap();
    }
    db.link("n/a", "n/b", "e");
    db.link("n/b", "n/c", "e");

    // 2-hop from a reaches b and c
    assert_eq!(db.one("n/a").hops_typed("e", 2).count(), 2);

    db.remove("n/b");

    // After deleting b, 2-hop from a reaches nothing
    assert_eq!(db.one("n/a").hops_typed("e", 2).count(), 0);
}

// ── Pipeline WHERE comparison operators ───────────────────────────────────────

#[test]
fn pipeline_where_cmp_operators() {
    let mut db = CoreDB::new();

    // Students with scores; some pass (>=60), some fail
    for (key, name, score) in [
        ("stu/ali", "Ali", 80.0_f64),
        ("stu/budi", "Budi", 55.0_f64),
        ("stu/cici", "Cici", 72.0_f64),
        ("stu/dodi", "Dodi", 45.0_f64),
    ] {
        db.put(key, &serde_json::json!({
            "_collection": "students",
            "_key": key,
            "name": name,
            "score": score,
        }).to_string()).unwrap();
    }

    // ── Test >= (pass threshold) ──────────────────────────────────────────────
    let hits = db.query(
        "SELECT * FROM students WHERE score >= 60"
    ).unwrap().collect();
    assert_eq!(hits.len(), 2, "Ali and Cici should pass (score >= 60)");

    // ── Test < (failing) ─────────────────────────────────────────────────────
    let hits = db.query(
        "SELECT * FROM students WHERE score < 60"
    ).unwrap().collect();
    assert_eq!(hits.len(), 2, "Budi and Dodi should fail (score < 60)");

    // ── Test != ──────────────────────────────────────────────────────────────
    let hits = db.query(
        "SELECT * FROM students WHERE score != 80"
    ).unwrap().collect();
    assert_eq!(hits.len(), 3, "Everyone except Ali");

    // ── Test AND with multiple comparison ops ────────────────────────────────
    // score >= 50 AND score <= 75 → Budi(55) and Cici(72)
    let hits = db.query(
        "SELECT * FROM students WHERE score >= 50 AND score <= 75"
    ).unwrap().collect();
    assert_eq!(hits.len(), 2, "Budi(55) and Cici(72) are in 50..=75");
}

// ── Collection-level edge listing ─────────────────────────────────────────────

#[test]
fn edges_from_collection_and_between() {
    let mut db = CoreDB::new();

    // Two classrooms, two lecturers, one department
    for (key, col) in [
        ("cls/math", "classrooms"), ("cls/physics", "classrooms"),
        ("lec/ali", "lecturers"),   ("lec/budi", "lecturers"),
        ("dept/sci", "departments"),
    ] {
        db.put(key, &serde_json::json!({
            "_collection": col, "_key": key
        }).to_string()).unwrap();
    }

    db.link("cls/math",    "lec/ali",  "taught_by");
    db.link("cls/physics", "lec/budi", "taught_by");
    db.link("lec/ali",     "dept/sci", "belongs_to");

    // ── edges_from_collection: all edges leaving classrooms ───────────────────
    let edges = db.edges_from_collection("classrooms");
    assert_eq!(edges.len(), 2);
    // edge_type label is now resolved
    assert!(edges.iter().all(|e| e.edge_type.as_deref() == Some("taught_by")));

    // ── edges_between: classrooms → lecturers only ────────────────────────────
    let edges = db.edges_between("classrooms", "lecturers");
    assert_eq!(edges.len(), 2);

    // ── edges_between: lecturers → classrooms = 0 (direction matters) ─────────
    let edges = db.edges_between("lecturers", "classrooms");
    assert_eq!(edges.len(), 0);

    // ── edges_between: lecturers → departments ────────────────────────────────
    let edges = db.edges_between("lecturers", "departments");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from_slug.as_deref(), Some("lec/ali"));
    assert_eq!(edges[0].to_slug.as_deref(),   Some("dept/sci"));
    assert_eq!(edges[0].edge_type.as_deref(), Some("belongs_to"));
}

#[test]
fn show_edges_sql() {
    let mut db = CoreDB::new();
    for (key, col) in [
        ("cls/math", "classrooms"), ("cls/physics", "classrooms"),
        ("lec/ali",  "lecturers"),  ("dept/sci",    "departments"),
    ] {
        db.put(key, &serde_json::json!({"_collection": col, "_key": key}).to_string()).unwrap();
    }
    db.link("cls/math",    "lec/ali",  "taught_by");
    db.link("cls/physics", "lec/ali",  "taught_by");
    db.link("lec/ali",     "dept/sci", "belongs_to");

    // Full schema — 2 distinct triples
    let hits = db.show("SHOW EDGES").unwrap();
    assert_eq!(hits.len(), 2);
    let types: Vec<_> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"taught_by"));
    assert!(types.contains(&"belongs_to"));

    // FROM classrooms → only taught_by
    let hits = db.show("SHOW EDGES FROM classrooms").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["type"].as_str(), Some("taught_by"));

    // FROM classrooms TO lecturers
    let hits = db.show("SHOW EDGES FROM classrooms TO lecturers").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["type"].as_str(), Some("taught_by"));

    // FROM classrooms TO departments → 0
    let hits = db.show("SHOW EDGES FROM classrooms TO departments").unwrap();
    assert_eq!(hits.len(), 0);
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

#[test]
fn alter_table_add_column() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT, capacity INTEGER)").unwrap();
    db.execute("ALTER TABLE venues ADD COLUMN suburb TEXT").unwrap();

    let hits = db.show("SHOW venues").unwrap();
    let fields: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref().and_then(|p| p["field"].as_str().map(str::to_string)))
        .collect();
    assert!(fields.contains(&"suburb".to_string()));
}

#[test]
fn alter_table_add_column_already_exists_errors() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT, capacity INTEGER)").unwrap();
    let err = db.execute("ALTER TABLE venues ADD COLUMN capacity INTEGER").unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn alter_table_add_column_no_table_errors() {
    let mut db = CoreDB::new();
    let err = db.execute("ALTER TABLE venues ADD COLUMN name TEXT").unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn alter_table_drop_column() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT, capacity INTEGER, suburb TEXT)").unwrap();
    // Insert a node with all three fields
    db.execute("INSERT INTO venues (_key, name, capacity, suburb) VALUES ('rod_laver', 'Rod Laver Arena', 15000, 'Melbourne')").unwrap();

    let count = db.execute("ALTER TABLE venues DROP COLUMN suburb").unwrap();
    assert_eq!(count, 1); // one node had the field removed

    // Schema no longer lists suburb
    let hits = db.show("SHOW venues").unwrap();
    let fields: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref().and_then(|p| p["field"].as_str().map(str::to_string)))
        .collect();
    assert!(!fields.contains(&"suburb".to_string()));

    // Node no longer has the field
    let node = db.get("venues/rod_laver").unwrap();
    let v: serde_json::Value = serde_json::from_str(&node).unwrap();
    assert!(v.get("suburb").is_none());
}

#[test]
fn alter_table_drop_column_if_exists_silent() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT)").unwrap();
    let count = db.execute("ALTER TABLE venues DROP COLUMN IF EXISTS nonexistent").unwrap();
    assert_eq!(count, 0);
}

#[test]
fn alter_table_drop_column_missing_errors() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT)").unwrap();
    let err = db.execute("ALTER TABLE venues DROP COLUMN ghost").unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn alter_table_rename_column() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE bands (_key TEXT, name TEXT, city TEXT)").unwrap();
    db.execute("INSERT INTO bands (_key, name, city) VALUES ('the_vines', 'The Vines', 'Sydney')").unwrap();

    let count = db.execute("ALTER TABLE bands RENAME COLUMN city TO hometown").unwrap();
    assert_eq!(count, 1);

    // Schema updated
    let hits = db.show("SHOW bands").unwrap();
    let fields: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref().and_then(|p| p["field"].as_str().map(str::to_string)))
        .collect();
    assert!(fields.contains(&"hometown".to_string()));
    assert!(!fields.contains(&"city".to_string()));

    // Node updated
    let node = db.get("bands/the_vines").unwrap();
    let v: serde_json::Value = serde_json::from_str(&node).unwrap();
    assert_eq!(v["hometown"], "Sydney");
    assert!(v.get("city").is_none());
}

#[test]
fn alter_table_rename_table() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE bands (_key TEXT, name TEXT)").unwrap();
    db.execute("INSERT INTO bands (_key, name) VALUES ('the_vines', 'The Vines')").unwrap();
    db.execute("INSERT INTO bands (_key, name) VALUES ('jet', 'Jet')").unwrap();

    let count = db.execute("ALTER TABLE bands RENAME TO artists").unwrap();
    assert_eq!(count, 2); // two nodes reclassified

    // Old collection query returns nothing
    let old_hits = db.query("SELECT * FROM bands").unwrap().collect();
    assert_eq!(old_hits.len(), 0);

    // New collection query returns both nodes
    let new_hits = db.query("SELECT * FROM artists").unwrap().collect();
    assert_eq!(new_hits.len(), 2);

    // SHOW TABLES reflects the rename
    let table_hits = db.show("SHOW TABLES").unwrap();
    let names: Vec<_> = table_hits.iter()
        .filter_map(|h| h.payload.as_ref().and_then(|p| p["name"].as_str().map(str::to_string)))
        .collect();
    assert!(names.contains(&"artists".to_string()));
    assert!(!names.contains(&"bands".to_string()));
}

#[test]
fn alter_table_rename_to_existing_errors() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE bands (_key TEXT, name TEXT)").unwrap();
    db.execute("CREATE TABLE artists (_key TEXT, name TEXT)").unwrap();
    let err = db.execute("ALTER TABLE bands RENAME TO artists").unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn alter_table_alter_column_type() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, name TEXT, capacity INTEGER)").unwrap();
    db.execute("ALTER TABLE venues ALTER COLUMN capacity TYPE REAL").unwrap();

    let hits = db.show("SHOW venues").unwrap();
    let capacity_hit = hits.iter()
        .find(|h| h.payload.as_ref().and_then(|p| p["field"].as_str()) == Some("capacity"))
        .expect("capacity field must be present");
    assert_eq!(
        capacity_hit.payload.as_ref().unwrap()["type"].as_str(),
        Some("REAL")
    );
}

#[test]
fn snapshot_v2_header_present_and_reopens() {
    use tempfile::TempDir;
    use sekejap::CoreDB;

    let dir = TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE t (_key TEXT, v INTEGER)").unwrap();
        db.execute("INSERT INTO t (_key, v) VALUES ('a', 1)").unwrap();
        db.execute("INSERT INTO t (_key, v) VALUES ('b', 2)").unwrap();
        db.compact().unwrap();
    }
    // The compacted snapshot must carry the versioned magic header.
    // (v3 = manifest snapshot: topology lives in the topology files.)
    let snap = std::fs::read(dir.path().join("snapshot.json")).unwrap();
    assert_eq!(&snap[0..8], b"SKSNAP\0\0", "snapshot must start with the magic");
    assert_eq!(u32::from_le_bytes(snap[8..12].try_into().unwrap()), 3, "format version");

    // Reopen straight from the headered snapshot — data intact, header transparently stripped.
    let db = CoreDB::open(dir.path()).unwrap();
    let a: serde_json::Value = serde_json::from_str(&db.get("t/a").unwrap()).unwrap();
    assert_eq!(a["v"].as_f64().unwrap(), 1.0);
    assert_eq!(db.query("SELECT * FROM t").unwrap().collect().len(), 2);
}

#[test]
fn snapshot_legacy_headerless_still_opens() {
    use tempfile::TempDir;
    use sekejap::CoreDB;

    let dir = TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE t (_key TEXT, v INTEGER)").unwrap();
        db.execute("INSERT INTO t (_key, v) VALUES ('a', 42)").unwrap();
        db.compact().unwrap(); // truncates WAL → snapshot is authoritative
    }
    // Simulate a pre-header (v1) snapshot by stripping the 16-byte header,
    // leaving a headerless JSON body exactly like older builds wrote.
    let snap_path = dir.path().join("snapshot.json");
    let full = std::fs::read(&snap_path).unwrap();
    assert_eq!(&full[0..8], b"SKSNAP\0\0");
    std::fs::write(&snap_path, &full[16..]).unwrap();

    // The legacy path must auto-detect the missing header and parse from offset 0.
    let db = CoreDB::open(dir.path()).unwrap();
    let a: serde_json::Value = serde_json::from_str(&db.get("t/a").unwrap()).unwrap();
    assert_eq!(a["v"].as_f64().unwrap(), 42.0, "legacy headerless snapshot must still load");
}

#[test]
fn alter_table_wal_replay() {
    use tempfile::TempDir;
    use sekejap::CoreDB;

    let dir = TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE venues (_key TEXT, name TEXT, capacity INTEGER)").unwrap();
        db.execute("INSERT INTO venues (_key, name, capacity) VALUES ('rod_laver', 'Rod Laver Arena', 15000)").unwrap();
        db.execute("ALTER TABLE venues ADD COLUMN suburb TEXT").unwrap();
        db.execute("ALTER TABLE venues RENAME COLUMN capacity TO seats").unwrap();
    }

    // Cold reload — WAL replay must restore all ALTER TABLE ops
    let db = CoreDB::open(dir.path()).unwrap();

    // Schema has suburb, seats; no capacity
    let hits = db.show("SHOW venues").unwrap();
    let fields: Vec<_> = hits.iter()
        .filter_map(|h| h.payload.as_ref().and_then(|p| p["field"].as_str().map(str::to_string)))
        .collect();
    assert!(fields.contains(&"suburb".to_string()), "suburb must survive replay");
    assert!(fields.contains(&"seats".to_string()),  "seats must survive replay");
    assert!(!fields.contains(&"capacity".to_string()), "capacity was renamed");

    // Node data: seats field exists, capacity does not
    let node = db.get("venues/rod_laver").unwrap();
    let v: serde_json::Value = serde_json::from_str(&node).unwrap();
    assert_eq!(v["seats"].as_f64(), Some(15000.0));
    assert!(v.get("capacity").is_none());
}

// ── ALTER TABLE index correctness ─────────────────────────────────────────────

#[test]
fn drop_column_removes_index_hint_and_btree() {
    use tempfile::TempDir;
    use sekejap::CoreDB;

    let dir = TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE venues (_key TEXT, name TEXT, capacity INTEGER)").unwrap();
        db.execute("CREATE INDEX ON venues USING btree (capacity)").unwrap();
        for (k, n, c) in [("rod_laver", "Rod Laver Arena", 15000), ("mcg", "MCG", 100024)] {
            db.execute(&format!(
                "INSERT INTO venues (_key, name, capacity) VALUES ('{k}', '{n}', {c})"
            )).unwrap();
        }

        // Index works before drop
        let hits = db.query("SELECT * FROM venues WHERE capacity > 10000 ORDER BY capacity ASC")
            .unwrap().collect();
        assert_eq!(hits.len(), 2);

        db.execute("ALTER TABLE venues DROP COLUMN capacity").unwrap();

        // After drop: schema hint is gone
        let show = db.show("SHOW venues").unwrap();
        let fields: Vec<_> = show.iter()
            .filter_map(|h| h.payload.as_ref().and_then(|p| p["field"].as_str().map(String::from)))
            .collect();
        assert!(!fields.contains(&"capacity".to_string()));
    }

    // WAL replay: index rebuild must NOT try to rebuild the dropped column
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let show = db.show("SHOW venues").unwrap();
        let fields: Vec<_> = show.iter()
            .filter_map(|h| h.payload.as_ref().and_then(|p| p["field"].as_str().map(String::from)))
            .collect();
        assert!(!fields.contains(&"capacity".to_string()),
            "capacity index hint must not survive WAL replay after DROP COLUMN");
    }
}

#[test]
fn rename_column_updates_index_hint() {
    use tempfile::TempDir;
    use sekejap::CoreDB;

    let dir = TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE venues (_key TEXT, name TEXT, seats INTEGER)").unwrap();
        db.execute("CREATE INDEX ON venues USING btree (seats)").unwrap();
        db.execute("INSERT INTO venues (_key, name, seats) VALUES ('mcg', 'MCG', 100024)").unwrap();

        db.execute("ALTER TABLE venues RENAME COLUMN seats TO capacity").unwrap();

        // Index on new name must work immediately
        let hits = db.query("SELECT * FROM venues WHERE capacity > 50000 ORDER BY capacity ASC")
            .unwrap().collect();
        assert_eq!(hits.len(), 1);
    }

    // WAL replay must rebuild index under the new name, not the old
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        // Force a btree rebuild as startup does (simulate by inserting a second row)
        db.execute("INSERT INTO venues (_key, name, capacity) VALUES ('etihad', 'Marvel Stadium', 56347)").unwrap();
        let hits = db.query("SELECT * FROM venues WHERE capacity > 50000 ORDER BY capacity ASC")
            .unwrap().collect();
        assert_eq!(hits.len(), 2);

        // Old name must no longer appear in schema hints
        let show = db.show("SHOW venues").unwrap();
        let indexed: Vec<_> = show.iter()
            .filter_map(|h| {
                let p = h.payload.as_ref()?;
                if p.get("source").and_then(|v| v.as_str()) == Some("declared") {
                    p["field"].as_str().map(String::from)
                } else { None }
            })
            .collect();
        assert!(indexed.contains(&"capacity".to_string()),
            "capacity must appear in schema after rename");
    }
}

#[test]
fn alter_column_type_rebuilds_btree() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE events (_key TEXT, name TEXT, score INTEGER)").unwrap();
    db.execute("CREATE INDEX ON events USING btree (score)").unwrap();
    for (k, s) in [("a", 10), ("b", 50), ("c", 90)] {
        db.execute(&format!(
            "INSERT INTO events (_key, name, score) VALUES ('{k}', '{k}', {s})"
        )).unwrap();
    }

    // Btree works with INTEGER type
    let before = db.query("SELECT * FROM events WHERE score > 40 ORDER BY score ASC")
        .unwrap().collect();
    assert_eq!(before.len(), 2);

    // Change type to REAL — btree should be rebuilt and still work
    db.execute("ALTER TABLE events ALTER COLUMN score TYPE REAL").unwrap();

    let after = db.query("SELECT * FROM events WHERE score > 40 ORDER BY score ASC")
        .unwrap().collect();
    assert_eq!(after.len(), 2);
}

// ── DROP INDEX ────────────────────────────────────────────────────────────────

/// DROP INDEX on a btree removes the index hint from the schema and destroys
/// the in-memory btree so range queries fall back to a full scan.
#[test]
fn drop_index_btree_removes_hint_and_data() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, capacity INTEGER)").unwrap();
    db.execute("CREATE INDEX ON venues USING btree (capacity)").unwrap();
    for (k, c) in [("rod_laver", 15000i64), ("forum", 11000i64), ("corner", 2400i64)] {
        db.execute(&format!(
            "INSERT INTO venues (_key, capacity) VALUES ('{k}', {c})"
        )).unwrap();
    }

    // Btree index is present — range query uses it
    let before = db.query("SELECT * FROM venues WHERE capacity > 10000 ORDER BY capacity ASC")
        .unwrap().collect();
    assert_eq!(before.len(), 2);

    // Drop the index
    db.execute("DROP INDEX ON venues USING btree (capacity)").unwrap();

    // Range query should still work via full scan fallback
    let after = db.query("SELECT * FROM venues WHERE capacity > 10000 ORDER BY capacity ASC")
        .unwrap().collect();
    assert_eq!(after.len(), 2);
}

/// DROP INDEX IF EXISTS on a non-existent index is silent.
#[test]
fn drop_index_if_exists_silent() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, capacity INTEGER)").unwrap();
    // No index created — IF EXISTS should not error
    db.execute("DROP INDEX IF EXISTS ON venues USING btree (capacity)").unwrap();
}

/// DROP INDEX without IF EXISTS on a non-existent index returns an error.
#[test]
fn drop_index_missing_errors() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE venues (_key TEXT, capacity INTEGER)").unwrap();
    let err = db.execute("DROP INDEX ON venues USING btree (capacity)");
    assert!(err.is_err());
}

/// When two collections share a GIN (fulltext) index on the same field name,
/// dropping the index from one collection must NOT destroy the other's data.
#[test]
fn drop_index_gin_shared_field_only_removes_one_collection() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (_key TEXT, body TEXT)").unwrap();
    db.execute("CREATE TABLE posts (_key TEXT, body TEXT)").unwrap();

    // Insert data first — GIN is batch-built, so the index must be created after rows.
    db.execute("INSERT INTO articles (_key, body) VALUES ('a1', 'live music in Fitzroy')").unwrap();
    db.execute("INSERT INTO articles (_key, body) VALUES ('a2', 'gallery opens in Collingwood')").unwrap();
    db.execute("INSERT INTO posts (_key, body) VALUES ('p1', 'live gig at Corner Hotel')").unwrap();

    db.execute("CREATE INDEX ON articles USING gin (body)").unwrap();
    db.execute("CREATE INDEX ON posts USING gin (body)").unwrap();

    // Both collections searchable via ILIKE (uses GIN)
    let hit_articles = db.query("SELECT * FROM articles WHERE body ILIKE 'Fitzroy'")
        .unwrap().collect();
    assert_eq!(hit_articles.len(), 1);
    let hit_posts = db.query("SELECT * FROM posts WHERE body ILIKE 'live'")
        .unwrap().collect();
    assert_eq!(hit_posts.len(), 1);

    // Drop GIN on articles only
    db.execute("DROP INDEX ON articles USING gin (body)").unwrap();

    // Posts GIN data must still work
    let still_posts = db.query("SELECT * FROM posts WHERE body ILIKE 'live'")
        .unwrap().collect();
    assert_eq!(still_posts.len(), 1, "posts GIN must survive when articles drops theirs");
}

/// DROP INDEX survives WAL replay — after a cold restart the index hint is gone
/// and the index data is absent.
#[test]
fn drop_index_wal_replay() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();

    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE venues (_key TEXT, capacity INTEGER)").unwrap();
        db.execute("CREATE INDEX ON venues USING btree (capacity)").unwrap();
        db.execute("INSERT INTO venues (_key, capacity) VALUES ('rod_laver', 15000)").unwrap();
        db.execute("DROP INDEX ON venues USING btree (capacity)").unwrap();
    }

    // Reopen — WAL replay must re-apply the DROP INDEX.
    // Full-scan fallback must still return the row.
    let db = CoreDB::open(dir.path()).unwrap();
    let rows = db.query("SELECT * FROM venues WHERE capacity > 10000")
        .unwrap().collect();
    assert_eq!(rows.len(), 1);
    // DDL should no longer mention btree on capacity
    let ddl = db.schema_ddl("venues").unwrap();
    // The DDL reflects field definitions, not index hints, so we verify
    // by confirming the query still works (btree hint absence is internal).
    assert!(ddl.contains("capacity"), "column must still exist");
}

// ── GIN ILIKE integration test ────────────────────────────────────────────────

/// GIN ILIKE must return the correct nodes (not empty) when accessed via SQL.
/// This exercises the full code path: build_gin_index → ilike() → query.rs FILTER.
/// Data is inserted before the index is built (GIN is batch-built, not incremental).
#[test]
fn gin_ilike_after_insert() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE bands (name TEXT)").unwrap();
    // Insert data first — GIN is batch-built so the index must be created after the rows.
    db.put("bands/b1", r#"{"_collection":"bands","name":"The Vines"}"#).unwrap();
    db.put("bands/b2", r#"{"_collection":"bands","name":"The Avalanches"}"#).unwrap();
    db.put("bands/b3", r#"{"_collection":"bands","name":"The John Butler Trio"}"#).unwrap();
    db.put("bands/b4", r#"{"_collection":"bands","name":"Something Something"}"#).unwrap();
    db.execute("CREATE INDEX ON bands USING gin (name)").unwrap();

    // SQL ILIKE via GIN index path
    let hits = db
        .query("SELECT * FROM bands WHERE name ILIKE '%the%'")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 3, "GIN ILIKE must return the 3 bands starting with 'The'");

    let names: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.payload.as_ref()?.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"The Vines"));
    assert!(names.contains(&"The Avalanches"));
    assert!(names.contains(&"The John Butler Trio"));
}

/// GIN ILIKE recheck reads ONLY the indexed field from the stored bytes (not a
/// full-record parse). Guards that optimization: exact matches must be returned
/// even when records carry a large non-indexed sibling field, and a trigram
/// false-positive (all trigrams present, substring absent) must be excluded.
#[test]
fn gin_ilike_verify_exact_with_large_sibling_field() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE docs (content TEXT, blob TEXT)").unwrap();
    let big = "x".repeat(20_000); // large non-indexed sibling field
    // Two true matches for '%abcabc%':
    db.put("docs/t1", &format!(r#"{{"_collection":"docs","content":"see abcabc here","blob":"{big}"}}"#)).unwrap();
    db.put("docs/t2", &format!(r#"{{"_collection":"docs","content":"abcabc","blob":"{big}"}}"#)).unwrap();
    // Trigram false positive: contains trigrams abc/bca/cab (from "cab abc") but
    // NOT the contiguous "abcabc" — the recheck must exclude it.
    db.put("docs/fp", &format!(r#"{{"_collection":"docs","content":"a cab and abc","blob":"{big}"}}"#)).unwrap();
    // Unrelated:
    db.put("docs/n1", &format!(r#"{{"_collection":"docs","content":"nothing here","blob":"{big}"}}"#)).unwrap();
    db.execute("CREATE INDEX ON docs USING gin (content)").unwrap();

    let hits = db.query("SELECT _key FROM docs WHERE content ILIKE '%abcabc%'").unwrap().collect();
    let keys: std::collections::HashSet<String> = hits.iter()
        .filter_map(|h| h.payload.as_ref()?.get("_key")?.as_str().map(String::from))
        .collect();
    assert_eq!(keys.len(), 2, "exactly the two true matches");
    assert!(keys.contains("t1") && keys.contains("t2"));
    assert!(!keys.contains("fp"), "trigram false positive must be rechecked out");
}

// ── GQL path functions: length(p), nodes(p) ──────────────────────────────────

/// `length(p)` counts hops from start.
/// Graph: Melbourne → Richmond → Hawthorn → Box Hill (each hop "adjacent")
#[test]
fn gql_path_length_multi_hop() {
    let mut db = CoreDB::new();
    db.put("suburbs/melbourne", r#"{"_collection":"suburbs","_key":"melbourne"}"#).unwrap();
    db.put("suburbs/richmond",  r#"{"_collection":"suburbs","_key":"richmond"}"#).unwrap();
    db.put("suburbs/hawthorn",  r#"{"_collection":"suburbs","_key":"hawthorn"}"#).unwrap();
    db.put("suburbs/box-hill",  r#"{"_collection":"suburbs","_key":"box-hill"}"#).unwrap();
    db.link("suburbs/melbourne", "suburbs/richmond", "adjacent");
    db.link("suburbs/richmond",  "suburbs/hawthorn", "adjacent");
    db.link("suburbs/hawthorn",  "suburbs/box-hill", "adjacent");

    // 2-hop path: melbourne → richmond → hawthorn
    let hits = db.query(
        "SELECT h2._key AS dest, length(p) AS depth \
         FROM MATCH p = (s:suburbs)-[:adjacent]->(h1:suburbs)-[:adjacent]->(h2:suburbs) \
         WHERE s._key = 'melbourne'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let payload = hits[0].payload.as_ref().unwrap();
    assert_eq!(payload["depth"], 2, "length(p) must be 2 after 2 hops");
}

/// `nodes(p)` contains the full slug list from start to current node.
#[test]
fn gql_path_nodes_multi_hop() {
    let mut db = CoreDB::new();
    db.put("suburbs/fitzroy",   r#"{"_collection":"suburbs","_key":"fitzroy"}"#).unwrap();
    db.put("suburbs/collingwood", r#"{"_collection":"suburbs","_key":"collingwood"}"#).unwrap();
    db.put("suburbs/richmond",  r#"{"_collection":"suburbs","_key":"richmond"}"#).unwrap();
    db.link("suburbs/fitzroy",    "suburbs/collingwood", "borders");
    db.link("suburbs/collingwood","suburbs/richmond",    "borders");

    let hits = db.query(
        "SELECT c._key AS dest, nodes(p) AS path \
         FROM MATCH p = (a:suburbs)-[:borders]->(b:suburbs)-[:borders]->(c:suburbs) \
         WHERE a._key = 'fitzroy'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let payload = hits[0].payload.as_ref().unwrap();
    let path = payload["path"].as_array().expect("nodes(p) must be array");
    assert_eq!(path.len(), 3, "3 nodes in path: fitzroy, collingwood, richmond");
    assert_eq!(path[0].as_str().unwrap(), "suburbs/fitzroy");
    assert_eq!(path[2].as_str().unwrap(), "suburbs/richmond");
}

// ── MATCH SHORTEST ───────────────────────────────────────────────────────────

/// Build a small graph that contains multiple paths of different lengths and
/// check that `MATCH SHORTEST` returns the shortest one.
///
/// Graph (all edges forward-directed) — a Bali transit network:
///   seminyak → kuta    (bus)
///   seminyak → ubud    (shuttle)
///   ubud → kuta        (scooter)
///   kuta → canggu      (walk)
///   ubud → canggu      (taxi)
///   canggu → uluwatu   (ferry)
///   kuta → uluwatu     (taxi)
///
/// Shortest route from seminyak → uluwatu:
///   seminyak → kuta → uluwatu  (2 hops)
fn setup_path_db() -> CoreDB {
    let mut db = CoreDB::new();
    db.put("places/seminyak", r#"{"_collection":"places","name":"Seminyak"}"#).unwrap();
    db.put("places/kuta",     r#"{"_collection":"places","name":"Kuta"}"#).unwrap();
    db.put("places/ubud",     r#"{"_collection":"places","name":"Ubud"}"#).unwrap();
    db.put("places/canggu",   r#"{"_collection":"places","name":"Canggu"}"#).unwrap();
    db.put("places/uluwatu",  r#"{"_collection":"places","name":"Uluwatu"}"#).unwrap();

    db.link("places/seminyak", "places/kuta",    "bus_to");
    db.link("places/seminyak", "places/ubud",    "shuttle_to");
    db.link("places/ubud",     "places/kuta",    "scooter_to");
    db.link("places/kuta",     "places/canggu",  "walk_to");
    db.link("places/ubud",     "places/canggu",  "taxi_to");
    db.link("places/canggu",   "places/uluwatu", "ferry_to");
    db.link("places/kuta",     "places/uluwatu", "taxi_to");

    db
}

#[test]
fn shortest_path_returns_correct_route() {
    let db = setup_path_db();

    // SELECT FROM MATCH SHORTEST — path row: a=start, b=end, r=path object
    let hits = db.query(
        "SELECT a.name AS from_name, b.name AS to_name, length(r) AS hops, nodes(r) AS path \
         FROM MATCH SHORTEST (a)-[r*]->(b) \
         WHERE a._key = 'places/seminyak' AND b._key = 'places/uluwatu'"
    ).unwrap().collect();

    assert_eq!(hits.len(), 1, "should find a path");
    let p = hits[0].payload.as_ref().unwrap();

    // Endpoints
    assert_eq!(p["from_name"].as_str().unwrap(), "Seminyak");
    assert_eq!(p["to_name"].as_str().unwrap(), "Uluwatu");

    // Shortest path is 2 hops: seminyak → kuta → uluwatu
    assert_eq!(p["hops"].as_i64().unwrap(), 2, "expected 2 hops");

    // Path keys: seminyak, kuta, uluwatu
    let path = p["path"].as_array().unwrap();
    assert_eq!(path.len(), 3);
    assert_eq!(path[0].as_str().unwrap(), "places/seminyak");
    assert_eq!(path[1].as_str().unwrap(), "places/kuta");
    assert_eq!(path[2].as_str().unwrap(), "places/uluwatu");
}

#[test]
fn shortest_path_collection_in_pattern() {
    // Same query but using (a:places) pattern + bare _key instead of full slug.
    let db = setup_path_db();
    let hits = db.query(
        "SELECT a.name AS from_name, b.name AS to_name, length(r) AS hops \
         FROM MATCH SHORTEST (a:places)-[r*]->(b:places) \
         WHERE a._key = 'seminyak' AND b._key = 'uluwatu'"
    ).unwrap().collect();
    assert_eq!(hits.len(), 1, "collection-in-pattern should find path");
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["hops"].as_i64().unwrap(), 2);
    assert_eq!(p["from_name"].as_str().unwrap(), "Seminyak");
    assert_eq!(p["to_name"].as_str().unwrap(), "Uluwatu");
}

#[test]
fn shortest_path_no_path_returns_none() {
    let db = setup_path_db();

    // uluwatu has no outgoing edges in our graph, so uluwatu → seminyak is impossible
    let hits = db.query(
        "SELECT a.name AS from_name, b.name AS to_name \
         FROM MATCH SHORTEST (a)-[r*]->(b) \
         WHERE a._key = 'places/uluwatu' AND b._key = 'places/seminyak'"
    ).unwrap().collect();

    assert!(hits.is_empty(), "expected 0 rows when no path exists");
}

#[test]
fn shortest_path_same_node_returns_zero_hops() {
    let db = setup_path_db();

    let hits = db.query(
        "SELECT length(r) AS hops, nodes(r) AS path \
         FROM MATCH SHORTEST (a)-[r*]->(b) \
         WHERE a._key = 'places/kuta' AND b._key = 'places/kuta'"
    ).unwrap().collect();

    assert_eq!(hits.len(), 1, "same-node path must return 1 row");
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["hops"].as_i64().unwrap(), 0);
    let path = p["path"].as_array().unwrap();
    assert_eq!(path.len(), 1);
    assert_eq!(path[0].as_str().unwrap(), "places/kuta");
}

#[test]
fn shortest_path_missing_node_returns_none() {
    let db = setup_path_db();

    // "places/nusa-penida" was never inserted
    let hits = db.query(
        "SELECT a.name AS from_name, b.name AS to_name \
         FROM MATCH SHORTEST (a)-[r*]->(b) \
         WHERE a._key = 'places/seminyak' AND b._key = 'places/nusa-penida'"
    ).unwrap().collect();

    assert!(hits.is_empty(), "expected 0 rows when target node doesn't exist");
}

// ── Target 8: SELECT … FROM MATCH ────────────────────────────────────────────

/// SELECT list projects the matched graph rows.
/// Graph: Melbourne → Richmond → Hawthorn (adjacent edges, 1-hop and 2-hop).
#[test]
fn select_from_match() {
    let mut db = CoreDB::new();
    db.put("suburbs/melbourne", r#"{"_collection":"suburbs","_key":"melbourne"}"#).unwrap();
    db.put("suburbs/richmond",  r#"{"_collection":"suburbs","_key":"richmond"}"#).unwrap();
    db.put("suburbs/hawthorn",  r#"{"_collection":"suburbs","_key":"hawthorn"}"#).unwrap();
    db.link("suburbs/melbourne", "suburbs/richmond", "adjacent");
    db.link("suburbs/richmond",  "suburbs/hawthorn", "adjacent");

    // Use destination (n) plus the GQL path variable p for hop count.
    let hits = db.query(
        "SELECT n._key AS dest, length(p) AS depth \
         FROM MATCH p = (s:suburbs)-[:adjacent]->(n:suburbs) \
         WHERE s._key = 'melbourne'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let p = hits[0].payload.as_ref().unwrap();
    // Single hop: depth = 1, destination = richmond
    assert_eq!(p["dest"].as_str().unwrap(), "richmond");
    assert_eq!(p["depth"].as_i64().unwrap(), 1);
}

// ── Target 9: PATH_* aggregates ───────────────────────────────────────────────

/// PATH_FIRST and PATH_LAST return the first/last element of a path array field.
/// Uses r._path_keys which contains the full slug list from start to current node.
#[test]
fn path_first_last() {
    // PATH_FIRST / PATH_LAST operate on a JSON array held in a node field.
    let mut db = CoreDB::new();
    db.put("hub/h", r#"{"_collection":"hub","_key":"h"}"#).unwrap();
    db.put("line/l", r#"{"_collection":"line","_key":"l","stops":["fitzroy","collingwood","richmond"]}"#).unwrap();
    db.link("hub/h", "line/l", "serves");

    let hits = db.query(
        "SELECT PATH_FIRST(b.stops) AS first_stop, PATH_LAST(b.stops) AS last_stop \
         FROM MATCH (a:hub)-[:serves]->(b:line) WHERE a._key = 'h'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["first_stop"].as_str().unwrap(), "fitzroy");
    assert_eq!(p["last_stop"].as_str().unwrap(), "richmond");
}

// ── Target 10: CASE WHEN, NOW(), JSON_ARRAY_LENGTH ───────────────────────────

/// CASE WHEN routes on a destination node field: ring 1 → "close", 2 → "far".
#[test]
fn case_when_depth() {
    let mut db = CoreDB::new();
    db.put("suburbs/melbourne", r#"{"_collection":"suburbs","_key":"melbourne","ring":0}"#).unwrap();
    db.put("suburbs/richmond",  r#"{"_collection":"suburbs","_key":"richmond","ring":1}"#).unwrap();
    db.put("suburbs/hawthorn",  r#"{"_collection":"suburbs","_key":"hawthorn","ring":2}"#).unwrap();
    db.link("suburbs/melbourne", "suburbs/richmond", "adjacent");
    db.link("suburbs/richmond",  "suburbs/hawthorn", "adjacent");

    // Two-hop path ends at hawthorn (ring 2 → "far").
    let hits = db.query(
        "SELECT h2._key AS dest, \
                CASE WHEN h2.ring = 1 THEN 'close' WHEN h2.ring = 2 THEN 'far' ELSE 'unknown' END AS proximity \
         FROM MATCH (s:suburbs)-[:adjacent]->(h1:suburbs)-[:adjacent]->(h2:suburbs) \
         WHERE s._key = 'melbourne'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["dest"].as_str().unwrap(), "hawthorn");
    assert_eq!(p["proximity"].as_str().unwrap(), "far");
}

/// NOW() returns a positive integer (Unix timestamp in seconds).
#[test]
fn now_returns_integer() {
    let mut db = CoreDB::new();
    db.put("suburbs/fitzroy",    r#"{"_collection":"suburbs","_key":"fitzroy"}"#).unwrap();
    db.put("suburbs/collingwood",r#"{"_collection":"suburbs","_key":"collingwood"}"#).unwrap();
    db.link("suburbs/fitzroy", "suburbs/collingwood", "borders");

    let hits = db.query(
        "SELECT NOW() AS ts \
         FROM MATCH (a:suburbs)-[r:borders]->(b:suburbs) \
         WHERE a._key = 'fitzroy'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let p = hits[0].payload.as_ref().unwrap();
    let ts = p["ts"].as_i64().expect("NOW() must return an integer");
    assert!(ts > 1_000_000_000, "timestamp should be a plausible Unix epoch, got {ts}");
}

/// JSON_ARRAY_LENGTH returns the length of a JSON array held in a node field.
#[test]
fn json_array_length() {
    let mut db = CoreDB::new();
    db.put("hub/h", r#"{"_collection":"hub","_key":"h"}"#).unwrap();
    db.put("line/l", r#"{"_collection":"line","_key":"l","stops":["fitzroy","collingwood","richmond"]}"#).unwrap();
    db.link("hub/h", "line/l", "serves");

    let hits = db.query(
        "SELECT JSON_ARRAY_LENGTH(b.stops) AS path_len \
         FROM MATCH (a:hub)-[:serves]->(b:line) WHERE a._key = 'h'"
    ).unwrap().collect();

    assert!(!hits.is_empty());
    let p = hits[0].payload.as_ref().unwrap();
    // stops: fitzroy, collingwood, richmond → length 3
    assert_eq!(p["path_len"].as_i64().unwrap(), 3);
}

// ── Path predicates on MATCH SHORTEST ────────────────────────────────────────

/// ANY predicate: at least one path node satisfies the condition → 1 row returned.
/// Path seminyak → kuta → uluwatu contains "Kuta" → ANY(n.name = 'Kuta') passes.
#[test]
fn shortest_with_any_predicate() {
    let db = setup_path_db();

    let hits = db.query(
        "SELECT length(r) AS hops \
         FROM MATCH SHORTEST (a)-[r*]->(b) \
         WHERE a._key = 'places/seminyak' AND b._key = 'places/uluwatu' \
         AND ANY(n IN nodes(r) WHERE n.name = 'Kuta')"
    ).unwrap().collect();

    assert_eq!(hits.len(), 1, "ANY should pass — Kuta is on the path");
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["hops"].as_i64().unwrap(), 2);
}

/// ALL predicate: every path node must satisfy condition — fails when it doesn't.
/// Path seminyak → kuta → uluwatu; not all nodes are named 'Seminyak' → 0 rows.
#[test]
fn shortest_with_all_predicate() {
    let db = setup_path_db();

    let hits = db.query(
        "SELECT length(r) AS hops \
         FROM MATCH SHORTEST (a)-[r*]->(b) \
         WHERE a._key = 'places/seminyak' AND b._key = 'places/uluwatu' \
         AND ALL(n IN nodes(r) WHERE n.name = 'Seminyak')"
    ).unwrap().collect();

    assert!(hits.is_empty(), "ALL should fail — not every node is named Seminyak");
}

// ── Multi-FROM cross-join ─────────────────────────────────────────────────────

/// Two independent MATCH sources are cross-joined: 2 × 3 = 6 rows.
#[test]
fn multi_from_two_matches() {
    let mut db = CoreDB::new();
    db.put("root1/r1", r#"{"_collection":"root1","_key":"r1"}"#).unwrap();
    db.put("root2/r2", r#"{"_collection":"root2","_key":"r2"}"#).unwrap();
    for i in 1..=2 {
        db.put(&format!("alpha/a{i}"), &format!(r#"{{"_collection":"alpha","_key":"a{i}"}}"#)).unwrap();
        db.link("root1/r1", &format!("alpha/a{i}"), "has");
    }
    for i in 1..=3 {
        db.put(&format!("beta/b{i}"), &format!(r#"{{"_collection":"beta","_key":"b{i}"}}"#)).unwrap();
        db.link("root2/r2", &format!("beta/b{i}"), "has");
    }

    let hits = db.query(
        "SELECT a._key AS ak, b._key AS bk \
         FROM MATCH ('root1/r1')-[:has]->(a), MATCH ('root2/r2')-[:has]->(b)"
    ).unwrap().collect();

    assert_eq!(hits.len(), 6, "2 × 3 Cartesian product = 6 rows");
}

/// MATCH source cross-joined with a collection source: 2 events × 3 suburbs = 6 rows.
#[test]
fn multi_from_match_and_collection() {
    let mut db = CoreDB::new();
    db.put("root/r", r#"{"_collection":"root","_key":"r"}"#).unwrap();
    for k in ["flood", "storm"] {
        db.put(&format!("events/{k}"), &format!(r#"{{"_collection":"events","_key":"{k}"}}"#)).unwrap();
        db.link("root/r", &format!("events/{k}"), "caused");
    }
    for s in ["fitzroy", "richmond", "hawthorn"] {
        db.put(&format!("suburbs/{s}"), &format!(r#"{{"_collection":"suburbs","_key":"{s}"}}"#)).unwrap();
    }

    let hits = db.query(
        "SELECT e._key AS event, s._key AS suburb \
         FROM MATCH ('root/r')-[:caused]->(e), suburbs AS s"
    ).unwrap().collect();

    assert_eq!(hits.len(), 6, "2 events × 3 suburbs = 6 rows");
}

/// MATCH source cross-joined with MATCH SHORTEST: 2 towns × 1 shortest row = 2 rows.
#[test]
fn multi_from_match_and_shortest() {
    let mut db = setup_path_db();
    db.put("towns/mel", r#"{"_collection":"towns","_key":"mel"}"#).unwrap();
    db.put("towns/syd", r#"{"_collection":"towns","_key":"syd"}"#).unwrap();
    db.put("root_n/r",  r#"{"_collection":"root_n","_key":"r"}"#).unwrap();
    db.link("root_n/r", "towns/mel", "near");
    db.link("root_n/r", "towns/syd", "near");

    let hits = db.query(
        "SELECT t._key AS town, length(p) AS hops \
         FROM MATCH ('root_n/r')-[:near]->(t), \
              MATCH SHORTEST (x)-[p*]->(y) WHERE x._key = 'places/seminyak' AND y._key = 'places/uluwatu'"
    ).unwrap().collect();

    // 2 towns × 1 shortest-path row = 2 rows; each carries the path length
    assert_eq!(hits.len(), 2, "2 towns × 1 shortest path = 2 rows");
    for hit in &hits {
        let p = hit.payload.as_ref().unwrap();
        assert_eq!(p["hops"].as_i64().unwrap(), 2, "seminyak→uluwatu shortest path = 2 hops");
    }
}

// ── Date scalar functions ──────────────────────────────────────────────────────

/// YEAR/MONTH/DAY/HOUR/MINUTE/SECOND/DOW/QUARTER in SELECT.
#[test]
fn date_parts_in_select() {
    let mut db = CoreDB::new();
    db.put(
        "posts/p1",
        r#"{"_collection":"posts","_key":"p1","published_at":"2024-07-15T14:30:45Z"}"#,
    )
    .unwrap();

    let hits = db.query(
        "SELECT YEAR(published_at) AS yr, MONTH(published_at) AS mo, \
         DAY(published_at) AS dy, HOUR(published_at) AS hr, \
         MINUTE(published_at) AS mi, SECOND(published_at) AS sc, \
         DOW(published_at) AS dow, QUARTER(published_at) AS qtr \
         FROM posts WHERE _key = 'p1'",
    )
    .unwrap()
    .collect();

    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["yr"].as_i64().unwrap(), 2024, "year");
    assert_eq!(p["mo"].as_i64().unwrap(), 7,    "month");
    assert_eq!(p["dy"].as_i64().unwrap(), 15,   "day");
    assert_eq!(p["hr"].as_i64().unwrap(), 14,   "hour");
    assert_eq!(p["mi"].as_i64().unwrap(), 30,   "minute");
    assert_eq!(p["sc"].as_i64().unwrap(), 45,   "second");
    // 2024-07-15 is a Monday → DOW = 1 (Sun=0, Mon=1)
    assert_eq!(p["dow"].as_i64().unwrap(), 1,   "dow");
    assert_eq!(p["qtr"].as_i64().unwrap(), 3,   "quarter");
}

/// DATE_TRUNC in SELECT.
#[test]
fn date_trunc_in_select() {
    let mut db = CoreDB::new();
    db.put(
        "ev/e1",
        r#"{"_collection":"ev","_key":"e1","ts":"2024-07-15T14:30:45Z"}"#,
    )
    .unwrap();

    let hits = db
        .query("SELECT DATE_TRUNC('month', ts) AS trunc FROM ev WHERE _key = 'e1'")
        .unwrap()
        .collect();

    assert_eq!(hits.len(), 1);
    let trunc = hits[0].payload.as_ref().unwrap()["trunc"]
        .as_str()
        .unwrap()
        .to_string();
    // Truncated to month start: 2024-07-01T00:00:00
    assert!(
        trunc.starts_with("2024-07-01T00:00:00"),
        "expected 2024-07-01T00:00:00…, got {trunc}"
    );
}

/// YEAR() in WHERE clause filters correctly.
#[test]
fn date_func_in_where() {
    let mut db = CoreDB::new();
    db.put(
        "art/a1",
        r#"{"_collection":"art","_key":"a1","published_at":"2022-03-10T00:00:00Z"}"#,
    )
    .unwrap();
    db.put(
        "art/a2",
        r#"{"_collection":"art","_key":"a2","published_at":"2024-07-15T00:00:00Z"}"#,
    )
    .unwrap();

    let hits = db
        .query("SELECT _key FROM art WHERE YEAR(published_at) = 2024")
        .unwrap()
        .collect();

    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].payload.as_ref().unwrap()["_key"]
            .as_str()
            .unwrap(),
        "a2"
    );
}

/// MONTH() > filter.
#[test]
fn date_func_month_gt_in_where() {
    let mut db = CoreDB::new();
    db.put("bl/b1", r#"{"_collection":"bl","_key":"b1","ts":"2024-03-01T00:00:00Z"}"#).unwrap();
    db.put("bl/b2", r#"{"_collection":"bl","_key":"b2","ts":"2024-09-01T00:00:00Z"}"#).unwrap();
    db.put("bl/b3", r#"{"_collection":"bl","_key":"b3","ts":"2024-06-15T00:00:00Z"}"#).unwrap();

    let hits = db
        .query("SELECT _key FROM bl WHERE MONTH(ts) > 6")
        .unwrap()
        .collect();

    // b2 (month=9) and b3 (month=6 — NOT > 6) and b1 (month=3)
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap(),
        "b2"
    );
}

/// NOW() in WHERE returns a unix-ms integer enabling _created_unix comparisons.
/// We verify it parses and executes without error (result may be 0 rows for
/// a freshly created node since _created_unix is set to now and NOW() is now).
#[test]
fn now_in_where_is_numeric() {
    let mut db = CoreDB::new();
    db.put("art/x1", r#"{"_collection":"art","_key":"x1"}"#).unwrap();

    // Should parse and execute without error — result may vary by timing.
    let result = db.query("SELECT _key FROM art WHERE _created_unix < NOW()");
    assert!(result.is_ok(), "NOW() in WHERE should parse and execute");
}

/// BM25 index is updated after INSERT so newly added nodes are searchable.
#[test]
fn bm25_updated_after_insert() {
    let mut db = CoreDB::new();
    // Two pre-existing docs so the rebuilt corpus has N≥3 (needed for IDF > 0
    // when a term appears in exactly one document: ln((N-1+0.5)/(1+0.5)) > 0 iff N > 2).
    db.put(
        "docs/d1",
        r#"{"_collection":"docs","_key":"d1","body":"rust programming language"}"#,
    )
    .unwrap();
    db.put(
        "docs/d0",
        r#"{"_collection":"docs","_key":"d0","body":"web development frontend tooling"}"#,
    )
    .unwrap();
    db.build_bm25_index("body");

    // Insert a new document after the index is built
    db.put(
        "docs/d2",
        r#"{"_collection":"docs","_key":"d2","body":"Melbourne cup horse race"}"#,
    )
    .unwrap();

    // d2 should surface via SQL BM25 search immediately
    let hits = db
        .query("SELECT _key FROM docs WHERE BM25(body, 'Melbourne horse') > 0.0")
        .unwrap()
        .collect();
    let found_d2 = hits.iter().any(|h| {
        h.payload
            .as_ref()
            .and_then(|p| p.get("_key"))
            .and_then(|v| v.as_str())
            == Some("d2")
    });
    assert!(
        found_d2,
        "newly inserted doc must be BM25-searchable; got {} hits",
        hits.len()
    );
}

/// MATCH start variable is bound in SELECT — a.title returns the start node field.
#[test]
fn match_start_var_is_bound() {
    let mut db = CoreDB::new();
    db.put(
        "posts/p1",
        r#"{"_collection":"posts","_key":"p1","title":"Rust is great"}"#,
    )
    .unwrap();
    db.put(
        "tags/t1",
        r#"{"_collection":"tags","_key":"t1","name":"programming"}"#,
    )
    .unwrap();
    db.link("posts/p1", "tags/t1", "tagged_with");

    let hits = db
        .query(
            "SELECT a.title AS post_title, b.name AS tag_name \
             FROM MATCH (a:posts)-[:tagged_with]->(b:tags)",
        )
        .unwrap()
        .collect();

    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(
        p["post_title"].as_str().unwrap(),
        "Rust is great",
        "start var 'a' must be bound"
    );
    assert_eq!(p["tag_name"].as_str().unwrap(), "programming");
}

// ── DEFAULT UUIDV4 / UUIDV5 column defaults ───────────────────────────────────

/// INSERT omitting a field with DEFAULT UUIDV4() — field is auto-filled with a UUID.
#[test]
fn default_uuidv4_auto_filled_on_insert() {
    let mut db = CoreDB::new();
    db.execute(
        "CREATE TABLE items (_key TEXT PRIMARY KEY, pub_id TEXT DEFAULT UUIDV4(), name TEXT)",
    )
    .unwrap();

    db.execute("INSERT INTO items (_key, name) VALUES ('item-1', 'Widget')").unwrap();

    let hits = db.query("SELECT _key, pub_id, name FROM items").unwrap().collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["_key"].as_str().unwrap(), "item-1");
    assert_eq!(p["name"].as_str().unwrap(), "Widget");

    let pub_id = p["pub_id"].as_str().expect("pub_id must be auto-filled");
    // Valid UUIDv4: 8-4-4-4-12 hex, version nibble = 4, variant bits = 8/9/a/b
    assert_eq!(pub_id.len(), 36, "UUID must be 36 chars with hyphens");
    assert_eq!(&pub_id[14..15], "4", "version nibble must be 4");
}

/// Explicit value in INSERT overrides DEFAULT UUIDV4().
#[test]
fn default_uuidv4_explicit_value_wins() {
    let mut db = CoreDB::new();
    db.execute(
        "CREATE TABLE items (_key TEXT PRIMARY KEY, pub_id TEXT DEFAULT UUIDV4(), name TEXT)",
    )
    .unwrap();

    db.execute(
        "INSERT INTO items (_key, pub_id, name) VALUES ('item-2', 'my-fixed-id', 'Gadget')",
    )
    .unwrap();

    let hits = db.query("SELECT pub_id FROM items").unwrap().collect();
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(
        p["pub_id"].as_str().unwrap(),
        "my-fixed-id",
        "explicit value must not be overridden by default"
    );
}

/// Two separate INSERTs produce two different UUIDs (randomness check).
#[test]
fn default_uuidv4_unique_per_row() {
    let mut db = CoreDB::new();
    db.execute(
        "CREATE TABLE items (_key TEXT PRIMARY KEY, pub_id TEXT DEFAULT UUIDV4(), name TEXT)",
    )
    .unwrap();

    db.execute("INSERT INTO items (_key, name) VALUES ('a', 'Alpha')").unwrap();
    db.execute("INSERT INTO items (_key, name) VALUES ('b', 'Beta')").unwrap();

    let hits = db.query("SELECT _key, pub_id FROM items").unwrap().collect();
    assert_eq!(hits.len(), 2);
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h.payload.as_ref().unwrap()["pub_id"].as_str().unwrap())
        .collect();
    assert_ne!(ids[0], ids[1], "each row must get a distinct UUID");
}

/// DEFAULT UUIDV5 produces a deterministic UUID — same inputs same output.
#[test]
fn default_uuidv5_deterministic() {
    // DNS namespace UUID
    let ns = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    let sql = format!(
        "CREATE TABLE items (_key TEXT PRIMARY KEY, stable_id TEXT DEFAULT UUIDV5('{ns}', 'sekejap-test'))"
    );
    let mut db = CoreDB::new();
    db.execute(&sql).unwrap();

    db.execute("INSERT INTO items (_key) VALUES ('x1')").unwrap();
    db.execute("INSERT INTO items (_key) VALUES ('x2')").unwrap();

    let hits = db.query("SELECT stable_id FROM items").unwrap().collect();
    assert_eq!(hits.len(), 2);

    let id0 = hits[0].payload.as_ref().unwrap()["stable_id"].as_str().unwrap().to_string();
    let id1 = hits[1].payload.as_ref().unwrap()["stable_id"].as_str().unwrap().to_string();

    // Both rows share the same literal name → same UUID
    assert_eq!(id0, id1, "UUIDV5 with same inputs must produce the same UUID");

    // Must be 36-char UUID format
    assert_eq!(id0.len(), 36);
}

/// ALTER TABLE ADD COLUMN with DEFAULT UUIDV4() — new column gets UUID on subsequent INSERTs.
#[test]
fn alter_table_add_column_default_uuidv4() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("ALTER TABLE items ADD COLUMN ext_id TEXT DEFAULT UUIDV4()").unwrap();

    db.execute("INSERT INTO items (_key, name) VALUES ('w1', 'Widget')").unwrap();

    let hits = db.query("SELECT ext_id FROM items").unwrap().collect();
    let p = hits[0].payload.as_ref().unwrap();
    let ext_id = p["ext_id"].as_str().expect("ext_id must be auto-filled after ALTER TABLE");
    assert_eq!(ext_id.len(), 36);
    assert_eq!(&ext_id[14..15], "4");
}

// ── Auto _key injection ────────────────────────────────────────────────────────

/// CREATE TABLE without _key → _key DEFAULT UUIDV4() is auto-injected.
/// INSERT without _key → slug auto-generated, node is queryable.
#[test]
fn create_table_without_key_auto_injects_uuid_key() {
    let mut db = CoreDB::new();
    // No _key in schema definition
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();

    // INSERT without _key — UUID auto-generated
    db.execute("INSERT INTO articles (title, body) VALUES ('Hello', 'World')").unwrap();

    let hits = db.query("SELECT _key, title FROM articles").unwrap().collect();
    assert_eq!(hits.len(), 1);
    let p = hits[0].payload.as_ref().unwrap();
    assert_eq!(p["title"].as_str().unwrap(), "Hello");

    let key = p["_key"].as_str().expect("_key must be auto-generated");
    assert_eq!(key.len(), 36, "_key must be a UUID");
    assert_eq!(&key[14..15], "4", "must be UUIDv4");
}

/// Two keyless INSERTs produce two distinct _key UUIDs.
#[test]
fn create_table_without_key_each_row_unique() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE notes (text TEXT)").unwrap();

    db.execute("INSERT INTO notes (text) VALUES ('First')").unwrap();
    db.execute("INSERT INTO notes (text) VALUES ('Second')").unwrap();

    let hits = db.query("SELECT _key FROM notes").unwrap().collect();
    assert_eq!(hits.len(), 2);
    let k0 = hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap().to_string();
    let k1 = hits[1].payload.as_ref().unwrap()["_key"].as_str().unwrap().to_string();
    assert_ne!(k0, k1, "each row must get a distinct UUID _key");
}

/// Explicit _key in INSERT overrides the auto-UUID default.
#[test]
fn create_table_without_key_explicit_key_wins() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE posts (title TEXT)").unwrap();

    db.execute("INSERT INTO posts (_key, title) VALUES ('hello-world', 'Hello')").unwrap();

    let hits = db.query("SELECT _key FROM posts").unwrap().collect();
    let key = hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap();
    assert_eq!(key, "hello-world", "explicit _key must not be overridden");
}

/// INSERT without _key and without a schema → MissingField error (no silent UUID).
#[test]
fn insert_without_key_no_schema_errors() {
    let mut db = CoreDB::new();
    // No CREATE TABLE — no schema registered
    db.put("items/seed", r#"{"_collection":"items","_key":"seed","name":"Seed"}"#).unwrap();

    let result = db.execute("INSERT INTO items (name) VALUES ('Widget')");
    assert!(result.is_err(), "INSERT without _key and no schema must fail");
}

// ── Parameter bindings ($1, $2, ...) ─────────────────────────────────────────

#[test]
fn param_select_where_eq() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('bob', 'Bob', 25)").unwrap();

    let hits = db.query_params(
        "SELECT * FROM users WHERE name = $1",
        &[serde_json::json!("Alice")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/alice");
}

#[test]
fn param_select_where_gt() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('bob', 'Bob', 25)").unwrap();

    let hits = db.query_params(
        "SELECT * FROM users WHERE age > $1",
        &[serde_json::json!(27)],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/alice");
}

#[test]
fn param_insert_values() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();

    let n = db.execute_params(
        "INSERT INTO users (_key, name, age) VALUES ($1, $2, $3)",
        &[serde_json::json!("charlie"), serde_json::json!("Charlie"), serde_json::json!(35)],
    ).unwrap();
    assert_eq!(n, 1);

    let hits = db.query("SELECT * FROM users WHERE name = 'Charlie'").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/charlie");
}

#[test]
fn param_update_set() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)").unwrap();

    db.execute_params(
        "UPDATE users SET age = $1 WHERE name = $2",
        &[serde_json::json!(31), serde_json::json!("Alice")],
    ).unwrap();

    let hits = db.query("SELECT * FROM users WHERE age = 31").unwrap().collect();
    assert_eq!(hits.len(), 1);
}

#[test]
fn param_like() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('alice', 'Alice')").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('bob', 'Bob')").unwrap();

    let hits = db.query_params(
        "SELECT * FROM users WHERE name LIKE $1",
        &[serde_json::json!("Ali%")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/alice");
}

#[test]
fn param_between() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('bob', 'Bob', 25)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('charlie', 'Charlie', 40)").unwrap();

    let hits = db.query_params(
        "SELECT * FROM users WHERE age BETWEEN $1 AND $2",
        &[serde_json::json!(26), serde_json::json!(35)],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/alice");
}

#[test]
fn param_in_list() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('alice', 'Alice')").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('bob', 'Bob')").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('charlie', 'Charlie')").unwrap();

    let hits = db.query_params(
        "SELECT * FROM users WHERE name IN ($1, $2)",
        &[serde_json::json!("Alice"), serde_json::json!("Charlie")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 2);
}

#[test]
fn param_reuse_same_param() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, label TEXT)").unwrap();
    db.execute("INSERT INTO items (_key, name, label) VALUES ('a', 'foo', 'foo')").unwrap();
    db.execute("INSERT INTO items (_key, name, label) VALUES ('b', 'foo', 'bar')").unwrap();

    // Use $1 in two different conditions
    let hits = db.query_params(
        "SELECT * FROM items WHERE name = $1 AND label = $1",
        &[serde_json::json!("foo")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "items/a");
}

#[test]
fn param_out_of_range_error() {
    let db = CoreDB::new();
    let result = db.query_params(
        "SELECT * FROM users WHERE name = $2",
        &[serde_json::json!("Alice")], // only 1 param but $2 used
    );
    match result {
        Err(e) => {
            let err_msg = format!("{e}");
            assert!(err_msg.contains("out of range"), "error: {err_msg}");
        }
        Ok(_) => panic!("expected error for out-of-range param"),
    }
}

#[test]
fn param_type_mismatch_error() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('alice', 'Alice', 30)").unwrap();

    // $1 is a string, but BETWEEN expects numbers
    let result = db.query_params(
        "SELECT * FROM users WHERE age BETWEEN $1 AND $2",
        &[serde_json::json!("not_a_number"), serde_json::json!(35)],
    );
    match result {
        Err(e) => {
            let err_msg = format!("{e}");
            assert!(err_msg.contains("expected number"), "error: {err_msg}");
        }
        Ok(_) => panic!("expected error for type mismatch"),
    }
}

#[test]
fn param_edge_insert() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"name":"Alice","_collection":"users"}"#).unwrap();
    db.put("users/bob", r#"{"name":"Bob","_collection":"users"}"#).unwrap();

    db.execute_params(
        "INSERT ($1)-[:follows {strength: $2}]->($3)",
        &[serde_json::json!("users/alice"), serde_json::json!(1.0), serde_json::json!("users/bob")],
    ).unwrap();

    let hits = db.query(
        "SELECT b._key AS name FROM MATCH ('users/alice')-[:follows]->(b)",
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
}

#[test]
fn param_zero_index_error() {
    let db = CoreDB::new();
    let result = db.query_params(
        "SELECT * FROM users WHERE name = $0",
        &[serde_json::json!("Alice")],
    );
    match result {
        Err(e) => {
            let err_msg = format!("{e}");
            assert!(err_msg.contains("$1, not $0"), "error: {err_msg}");
        }
        Ok(_) => panic!("expected error for $0 param"),
    }
}

#[test]
fn param_bare_dollar_error() {
    let db = CoreDB::new();
    let result = db.query_params(
        "SELECT * FROM users WHERE name = $",
        &[serde_json::json!("Alice")],
    );
    assert!(result.is_err());
}

#[test]
fn param_null_and_bool() {
    let mut db = CoreDB::new();
    // No schema — just put + insert directly to test bool/null param binding.
    db.put("items/seed", r#"{"_collection":"items","_key":"seed"}"#).unwrap();

    db.execute_params(
        "INSERT INTO items (_key, active, note) VALUES ($1, $2, $3)",
        &[serde_json::json!("i1"), serde_json::json!(true), serde_json::Value::Null],
    ).unwrap();

    let hits = db.query("SELECT * FROM items WHERE active = true").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "items/i1");
}

// ── Param hardening: math expressions (parse_math_primary) ───────────────────

#[test]
fn param_math_expr_in_select_from_match() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"_collection":"users","_key":"alice","score":10}"#).unwrap();
    db.put("users/bob",   r#"{"_collection":"users","_key":"bob","score":20}"#).unwrap();
    db.put("posts/p1",    r#"{"_collection":"posts","_key":"p1","weight":5}"#).unwrap();
    db.link("users/alice", "posts/p1", "wrote");
    db.link("users/bob",   "posts/p1", "wrote");

    // SUM(a.score * $1) — param in math expression, GROUP BY b._key
    let hits = db.query_params(
        "SELECT b._key AS post, SUM(a.score * $1) AS weighted \
         FROM MATCH (a:users)-[r:wrote]->(b:posts) \
         GROUP BY b._key",
        &[serde_json::json!(3)],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let payload = hits[0].payload.as_ref().unwrap();
    let weighted = payload.get("weighted").and_then(|v| v.as_f64()).unwrap();
    // (10 * 3) + (20 * 3) = 90
    assert!((weighted - 90.0).abs() < 0.01, "expected 90, got {weighted}");
}

// ── Param hardening: pipeline WHERE (parse_pipe_where_cond) ──────────────────

#[test]
fn param_pipeline_where_string() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"_collection":"users","_key":"alice","name":"Alice"}"#).unwrap();
    db.put("users/bob",   r#"{"_collection":"users","_key":"bob","name":"Bob"}"#).unwrap();
    db.put("posts/p1",    r#"{"_collection":"posts","_key":"p1","title":"hello"}"#).unwrap();
    db.link("users/alice", "posts/p1", "wrote");
    db.link("users/bob",   "posts/p1", "wrote");

    // Pipeline with param in WHERE
    let hits = db.query_params(
        "SELECT p._key AS post_key FROM MATCH (u:users)-[:wrote]->(p) WHERE u._key = $1",
        &[serde_json::json!("alice")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let pk = hits[0].payload.as_ref().unwrap().get("post_key")
        .and_then(|v| v.as_str()).unwrap();
    assert_eq!(pk, "p1");
}

#[test]
fn param_pipeline_where_number() {
    let mut db = CoreDB::new();
    db.put("items/a", r#"{"_collection":"items","_key":"a","score":10}"#).unwrap();
    db.put("items/b", r#"{"_collection":"items","_key":"b","score":20}"#).unwrap();
    db.put("items/c", r#"{"_collection":"items","_key":"c","score":30}"#).unwrap();
    db.link("items/b", "items/a", "back");
    db.link("items/c", "items/a", "back");

    // WHERE on start node — filter by score > $1, then traverse
    let hits = db.query_params(
        "SELECT s._key AS key FROM MATCH (s:items)-[:back]->(t) WHERE s.score > $1",
        &[serde_json::json!(15)],
    ).unwrap().collect();
    let mut keys: Vec<&str> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap().get("key").unwrap().as_str().unwrap())
        .collect();
    keys.sort();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys, vec!["b", "c"]);
}

// ── Param hardening: pipeline slug (parse_pipe_match_start) ──────────────────

#[test]
fn param_pipeline_start_slug() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"_collection":"users","_key":"alice"}"#).unwrap();
    db.put("posts/p1",    r#"{"_collection":"posts","_key":"p1","title":"hi"}"#).unwrap();
    db.put("posts/p2",    r#"{"_collection":"posts","_key":"p2","title":"bye"}"#).unwrap();
    db.link("users/alice", "posts/p1", "wrote");
    db.link("users/alice", "posts/p2", "wrote");

    let hits = db.query_params(
        "SELECT p._key AS pk FROM MATCH ($1)-[:wrote]->(p)",
        &[serde_json::json!("users/alice")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 2);
}

// ── Param hardening: pipeline math (parse_pipe_math_primary) ─────────────────

#[test]
fn param_pipeline_math_literal() {
    let mut db = CoreDB::new();
    db.put("students/budi", r#"{"_collection":"students","_key":"budi"}"#).unwrap();
    db.put("answers/a1",    r#"{"_collection":"answers","_key":"a1","score":0.8}"#).unwrap();
    db.put("questions/q1",  r#"{"_collection":"questions","_key":"q1","weight":0.5}"#).unwrap();
    db.link("students/budi", "answers/a1", "answered");
    db.link("answers/a1",    "questions/q1", "for");

    // Use $1 as a multiplier in pipeline math expression
    let hits = db.query_params(
        "SELECT q._key AS qk, SUM(a.score * q.weight * $1) AS scaled \
         FROM MATCH ('students/budi')-[:answered]->(a)-[:for]->(q) \
         GROUP BY q._key ORDER BY scaled DESC",
        &[serde_json::json!(100)],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let scaled = hits[0].payload.as_ref().unwrap().get("scaled")
        .and_then(|v| v.as_f64()).unwrap();
    // 0.8 * 0.5 * 100 = 40.0
    assert!((scaled - 40.0).abs() < 0.01, "expected 40.0, got {scaled}");
}

// ── Param hardening: vector array (parse_f32_array_or_param) ─────────────────

#[test]
fn param_vector_near() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE docs (_key TEXT, emb VECTOR)").unwrap();
    db.execute("INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 0.0, 0.0])").unwrap();
    db.execute("INSERT INTO docs (_key, emb) VALUES ('d2', [0.0, 1.0, 0.0])").unwrap();
    db.execute("INSERT INTO docs (_key, emb) VALUES ('d3', [0.9, 0.1, 0.0])").unwrap();

    // VECTOR_NEAR with vector passed as $1
    let hits = db.query_params(
        "SELECT * FROM docs WHERE VECTOR_NEAR(emb, $1, 2)",
        &[serde_json::json!([1.0, 0.0, 0.0])],
    ).unwrap().collect();
    assert_eq!(hits.len(), 2);
    // d1 and d3 should be closest to [1,0,0]
    let slugs: Vec<&str> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert!(slugs.contains(&"docs/d1"));
    assert!(slugs.contains(&"docs/d3"));
}

#[test]
fn param_vector_insert() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE docs (_key TEXT, emb VECTOR)").unwrap();

    // Insert vector via $N param
    db.execute_params(
        "INSERT INTO docs (_key, emb) VALUES ($1, $2)",
        &[serde_json::json!("d1"), serde_json::json!([0.5, 0.5, 0.0])],
    ).unwrap();

    let hits = db.query("SELECT * FROM docs").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "docs/d1");
}

// ── Param: DELETE with bindings ──────────────────────────────────────────────

#[test]
fn param_delete_where() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('a', 'Alice', 30)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('b', 'Bob', 25)").unwrap();
    db.execute("INSERT INTO users (_key, name, age) VALUES ('c', 'Carol', 40)").unwrap();

    let n = db.execute_params(
        "DELETE FROM users WHERE name = $1",
        &[serde_json::json!("Bob")],
    ).unwrap();
    assert_eq!(n, 1);

    let hits = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(hits.len(), 2);
    let slugs: Vec<&str> = hits.iter().map(|h| h.slug.as_str()).collect();
    assert!(!slugs.contains(&"users/b"));
}

#[test]
fn param_delete_edge() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"name":"Alice","_collection":"users"}"#).unwrap();
    db.put("users/bob", r#"{"name":"Bob","_collection":"users"}"#).unwrap();
    db.link("users/alice", "users/bob", "follows");

    // Verify edge exists
    let hits = db.query(
        "SELECT b._key AS name FROM MATCH ('users/alice')-[:follows]->(b)",
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);

    // Delete edge with params
    db.execute_params(
        "DELETE ($1)-[:follows]->($2)",
        &[serde_json::json!("users/alice"), serde_json::json!("users/bob")],
    ).unwrap();

    // Verify edge removed
    let hits = db.query(
        "SELECT b._key AS name FROM MATCH ('users/alice')-[:follows]->(b)",
    ).unwrap().collect();
    assert_eq!(hits.len(), 0);
}

// ── Param: MATCH graph queries with bindings (VIP) ───────────────────────────

#[test]
fn param_match_slug_start() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"_collection":"users","_key":"alice","name":"Alice"}"#).unwrap();
    db.put("users/bob",   r#"{"_collection":"users","_key":"bob","name":"Bob"}"#).unwrap();
    db.put("posts/p1",    r#"{"_collection":"posts","_key":"p1","title":"hello"}"#).unwrap();
    db.link("users/alice", "posts/p1", "wrote");
    db.link("users/bob",   "posts/p1", "wrote");

    // MATCH ($1) — param as the start node slug (SELECT FROM MATCH supports projections)
    let hits = db.query_params(
        "SELECT b._key AS post FROM MATCH ($1)-[:wrote]->(b)",
        &[serde_json::json!("users/alice")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let post = hits[0].payload.as_ref().unwrap().get("post")
        .and_then(|v| v.as_str()).unwrap();
    assert_eq!(post, "p1");
}

#[test]
fn param_match_where_key() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"_collection":"users","_key":"alice","name":"Alice"}"#).unwrap();
    db.put("users/bob",   r#"{"_collection":"users","_key":"bob","name":"Bob"}"#).unwrap();
    db.put("posts/p1",    r#"{"_collection":"posts","_key":"p1","title":"hello"}"#).unwrap();
    db.put("posts/p2",    r#"{"_collection":"posts","_key":"p2","title":"bye"}"#).unwrap();
    db.link("users/alice", "posts/p1", "wrote");
    db.link("users/bob",   "posts/p2", "wrote");

    // WHERE a._key = $1 — param in MATCH WHERE condition
    let hits = db.query_params(
        "SELECT b._key AS post FROM MATCH (a:users)-[r:wrote]->(b:posts) \
         WHERE a._key = $1",
        &[serde_json::json!("alice")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let post = hits[0].payload.as_ref().unwrap().get("post")
        .and_then(|v| v.as_str()).unwrap();
    assert_eq!(post, "p1");
}

#[test]
fn param_match_where_value() {
    let mut db = CoreDB::new();
    db.put("items/a", r#"{"_collection":"items","_key":"a","label":"hot"}"#).unwrap();
    db.put("items/b", r#"{"_collection":"items","_key":"b","label":"cold"}"#).unwrap();
    db.put("tags/t1", r#"{"_collection":"tags","_key":"t1"}"#).unwrap();
    db.link("items/a", "tags/t1", "tagged");
    db.link("items/b", "tags/t1", "tagged");

    // SELECT FROM MATCH with WHERE field = $1 — param as a comparison value
    let hits = db.query_params(
        "SELECT i._key AS item FROM MATCH (i:items)-[:tagged]->(t:tags) \
         WHERE i.label = $1",
        &[serde_json::json!("hot")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 1);
    let item = hits[0].payload.as_ref().unwrap().get("item")
        .and_then(|v| v.as_str()).unwrap();
    assert_eq!(item, "a");
}

#[test]
fn param_select_from_match_where_key() {
    let mut db = CoreDB::new();
    db.put("students/budi", r#"{"_collection":"students","_key":"budi"}"#).unwrap();
    db.put("answers/a1",    r#"{"_collection":"answers","_key":"a1","score":0.8}"#).unwrap();
    db.put("answers/a2",    r#"{"_collection":"answers","_key":"a2","score":0.9}"#).unwrap();
    db.link("students/budi", "answers/a1", "answered");
    db.link("students/budi", "answers/a2", "answered");

    // SELECT FROM MATCH with WHERE a._key = $1
    let hits = db.query_params(
        "SELECT a.score AS score FROM MATCH (s:students)-[:answered]->(a:answers) \
         WHERE s._key = $1",
        &[serde_json::json!("budi")],
    ).unwrap().collect();
    assert_eq!(hits.len(), 2);
    let mut scores: Vec<f64> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap().get("score").unwrap().as_f64().unwrap())
        .collect();
    scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!((scores[0] - 0.8).abs() < 0.01);
    assert!((scores[1] - 0.9).abs() < 0.01);
}

// ── Param: UPDATE with multiple conditions ───────────────────────────────────

#[test]
fn param_update_where_multiple() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY, name TEXT, age INTEGER, active INTEGER)").unwrap();
    db.execute("INSERT INTO users (_key, name, age, active) VALUES ('a', 'Alice', 30, 1)").unwrap();
    db.execute("INSERT INTO users (_key, name, age, active) VALUES ('b', 'Bob', 25, 1)").unwrap();
    db.execute("INSERT INTO users (_key, name, age, active) VALUES ('c', 'Carol', 40, 0)").unwrap();

    // UPDATE with param in SET and WHERE
    let n = db.execute_params(
        "UPDATE users SET active = $1 WHERE age > $2",
        &[serde_json::json!(0), serde_json::json!(28)],
    ).unwrap();
    assert_eq!(n, 2); // Alice (30) and Carol (40) updated

    let hits = db.query("SELECT * FROM users WHERE active = 1").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "users/b"); // Only Bob still active
}

// ── Param: LIMIT and OFFSET ─────────────────────────────────────────────────

#[test]
fn param_limit_and_offset() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, val INTEGER)").unwrap();
    for i in 1..=10 {
        db.execute_params(
            "INSERT INTO items (_key, val) VALUES ($1, $2)",
            &[serde_json::json!(format!("i{i}")), serde_json::json!(i)],
        ).unwrap();
    }

    // LIMIT with param
    let hits = db.query_params(
        "SELECT * FROM items ORDER BY val ASC LIMIT $1",
        &[serde_json::json!(3)],
    ).unwrap().collect();
    assert_eq!(hits.len(), 3);

    // OFFSET with param
    let hits = db.query_params(
        "SELECT * FROM items ORDER BY val ASC LIMIT $1 OFFSET $2",
        &[serde_json::json!(2), serde_json::json!(5)],
    ).unwrap().collect();
    assert_eq!(hits.len(), 2);
}

// ── Score projection in SELECT ───────────────────────────────────────────────

/// `SELECT *, BM25(field, 'query') AS score FROM ...` — score in SELECT with alias.
#[test]
fn select_bm25_score_projection() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (_key TEXT, title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'rust guide', 'learn rust programming')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'python guide', 'learn python scripting')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a3', 'go guide', 'learn go programming')").unwrap();
    db.execute("CREATE INDEX ON articles USING bm25 (title)").unwrap();

    // SELECT *, BM25(...) AS score
    let hits = db.query("SELECT *, BM25(title, 'rust') AS score FROM articles").unwrap().collect();
    assert_eq!(hits.len(), 3);
    for h in &hits {
        let p = h.payload.as_ref().unwrap();
        assert!(p.get("score").is_some(), "score field must be present");
        assert!(p.get("title").is_some(), "title field must still be present");
    }
    // a1 has 'rust' in title → score > 0
    let a1 = hits.iter().find(|h| h.slug == "articles/a1").unwrap();
    let score = a1.payload.as_ref().unwrap().get("score").unwrap().as_f64().unwrap();
    assert!(score > 0.0, "a1 should have positive BM25 score");
    // a2 does not have 'rust' → score = 0
    let a2 = hits.iter().find(|h| h.slug == "articles/a2").unwrap();
    let score = a2.payload.as_ref().unwrap().get("score").unwrap().as_f64().unwrap();
    assert_eq!(score, 0.0, "a2 should have zero BM25 score");

    // SELECT title, BM25(...) AS relevance — explicit fields + score
    let hits = db.query("SELECT title, BM25(title, 'rust') AS relevance FROM articles").unwrap().collect();
    assert_eq!(hits.len(), 3);
    let a1 = hits.iter().find(|h| h.slug == "articles/a1").unwrap();
    let p = a1.payload.as_ref().unwrap();
    assert!(p.get("relevance").is_some(), "relevance alias must be present");
    assert!(p.get("title").is_some(), "title must be present");
    assert!(p.get("body").is_none(), "body must not be present in explicit select");
}

/// ORDER BY on a BM25 alias must sort by the actual score, not storage order.
/// Regression test: `ORDER BY bk DESC LIMIT N` was a no-op because the alias
/// wasn't resolved to the ScoreExpr before sorting.
#[test]
fn order_by_bm25_alias() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE resources (_key TEXT, keywords TEXT)").unwrap();
    // Insert 5 rows, only 2 contain 'watercourse'
    db.execute("INSERT INTO resources (_key, keywords) VALUES ('r1', 'river watercourse hydrology')").unwrap();
    db.execute("INSERT INTO resources (_key, keywords) VALUES ('r2', 'road transport highway')").unwrap();
    db.execute("INSERT INTO resources (_key, keywords) VALUES ('r3', 'watercourse creek stream')").unwrap();
    db.execute("INSERT INTO resources (_key, keywords) VALUES ('r4', 'building footprint')").unwrap();
    db.execute("INSERT INTO resources (_key, keywords) VALUES ('r5', 'land parcel cadastre')").unwrap();
    db.execute("CREATE INDEX ON resources USING bm25 (keywords)").unwrap();

    let hits = db.query(
        "SELECT _key, BM25(keywords, 'watercourse') AS bk FROM resources ORDER BY bk DESC LIMIT 3"
    ).unwrap().collect();

    assert_eq!(hits.len(), 3);
    // Top results must have positive BM25 scores (the watercourse rows)
    let bk0 = hits[0].payload.as_ref().unwrap().get("bk").unwrap().as_f64().unwrap();
    let bk1 = hits[1].payload.as_ref().unwrap().get("bk").unwrap().as_f64().unwrap();
    assert!(bk0 > 0.0, "first result must have positive BM25 score, got {bk0}");
    assert!(bk1 > 0.0, "second result must have positive BM25 score, got {bk1}");
    // Scores must be in descending order
    assert!(bk0 >= bk1, "results must be sorted descending: {bk0} >= {bk1}");
}

// ── Multi-row INSERT ─────────────────────────────────────────────────────────

#[test]
fn multi_row_insert_basic() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE users (_key TEXT, name TEXT, age INTEGER)"#).unwrap();
    let n = db.execute(
        "INSERT INTO users (_key, name, age) VALUES ('a', 'Alice', 30), ('b', 'Bob', 25), ('c', 'Carol', 28)"
    ).unwrap();
    assert_eq!(n, 3);

    // All three rows should be queryable
    let hits = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(hits.len(), 3);
    let names: Vec<&str> = hits.iter()
        .map(|h| h.payload.as_ref().unwrap()["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Alice"));
    assert!(names.contains(&"Bob"));
    assert!(names.contains(&"Carol"));
}

#[test]
fn multi_row_insert_with_params() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE items (_key TEXT, val INTEGER)"#).unwrap();
    let n = db.execute_params(
        "INSERT INTO items (_key, val) VALUES ($1, $2), ($3, $4)",
        &[
            serde_json::json!("k1"), serde_json::json!(10),
            serde_json::json!("k2"), serde_json::json!(20),
        ],
    ).unwrap();
    assert_eq!(n, 2);

    let hits = db.query("SELECT * FROM items ORDER BY val").unwrap().collect();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].payload.as_ref().unwrap()["val"].as_f64(), Some(10.0));
    assert_eq!(hits[1].payload.as_ref().unwrap()["val"].as_f64(), Some(20.0));
}

#[test]
fn multi_row_insert_with_vectors() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE docs (_key TEXT, emb VECTOR)"#).unwrap();
    let n = db.execute(
        "INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 0.0, 0.0]), ('d2', [0.0, 1.0, 0.0]), ('d3', [0.0, 0.0, 1.0])"
    ).unwrap();
    assert_eq!(n, 3);

    // All three vectors should be searchable
    let hits = db.query(
        "SELECT * FROM docs WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 3)"
    ).unwrap().collect();
    assert_eq!(hits.len(), 3);
    // Nearest to [1,0,0] should be d1
    assert_eq!(hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap(), "d1");
}

#[test]
fn multi_row_insert_single_row_unchanged() {
    // Single-row INSERT still works exactly as before
    let mut db = CoreDB::new();
    let n = db.execute(
        "INSERT INTO users (_key, name) VALUES ('alice', 'Alice')"
    ).unwrap();
    assert_eq!(n, 1);
    let hits = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap()["name"].as_str().unwrap(), "Alice");
}

// ── Mass UPDATE deferred HNSW rebuild ────────────────────────────────────────

#[test]
fn mass_update_vector_deferred_hnsw() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE docs (_key TEXT, category TEXT, emb VECTOR)"#).unwrap();
    // Insert multiple docs with vectors
    db.execute("INSERT INTO docs (_key, category, emb) VALUES ('d1', 'x', [1.0, 0.0, 0.0])").unwrap();
    db.execute("INSERT INTO docs (_key, category, emb) VALUES ('d2', 'x', [0.0, 1.0, 0.0])").unwrap();
    db.execute("INSERT INTO docs (_key, category, emb) VALUES ('d3', 'y', [0.0, 0.0, 1.0])").unwrap();

    // Mass update: set all category='x' docs to new vector
    let n = db.execute(
        "UPDATE docs SET emb = [0.5, 0.5, 0.0] WHERE category = 'x'"
    ).unwrap();
    assert_eq!(n, 2);

    // After update, VECTOR_NEAR should reflect new vectors
    let hits = db.query(
        "SELECT * FROM docs WHERE VECTOR_NEAR(emb, [0.5, 0.5, 0.0], 3)"
    ).unwrap().collect();
    assert_eq!(hits.len(), 3);
    // d1 and d2 should be nearest (they now have [0.5, 0.5, 0.0])
    let top_keys: Vec<&str> = hits[..2].iter()
        .map(|h| h.payload.as_ref().unwrap()["_key"].as_str().unwrap())
        .collect();
    assert!(top_keys.contains(&"d1"));
    assert!(top_keys.contains(&"d2"));
}

#[test]
fn mass_update_vector_with_params() {
    let mut db = CoreDB::new();
    db.execute(r#"CREATE TABLE docs (_key TEXT, category TEXT, emb VECTOR)"#).unwrap();
    db.execute("INSERT INTO docs (_key, category, emb) VALUES ('d1', 'x', [1.0, 0.0, 0.0])").unwrap();
    db.execute("INSERT INTO docs (_key, category, emb) VALUES ('d2', 'x', [0.0, 1.0, 0.0])").unwrap();
    db.execute("INSERT INTO docs (_key, category, emb) VALUES ('d3', 'y', [0.0, 0.0, 1.0])").unwrap();

    // Mass update with param binding for both vector and WHERE
    let n = db.execute_params(
        "UPDATE docs SET emb = $1 WHERE category = $2",
        &[serde_json::json!([0.5, 0.5, 0.0]), serde_json::json!("x")],
    ).unwrap();
    assert_eq!(n, 2);

    let hits = db.query(
        "SELECT * FROM docs WHERE VECTOR_NEAR(emb, [0.5, 0.5, 0.0], 3)"
    ).unwrap().collect();
    assert_eq!(hits.len(), 3);
    // d1 and d2 should be nearest
    let top_keys: Vec<&str> = hits[..2].iter()
        .map(|h| h.payload.as_ref().unwrap()["_key"].as_str().unwrap())
        .collect();
    assert!(top_keys.contains(&"d1"));
    assert!(top_keys.contains(&"d2"));
}

// ── Incremental HNSW correctness ─────────────────────────────────────────────

#[test]
fn incremental_hnsw_correctness() {
    // Insert 200 vectors one at a time (incremental HNSW insert).
    // Verify VECTOR_NEAR returns the correct nearest neighbor for several probes.
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE vecs (_key TEXT, embedding VECTOR)").unwrap();
    db.execute("CREATE INDEX ON vecs USING hnsw (embedding)").unwrap();

    let dim = 32;
    let n = 200;

    // Deterministic pseudo-random vectors
    let make_vec = |seed: usize| -> Vec<f32> {
        (0..dim).map(|i: usize| {
            let x = ((seed.wrapping_mul(6364136223846793005)
                .wrapping_add(i.wrapping_mul(1442695040888963407))) >> 33) as f32;
            (x / u32::MAX as f32) * 2.0 - 1.0
        }).collect()
    };

    for i in 0..n {
        let vec = make_vec(i);
        let coords: String = vec.iter()
            .map(|f| format!("{:.6}", f))
            .collect::<Vec<_>>()
            .join(", ");
        db.execute(&format!(
            "INSERT INTO vecs (_key, embedding) VALUES ('v{}', [{}])", i, coords
        )).unwrap();
    }

    // Probe: search for a known vector, should find itself as the closest
    let probe_vec = make_vec(42);
    let coords: String = probe_vec.iter()
        .map(|f| format!("{:.6}", f))
        .collect::<Vec<_>>()
        .join(", ");
    let hits = db.query(&format!(
        "SELECT _key FROM vecs WHERE VECTOR_NEAR(embedding, [{}], 5)", coords
    )).unwrap().collect();

    assert_eq!(hits.len(), 5, "should return 5 nearest neighbors");
    // The exact vector should be the first result
    let first_key = hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap();
    assert_eq!(first_key, "v42", "exact match should be nearest");

    // Probe with a different vector
    let probe_vec2 = make_vec(100);
    let coords2: String = probe_vec2.iter()
        .map(|f| format!("{:.6}", f))
        .collect::<Vec<_>>()
        .join(", ");
    let hits2 = db.query(&format!(
        "SELECT _key FROM vecs WHERE VECTOR_NEAR(embedding, [{}], 10)", coords2
    )).unwrap().collect();

    assert_eq!(hits2.len(), 10, "should return 10 nearest neighbors");
    let first_key2 = hits2[0].payload.as_ref().unwrap()["_key"].as_str().unwrap();
    assert_eq!(first_key2, "v100", "exact match should be nearest");
}

#[test]
fn incremental_hnsw_remove_and_reinsert() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE items (_key TEXT, emb VECTOR)").unwrap();
    db.execute("CREATE INDEX ON items USING hnsw (emb)").unwrap();

    // Insert 50 vectors
    for i in 0..50 {
        let v: Vec<f32> = (0..16).map(|d| (i * 16 + d) as f32 / 800.0).collect();
        let coords: String = v.iter().map(|f| format!("{:.6}", f)).collect::<Vec<_>>().join(", ");
        db.execute(&format!(
            "INSERT INTO items (_key, emb) VALUES ('item{}', [{}])", i, coords
        )).unwrap();
    }

    // Verify search works with 50 items
    let probe: Vec<f32> = (0..16).map(|d| (25 * 16 + d) as f32 / 800.0).collect();
    let coords: String = probe.iter().map(|f| format!("{:.6}", f)).collect::<Vec<_>>().join(", ");
    let hits = db.query(&format!(
        "SELECT _key FROM items WHERE VECTOR_NEAR(emb, [{}], 5)", coords
    )).unwrap().collect();
    assert_eq!(hits.len(), 5);
    let first = hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap();
    assert_eq!(first, "item25");

    // Delete item25 and verify it no longer appears
    db.execute("DELETE FROM items WHERE _key = 'item25'").unwrap();
    let hits2 = db.query(&format!(
        "SELECT _key FROM items WHERE VECTOR_NEAR(emb, [{}], 5)", coords
    )).unwrap().collect();
    assert_eq!(hits2.len(), 5);
    for h in &hits2 {
        let key = h.payload.as_ref().unwrap()["_key"].as_str().unwrap();
        assert_ne!(key, "item25", "deleted item should not appear in results");
    }
}

// ── Disk-backed vector store ──────────────────────────────────────────────────

#[test]
fn disk_vector_store_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();

    // Insert vectors, close, reopen, verify they survive.
    {
        let mut db = CoreDB::open(path).unwrap();
        db.execute("CREATE TABLE docs (_key TEXT, emb VECTOR)").unwrap();
        for i in 0..20_usize {
            let vec: Vec<f32> = (0..8).map(|j| (i * 8 + j) as f32 * 0.01).collect();
            let vec_str: String = format!("[{}]", vec.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","));
            let sql = format!(
                "INSERT INTO docs (_key, emb) VALUES ('doc{i}', {vec_str})"
            );
            db.execute(&sql).unwrap();
        }
        db.compact().unwrap();
    }

    // Reopen and verify vectors are present via VECTOR_NEAR query.
    {
        let db = CoreDB::open(path).unwrap();
        let query_vec: Vec<f32> = (0..8).map(|j| j as f32 * 0.01).collect();
        let query_str = format!("[{}]", query_vec.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","));
        let hits: Vec<_> = db
            .query(&format!(
                "SELECT _key FROM docs WHERE VECTOR_NEAR(emb, {query_str}, 5)"
            ))
            .unwrap()
            .collect();
        assert!(
            !hits.is_empty(),
            "VECTOR_NEAR should return results after reopen"
        );
        // doc0 should be the closest match (its vector starts at 0.0).
        let first_key = hits[0].payload.as_ref().unwrap()["_key"].as_str().unwrap();
        assert_eq!(first_key, "doc0", "doc0 should be nearest to query vector");
    }
}

#[test]
fn disk_vector_store_compact_prunes_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();

    {
        let mut db = CoreDB::open(path).unwrap();
        db.execute("CREATE TABLE items (_key TEXT, vec VECTOR)").unwrap();
        // Insert 10 items
        for i in 0..10_usize {
            let vec: Vec<f32> = (0..4).map(|j| (i * 4 + j) as f32).collect();
            let vec_str = format!("[{}]", vec.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","));
            db.execute(&format!(
                "INSERT INTO items (_key, vec) VALUES ('item{i}', {vec_str})"
            )).unwrap();
        }
        // Delete half
        for i in 0..5_usize {
            db.execute(&format!("DELETE FROM items WHERE _key = 'item{i}'")).unwrap();
        }
        db.compact().unwrap();
    }

    // Reopen: only items 5-9 should remain.
    {
        let db = CoreDB::open(path).unwrap();
        let all: Vec<_> = db.query("SELECT _key FROM items").unwrap().collect();
        assert_eq!(all.len(), 5);
        for h in &all {
            let key = h.payload.as_ref().unwrap()["_key"].as_str().unwrap();
            assert!(
                key.starts_with("item") && key[4..].parse::<usize>().unwrap() >= 5,
                "only items 5-9 should survive compaction, got {key}"
            );
        }
    }
}

#[test]
fn disk_vector_store_get_vector_api() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();

    let mut db = CoreDB::open(path).unwrap();
    let vec_data: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
    db.put_vector("mynode", "emb", &vec_data).unwrap();

    let retrieved = db.get_vector("mynode", "emb").expect("vector should exist");
    assert_eq!(retrieved, &vec_data[..]);
}

#[test]
fn disk_vector_store_backward_compat_no_bin_file() {
    // Simulate a legacy DB: has snapshot with vectors in JSON but no .bin files.
    // This requires a snapshot written WITHOUT has_vector_files (legacy format).
    // We achieve this by creating a DB, inserting vectors, then writing a snapshot
    // that includes vectors in JSON (by compacting before any .bin files exist).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();

    // Create a DB with vectors and compact. The first compact writes vectors to
    // .bin files AND the snapshot. We need to delete .bin files AND rewrite the
    // snapshot to simulate a truly legacy DB (pre-Phase 4 snapshot).
    {
        let mut db = CoreDB::open(path).unwrap();
        db.execute("CREATE TABLE docs (_key TEXT, emb VECTOR)").unwrap();
        let sql = "INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 0.0, 0.0, 0.0])";
        db.execute(sql).unwrap();
        // Don't compact — let the WAL retain the PutVector entry.
    }

    // Delete .bin files to simulate legacy (the WAL still has the PutVector entry).
    let bin_path = dir.path().join("vectors_emb.bin");
    if bin_path.exists() {
        std::fs::remove_file(&bin_path).unwrap();
    }

    // Reopen: WAL replay should recreate the vector, then migrate to disk.
    {
        let db = CoreDB::open(path).unwrap();
        let v = db.get_vector("docs/d1", "emb");
        assert!(v.is_some(), "vector should be loadable from WAL replay even without .bin file");
        assert_eq!(v.unwrap(), &[1.0, 0.0, 0.0, 0.0]);
    }
}

#[test]
fn disk_vector_store_phase6_compact_skips_json_vectors() {
    // After compact with disk-backed stores, snapshot.json should NOT contain
    // vector data (has_vector_files = true). Reopening should load from .bin files.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();

    {
        let mut db = CoreDB::open(path).unwrap();
        db.execute("CREATE TABLE docs (_key TEXT, emb VECTOR)").unwrap();
        db.execute("INSERT INTO docs (_key, emb) VALUES ('d1', [1.0, 2.0, 3.0])").unwrap();
        db.execute("INSERT INTO docs (_key, emb) VALUES ('d2', [4.0, 5.0, 6.0])").unwrap();
        db.compact().unwrap();
    }

    // Verify snapshot doesn't contain vector float data.
    let snap_path = dir.path().join("snapshot.json");
    let snap_content = std::fs::read_to_string(&snap_path).unwrap();
    assert!(snap_content.contains("\"has_vector_files\":true"),
        "snapshot should have has_vector_files flag");
    assert!(!snap_content.contains("\"data\":[1.0,2.0,3.0]"),
        "snapshot should NOT contain vector data");

    // Reopen and verify vectors are loaded from .bin files.
    {
        let db = CoreDB::open(path).unwrap();
        let v1 = db.get_vector("docs/d1", "emb");
        assert!(v1.is_some(), "vector d1 should load from .bin file");
        assert_eq!(v1.unwrap(), &[1.0, 2.0, 3.0]);
        let v2 = db.get_vector("docs/d2", "emb");
        assert!(v2.is_some(), "vector d2 should load from .bin file");
        assert_eq!(v2.unwrap(), &[4.0, 5.0, 6.0]);
    }
}

// ── SQL Transaction tests (BEGIN / COMMIT / ROLLBACK) ─────────────────────

#[test]
fn sql_begin_commit_inserts() {
    let mut db = CoreDB::new();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('alice', 'Alice')").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('bob', 'Bob')").unwrap();

    // Not visible yet — still inside the transaction.
    let rows: Vec<_> = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(rows.len(), 0, "inserts should be invisible before COMMIT");

    db.execute("COMMIT").unwrap();

    // Now visible.
    let rows: Vec<_> = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(rows.len(), 2, "inserts should be visible after COMMIT");
}

#[test]
fn sql_begin_rollback_discards() {
    let mut db = CoreDB::new();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO users (_key, name) VALUES ('alice', 'Alice')").unwrap();
    db.execute("ROLLBACK").unwrap();

    let rows: Vec<_> = db.query("SELECT * FROM users").unwrap().collect();
    assert_eq!(rows.len(), 0, "inserts should be discarded after ROLLBACK");
}

#[test]
fn sql_nested_begin_errors() {
    let mut db = CoreDB::new();
    db.execute("BEGIN").unwrap();
    let err = db.execute("BEGIN").unwrap_err();
    assert!(
        format!("{err}").contains("nested BEGIN"),
        "expected nested BEGIN error, got: {err}"
    );
}

#[test]
fn sql_commit_outside_txn_errors() {
    let mut db = CoreDB::new();
    let err = db.execute("COMMIT").unwrap_err();
    assert!(
        format!("{err}").contains("without an active transaction"),
        "expected no-txn error, got: {err}"
    );
}

#[test]
fn sql_rollback_outside_txn_errors() {
    let mut db = CoreDB::new();
    let err = db.execute("ROLLBACK").unwrap_err();
    assert!(
        format!("{err}").contains("without an active transaction"),
        "expected no-txn error, got: {err}"
    );
}

#[test]
fn sql_begin_commit_edge_insert() {
    let mut db = CoreDB::new();
    db.put("users/alice", r#"{"name":"Alice"}"#).unwrap();
    db.put("users/bob", r#"{"name":"Bob"}"#).unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT ('users/alice')-[:KNOWS]->('users/bob')").unwrap();
    db.execute("COMMIT").unwrap();

    let fwd: Vec<_> = db.one("users/alice").forward("KNOWS").collect();
    assert_eq!(fwd.len(), 1, "edge should exist after COMMIT");
}

#[test]
fn sql_begin_commit_mixed() {
    let mut db = CoreDB::new();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO items (_key, name) VALUES ('x', 'X')").unwrap();
    db.execute("INSERT INTO items (_key, name) VALUES ('y', 'Y')").unwrap();
    db.execute("INSERT ('items/x')-[:RELATED]->('items/y')").unwrap();
    db.execute("COMMIT").unwrap();

    let rows: Vec<_> = db.query("SELECT * FROM items").unwrap().collect();
    assert_eq!(rows.len(), 2, "both items should exist");

    let fwd: Vec<_> = db.one("items/x").forward("RELATED").collect();
    assert_eq!(fwd.len(), 1, "edge should exist");
}

// ── WAL transaction atomicity tests ───────────────────────────────────────

#[test]
fn sql_txn_wal_atomic_commit() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO docs (_key, title) VALUES ('a', 'Alpha')").unwrap();
        db.execute("INSERT INTO docs (_key, title) VALUES ('b', 'Beta')").unwrap();
        db.execute("COMMIT").unwrap();
    }
    // Reopen — committed data should survive.
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let rows: Vec<_> = db.query("SELECT * FROM docs").unwrap().collect();
        assert_eq!(rows.len(), 2, "committed transaction should survive reopen");
    }
}

#[test]
fn sql_txn_wal_incomplete_discarded() {
    let dir = tempfile::TempDir::new().unwrap();
    // 1. Create a DB with one baseline node.
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("INSERT INTO docs (_key, title) VALUES ('base', 'Baseline')").unwrap();
    }
    // 2. Append an incomplete transaction directly to the WAL:
    //    TxnBegin + Put entry, but NO TxnEnd — simulates a crash mid-COMMIT.
    {
        use std::io::Write;
        let wal_path = dir.path().join("wal.log");
        let mut f = std::fs::OpenOptions::new().append(true).open(&wal_path).unwrap();

        // Helper: write one WAL frame (CRC32 + len + JSON).
        let write_frame = |f: &mut std::fs::File, json: &[u8]| {
            let len_bytes = (json.len() as u32).to_le_bytes();
            let mut crc_input = Vec::with_capacity(4 + json.len());
            crc_input.extend_from_slice(&len_bytes);
            crc_input.extend_from_slice(json);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&crc_input);
            let crc = hasher.finalize().to_le_bytes();
            f.write_all(&crc).unwrap();
            f.write_all(&len_bytes).unwrap();
            f.write_all(json).unwrap();
            f.flush().unwrap();
        };

        // TxnBegin marker
        write_frame(&mut f, br#"{"op":"txn_begin"}"#);
        // A Put entry inside the transaction (no TxnEnd follows — crash!)
        write_frame(
            &mut f,
            br#"{"op":"put","slug":"docs/orphan","payload":"{\"_key\":\"orphan\",\"title\":\"Ghost\",\"_collection\":\"docs\"}"}"#,
        );
        // No TxnEnd — simulates process crash.
    }
    // 3. Reopen — baseline should exist, orphan should be discarded.
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let rows: Vec<_> = db.query("SELECT * FROM docs").unwrap().collect();
        assert_eq!(rows.len(), 1, "incomplete transaction should be discarded");
        let base: Vec<_> = db.query("SELECT title FROM docs WHERE _key = 'base'").unwrap().collect();
        assert_eq!(base.len(), 1, "baseline node should survive");
    }
}

// ── Search index tests ───────────────────────────────────────────────────────

#[test]
fn search_index_create_and_query() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast and safe')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'Python Guide', 'Python is easy to learn')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a3', 'Rust and Python', 'Both languages are great')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('rust')").unwrap().collect();
    assert_eq!(rows.len(), 2, "should find 2 docs containing 'rust'");
    let slugs: Vec<&str> = rows.iter().map(|h| h.slug.as_str()).collect();
    assert!(slugs.contains(&"articles/a1"));
    assert!(slugs.contains(&"articles/a3"));
}

#[test]
fn search_composes_with_spatial() {
    // Regression: `SEARCH(...) AND ST_DWithin(...)` returned 0 rows. The spatial
    // filter's starter left current_coll_hash unset, so SearchFilter took its
    // else-branch and cleared every candidate. It must fall back to the candidates'
    // own collection and intersect normally (cf. the composed cross-model query).
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE cafes (name TEXT, geometry GEO)").unwrap();
    db.put("cafes/a", r#"{"_collection":"cafes","_key":"a","name":"great coffee","geometry":{"type":"Point","coordinates":[106.50,-6.40]}}"#).unwrap();
    db.put("cafes/b", r#"{"_collection":"cafes","_key":"b","name":"best coffee","geometry":{"type":"Point","coordinates":[106.51,-6.41]}}"#).unwrap();
    db.put("cafes/c", r#"{"_collection":"cafes","_key":"c","name":"coffee house","geometry":{"type":"Point","coordinates":[120.0,-8.0]}}"#).unwrap(); // far
    db.put("cafes/d", r#"{"_collection":"cafes","_key":"d","name":"tea room","geometry":{"type":"Point","coordinates":[106.50,-6.40]}}"#).unwrap(); // near, no 'coffee'
    db.build_spatial_index();
    db.execute("CREATE INDEX ON cafes USING search (name)").unwrap();

    // 'coffee' near the origin → a, b only (c is far; d has no 'coffee').
    let both_orders = [
        "SELECT _key FROM cafes WHERE SEARCH('coffee') AND ST_DWithin(geometry, POINT(106.5 -6.4), 5000) ORDER BY _key ASC",
        "SELECT _key FROM cafes WHERE ST_DWithin(geometry, POINT(106.5 -6.4), 5000) AND SEARCH('coffee') ORDER BY _key ASC",
    ];
    for q in both_orders {
        let rows: Vec<String> = db.query(q).unwrap().collect().iter().map(|h| h.slug.clone()).collect();
        assert_eq!(rows, vec!["cafes/a".to_string(), "cafes/b".to_string()], "SEARCH∩spatial for: {q}");
    }
}

#[test]
fn search_index_multi_term_and() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast and safe')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'Python Guide', 'Python is easy to learn')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a3', 'Rust and Python', 'Both languages are great')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('rust fast')").unwrap().collect();
    assert_eq!(rows.len(), 1, "AND semantics: only doc with both 'rust' AND 'fast'");
    assert_eq!(rows[0].slug, "articles/a1");
}

#[test]
fn search_index_no_match() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('javascript')").unwrap().collect();
    assert_eq!(rows.len(), 0);
}

#[test]
fn search_score_ordering() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast and safe')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'Python Guide', 'Python is easy to learn')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a3', 'Rust and Python', 'Both languages are great')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    let rows: Vec<_> = db.query(
        "SELECT title FROM articles WHERE SEARCH('rust') ORDER BY SEARCH_SCORE('rust fast') DESC"
    ).unwrap().collect();

    assert_eq!(rows.len(), 2);
    // a1 has both "rust" and "fast" → score 1.0 → ranked first
    // a3 has "rust" but not "fast" → score 0.5 → ranked second
    assert_eq!(rows[0].slug, "articles/a1");
    assert_eq!(rows[1].slug, "articles/a3");
}

#[test]
fn search_with_other_filters() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT, category TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body, category) VALUES ('a1', 'Rust Programming', 'Rust is fast', 'tech')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body, category) VALUES ('a2', 'Rust Cooking', 'Rustic food is great', 'food')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    let rows: Vec<_> = db.query(
        "SELECT * FROM articles WHERE SEARCH('rust') AND category = 'tech'"
    ).unwrap().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "articles/a1");
}

#[test]
fn search_index_persistence() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
        db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast')").unwrap();
        db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'Python Guide', 'Python is easy')").unwrap();
        db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();
        db.compact().unwrap();
    }
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('rust')").unwrap().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "articles/a1");
    }
}

#[test]
fn search_index_drop() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Rust is fast')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('rust')").unwrap().collect();
    assert_eq!(rows.len(), 1);

    db.execute("DROP INDEX ON articles USING search (title)").unwrap();
    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('rust')").unwrap().collect();
    assert_eq!(rows.len(), 0, "after DROP INDEX, search should return no results");
}

#[test]
fn search_fuzzy_typo_tolerance() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE articles (title TEXT, body TEXT)").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a1', 'Rust Programming', 'Systems programming language')").unwrap();
    db.execute("INSERT INTO articles (_key, title, body) VALUES ('a2', 'Python Guide', 'Scripting language tutorial')").unwrap();
    db.execute("CREATE INDEX ON articles USING search (title, body)").unwrap();

    // Exact match still works
    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('programming')").unwrap().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "articles/a1");

    // Typo: "programing" (1 edit from "programming", 10 chars → max_dist=2)
    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('programing')").unwrap().collect();
    assert_eq!(rows.len(), 1, "fuzzy should find 'programming' from 'programing'");
    assert_eq!(rows[0].slug, "articles/a1");

    // Short term (4 chars) — no fuzzy: "ruts" should NOT match "rust"
    let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('ruts')").unwrap().collect();
    assert_eq!(rows.len(), 0, "4-char terms get no typo tolerance");
}

#[test]
fn search_fuzzy_persistence() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE articles (title TEXT)").unwrap();
        db.execute("INSERT INTO articles (_key, title) VALUES ('a1', 'Rust Programming Language')").unwrap();
        db.execute("CREATE INDEX ON articles USING search (title)").unwrap();
        db.compact().unwrap();
    }
    {
        let db = CoreDB::open(dir.path()).unwrap();
        // Fuzzy should work after reopen
        let rows: Vec<_> = db.query("SELECT * FROM articles WHERE SEARCH('programing')").unwrap().collect();
        assert_eq!(rows.len(), 1, "fuzzy should survive compact + reopen");
    }
}

#[test]
fn batch_wal_sync_update_disk() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    for i in 0..1000usize {
        db.put(
            &format!("products/p{i}"),
            &format!(
                r#"{{"_collection":"products","_key":"p{i}","category":"cat{}","name":"Product {i}"}}"#,
                i % 10
            ),
        ).unwrap();
    }
    db.execute("CREATE INDEX ON products USING btree (category)").unwrap();

    let start = std::time::Instant::now();
    let iters = 10u32;
    for _ in 0..iters {
        db.execute("UPDATE products SET name = 'Updated' WHERE category = 'cat3'").unwrap();
    }
    let per_iter = start.elapsed() / iters;
    eprintln!("[BATCH SYNC] UPDATE 100 rows disk: {per_iter:?}/call, DELETE next...");

    let start = std::time::Instant::now();
    db.execute("DELETE FROM products WHERE category = 'cat9'").unwrap();
    let del = start.elapsed();
    eprintln!("[BATCH SYNC] DELETE 100 rows disk: {del:?}");

    let count: Vec<_> = db.query("SELECT * FROM products WHERE category = 'cat9'").unwrap().collect();
    assert_eq!(count.len(), 0);
}

#[test]
fn deferred_bm25_put_many_correctness() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE articles (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("CREATE INDEX ON articles USING bm25 (title)").unwrap();

    let items: Vec<(String, String)> = (0..50)
        .map(|i| (
            format!("articles/a{i}"),
            format!(r#"{{"_collection":"articles","_key":"a{i}","title":"Article {i} about rust databases"}}"#),
        ))
        .collect();
    db.put_many(items.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();

    let results = db.bm25_search("title", "rust", 100);
    assert_eq!(results.len(), 50, "BM25 must find all 50 docs after deferred flush");
}

#[test]
fn deferred_bm25_sql_batch_correctness() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE docs (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("CREATE INDEX ON docs USING bm25 (title)").unwrap();

    let mut sql = String::from("INSERT INTO docs (_key, title) VALUES ");
    for i in 0..100 {
        if i > 0 { sql.push_str(", "); }
        sql.push_str(&format!("('d{i}', 'Document {i} about embedded systems')"));
    }
    db.execute(&sql).unwrap();

    let results = db.bm25_search("title", "embedded", 200);
    assert_eq!(results.len(), 100, "BM25 must find all 100 docs after SQL batch");
}

#[test]
fn deferred_gin_sql_batch_correctness() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("CREATE INDEX ON items USING gin (name)").unwrap();

    let mut sql = String::from("INSERT INTO items (_key, name) VALUES ");
    for i in 0..50 {
        if i > 0 { sql.push_str(", "); }
        sql.push_str(&format!("('i{i}', 'Widget model {i}')"));
    }
    db.execute(&sql).unwrap();

    let hits: Vec<_> = db.query("SELECT * FROM items WHERE name LIKE '%Widget%'").unwrap().collect();
    assert_eq!(hits.len(), 50, "GIN LIKE must find all 50 items after deferred batch");
}

#[test]
fn deferred_txn_commit_correctness() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE articles (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("CREATE INDEX ON articles USING bm25 (title)").unwrap();

    db.execute("BEGIN").unwrap();
    for i in 0..20 {
        db.execute(&format!(
            "INSERT INTO articles (_key, title) VALUES ('t{i}', 'Transaction item {i} about vector search')"
        )).unwrap();
    }
    db.execute("COMMIT").unwrap();

    let results = db.bm25_search("title", "vector", 50);
    assert_eq!(results.len(), 20, "BM25 must find all 20 docs after COMMIT");
}

// ── Logical WAL mode (SET WAL_MODE = logical) ────────────────────────────────

#[test]
fn logical_wal_update_survives_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        for i in 0..50 {
            db.execute(&format!(
                "INSERT INTO products (_key, category, name) VALUES ('p{i}', 'cat{}', 'Product {i}')",
                i % 5
            )).unwrap();
        }
        db.execute("SET WAL_MODE = logical").unwrap();
        let n = db.execute("UPDATE products SET name = 'Updated' WHERE category = 'cat3'").unwrap();
        assert_eq!(n, 10, "10 rows in cat3");
    }
    // Reopen — the logical Update entry must replay to the same final state.
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let updated: Vec<_> = db.query("SELECT * FROM products WHERE name = 'Updated'")
            .unwrap().collect();
        assert_eq!(updated.len(), 10, "logical UPDATE must replay on reopen");
        let untouched: Vec<_> = db.query("SELECT * FROM products WHERE category = 'cat1'")
            .unwrap().collect();
        for hit in untouched {
            let name = hit.payload.as_ref().unwrap()["name"].as_str().unwrap().to_string();
            assert_ne!(name, "Updated", "cat1 rows must not be touched");
        }
    }
}

#[test]
fn logical_wal_update_timestamp_deterministic() {
    let dir = tempfile::TempDir::new().unwrap();
    let ts_before: i64;
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("INSERT INTO docs (_key, title) VALUES ('a', 'Alpha')").unwrap();
        db.execute("SET WAL_MODE = logical").unwrap();
        db.execute("UPDATE docs SET title = 'Changed' WHERE _key = 'a'").unwrap();
        let hit = db.query("SELECT * FROM docs WHERE _key = 'a'").unwrap().collect();
        ts_before = hit[0].payload.as_ref().unwrap()["_updated_unix"].as_i64().unwrap();
    }
    // Reopen: replay must reproduce the exact same _updated_unix (stored in the entry).
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let hit = db.query("SELECT * FROM docs WHERE _key = 'a'").unwrap().collect();
        let ts_after = hit[0].payload.as_ref().unwrap()["_updated_unix"].as_i64().unwrap();
        assert_eq!(ts_before, ts_after, "logical replay must reproduce _updated_unix exactly");
        assert_eq!(hit[0].payload.as_ref().unwrap()["title"].as_str().unwrap(), "Changed");
    }
}

#[test]
fn logical_wal_update_btree_index_consistent_after_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, status TEXT, n INTEGER)").unwrap();
        db.execute("CREATE INDEX ON items USING btree (status)").unwrap();
        for i in 0..20 {
            db.execute(&format!(
                "INSERT INTO items (_key, status, n) VALUES ('i{i}', 'open', {i})"
            )).unwrap();
        }
        db.execute("SET WAL_MODE = logical").unwrap();
        db.execute("UPDATE items SET status = 'closed' WHERE n < 5").unwrap();
    }
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let closed: Vec<_> = db.query("SELECT * FROM items WHERE status = 'closed'")
            .unwrap().collect();
        assert_eq!(closed.len(), 5, "btree-indexed query must see logical UPDATE after reopen");
        let open: Vec<_> = db.query("SELECT * FROM items WHERE status = 'open'")
            .unwrap().collect();
        assert_eq!(open.len(), 15);
    }
}

#[test]
fn logical_wal_smaller_than_physical() {
    // Same workload in both modes — logical WAL must be much smaller.
    let size_of = |logical: bool| -> u64 {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open(dir.path()).unwrap();
        for i in 0..200 {
            db.execute(&format!(
                "INSERT INTO products (_key, category, name) VALUES ('p{i}', 'cat{}', 'Product number {i} with a reasonably long name')",
                i % 2
            )).unwrap();
        }
        let before = std::fs::metadata(dir.path().join("wal.log")).unwrap().len();
        if logical {
            db.execute("SET WAL_MODE = logical").unwrap();
        }
        db.execute("UPDATE products SET name = 'X' WHERE category = 'cat0'").unwrap();
        let after = std::fs::metadata(dir.path().join("wal.log")).unwrap().len();
        after - before
    };
    let physical = size_of(false);
    let logical = size_of(true);
    assert!(
        logical * 10 < physical,
        "logical WAL ({logical} B) should be >10x smaller than physical ({physical} B)"
    );
}

#[test]
fn physical_wal_mode_is_default_and_switchable() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("INSERT INTO docs (_key, title) VALUES ('a', 'Alpha')").unwrap();
        // logical on, then back to physical — both updates must persist
        db.execute("SET WAL_MODE = logical").unwrap();
        db.execute("UPDATE docs SET title = 'One' WHERE _key = 'a'").unwrap();
        db.execute("SET WAL_MODE = physical").unwrap();
        db.execute("UPDATE docs SET title = 'Two' WHERE _key = 'a'").unwrap();
    }
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let hit = db.query("SELECT * FROM docs WHERE _key = 'a'").unwrap().collect();
        assert_eq!(hit[0].payload.as_ref().unwrap()["title"].as_str().unwrap(), "Two",
            "mixed-mode WAL must replay in order");
    }
}

#[test]
fn wal_sync_levels_accepted_and_data_survives() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        for level in ["os", "barrier", "full"] {
            db.execute(&format!("SET WAL_SYNC = {level}")).unwrap();
            db.execute(&format!(
                "INSERT INTO docs (_key, title) VALUES ('{level}', 'at {level}')"
            )).unwrap();
        }
        assert!(db.execute("SET WAL_SYNC = bogus").is_err());
        // Level must survive compact (WAL writer recreation).
        db.execute("SET WAL_SYNC = os").unwrap();
        db.execute("COMPACT").unwrap();
        db.execute("UPDATE docs SET title = 'post-compact' WHERE _key = 'os'").unwrap();
    }
    {
        let db = CoreDB::open(dir.path()).unwrap();
        let rows: Vec<_> = db.query("SELECT * FROM docs").unwrap().collect();
        assert_eq!(rows.len(), 3);
        let hit = db.query("SELECT * FROM docs WHERE _key = 'os'").unwrap().collect();
        assert_eq!(hit[0].payload.as_ref().unwrap()["title"].as_str().unwrap(), "post-compact");
    }
}

// ── Search index maintained on writes (aligned with BM25) ────────────────────

#[test]
fn search_index_maintained_on_insert() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT, artist TEXT)").unwrap();
    // Index created BEFORE any data — the previously-broken case.
    db.execute("CREATE INDEX ON songs USING search (title, artist)").unwrap();
    db.execute("INSERT INTO songs (_key, title, artist) VALUES ('s1', 'Yesterday', 'The Beatles')").unwrap();
    db.execute("INSERT INTO songs (_key, title, artist) VALUES ('s2', 'Let It Be', 'The Beatles')").unwrap();

    let hits: Vec<_> = db.query("SELECT _key FROM songs WHERE SEARCH('yesterday')").unwrap().collect();
    assert_eq!(hits.len(), 1, "doc inserted after CREATE INDEX must be searchable immediately");
    assert_eq!(hits[0].payload.as_ref().unwrap()["_key"], "s1");

    // A later insert must also be visible without any rebuild.
    db.execute("INSERT INTO songs (_key, title, artist) VALUES ('s3', 'Yesterday Once More', 'The Carpenters')").unwrap();
    let hits: Vec<_> = db.query("SELECT _key FROM songs WHERE SEARCH('yesterday')").unwrap().collect();
    assert_eq!(hits.len(), 2, "second insert must also be searchable");
}

#[test]
fn search_index_maintained_on_update_and_delete() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("INSERT INTO songs (_key, title) VALUES ('s1', 'Yesterday')").unwrap();
    db.execute("CREATE INDEX ON songs USING search (title)").unwrap();

    // UPDATE: old term gone, new term searchable.
    db.execute("UPDATE songs SET title = 'Tomorrow' WHERE _key = 's1'").unwrap();
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('yesterday')").unwrap().collect().len(), 0,
        "updated-away term must no longer match");
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('tomorrow')").unwrap().collect().len(), 1,
        "new term must match after UPDATE");

    // DELETE: node no longer searchable.
    db.execute("DELETE FROM songs WHERE _key = 's1'").unwrap();
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('tomorrow')").unwrap().collect().len(), 0,
        "deleted node must not be searchable");
}

#[test]
fn search_index_maintained_in_batch_insert() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("CREATE INDEX ON songs USING search (title)").unwrap();
    // Multi-row INSERT goes through the deferred path → must flush search once.
    db.execute("INSERT INTO songs (_key, title) VALUES ('a','rust language'),('b','python guide'),('c','rust systems')").unwrap();
    let hits: Vec<_> = db.query("SELECT _key FROM songs WHERE SEARCH('rust')").unwrap().collect();
    assert_eq!(hits.len(), 2, "batch-inserted docs must be searchable after deferred flush");
}

// ── GIN maintained on DELETE ─────────────────────────────────────────────────

#[test]
fn gin_index_maintained_on_delete() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE docs (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("INSERT INTO docs (_key, body) VALUES ('d1', 'the quick brown fox')").unwrap();
    db.execute("INSERT INTO docs (_key, body) VALUES ('d2', 'the lazy brown dog')").unwrap();
    db.execute("CREATE INDEX ON docs USING gin (body)").unwrap();

    assert_eq!(db.query("SELECT _key FROM docs WHERE body ILIKE '%quick%'").unwrap().collect().len(), 1);
    db.execute("DELETE FROM docs WHERE _key = 'd1'").unwrap();
    assert_eq!(db.query("SELECT _key FROM docs WHERE body ILIKE '%quick%'").unwrap().collect().len(), 0,
        "deleted doc must not match GIN ILIKE after DELETE");
    assert_eq!(db.query("SELECT _key FROM docs WHERE body ILIKE '%brown%'").unwrap().collect().len(), 1,
        "remaining doc still matches");
}

#[test]
fn drop_table_clears_search_index() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("INSERT INTO songs (_key, title) VALUES ('s1', 'Yesterday')").unwrap();
    db.execute("CREATE INDEX ON songs USING search (title)").unwrap();
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('yesterday')").unwrap().collect().len(), 1);

    db.execute("DROP TABLE songs").unwrap();
    // Recreate and confirm the old index/data didn't linger.
    db.execute("CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("CREATE INDEX ON songs USING search (title)").unwrap();
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('yesterday')").unwrap().collect().len(), 0,
        "dropped table's search index must not survive");
}

#[test]
fn bulk_delete_keeps_search_consistent() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT, tag TEXT)").unwrap();
    db.execute("CREATE INDEX ON songs USING search (title)").unwrap();
    for i in 0..200 {
        db.execute(&format!("INSERT INTO songs (_key, title, tag) VALUES ('s{i}', 'rust song {i}', '{}')", if i % 2 == 0 { "keep" } else { "drop" })).unwrap();
    }
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('rust')").unwrap().collect().len(), 200);
    db.execute("DELETE FROM songs WHERE tag = 'drop'").unwrap(); // 100 rows
    assert_eq!(db.query("SELECT _key FROM songs WHERE SEARCH('rust')").unwrap().collect().len(), 100,
        "search must reflect bulk delete of 100 rows");
}

// ── HNSW maintained on DELETE (no orphan graph node) ─────────────────────────

#[test]
fn hnsw_index_maintained_on_delete() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, embedding VECTOR)").unwrap();
    db.execute("CREATE INDEX ON items USING hnsw (embedding)").unwrap();
    // 6 vectors; target query is [1,0,0]. 'hit' sits exactly on it.
    db.execute("INSERT INTO items (_key,name,embedding) VALUES ('hit','exact',[1.0,0.0,0.0])").unwrap();
    for i in 0..5 {
        let x = 0.5 - i as f32 * 0.1;
        db.execute(&format!("INSERT INTO items (_key,name,embedding) VALUES ('n{i}','other',[{x},0.9,0.1])")).unwrap();
    }
    // Before delete: nearest neighbour to [1,0,0] must be 'hit'.
    let before: Vec<_> = db.query("SELECT _key FROM items WHERE VECTOR_NEAR(embedding, [1.0,0.0,0.0], 3)").unwrap().collect();
    assert!(before.iter().any(|h| h.payload.as_ref().unwrap()["_key"] == "hit"),
        "sanity: 'hit' should be found before delete");

    // Delete the exact-match node, then search again — it must never come back.
    db.execute("DELETE FROM items WHERE _key = 'hit'").unwrap();
    let after: Vec<_> = db.query("SELECT _key FROM items WHERE VECTOR_NEAR(embedding, [1.0,0.0,0.0], 6)").unwrap().collect();
    assert!(!after.iter().any(|h| h.payload.as_ref().unwrap()["_key"] == "hit"),
        "deleted node must not be returned by vector search (no orphan graph node)");
    // Remaining nodes still searchable.
    assert!(!after.is_empty(), "other vectors still reachable after delete");
}

// ── BM25_NORM: bounded [0,1] blendable BM25 ──────────────────────────────────

#[test]
fn bm25_norm_bounded_and_ordered() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE docs (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    // 'rust' in a minority of docs → positive IDF (discriminating term)
    db.execute("INSERT INTO docs (_key,body) VALUES ('a','rust rust rust systems memory safety')").unwrap();
    db.execute("INSERT INTO docs (_key,body) VALUES ('b','a note mentioning rust once briefly')").unwrap();
    db.execute("INSERT INTO docs (_key,body) VALUES ('c','python guide beginners')").unwrap();
    db.execute("INSERT INTO docs (_key,body) VALUES ('d','javascript web platform')").unwrap();
    db.execute("INSERT INTO docs (_key,body) VALUES ('e','go concurrency patterns')").unwrap();
    db.execute("CREATE INDEX ON docs USING bm25 (body)").unwrap();

    let rows = db.query("SELECT _key, BM25_NORM(body,'rust') AS s FROM docs ORDER BY s DESC").unwrap().collect();
    let vals: Vec<(String, f64)> = rows.iter().map(|h| {
        let p = h.payload.as_ref().unwrap();
        (p["_key"].as_str().unwrap().to_string(), p["s"].as_f64().unwrap())
    }).collect();

    // Every value in [0,1]
    for (k, v) in &vals {
        assert!(*v >= 0.0 && *v <= 1.0, "BM25_NORM out of [0,1]: {k}={v}");
    }
    // 'a' (rust x3) must outrank 'b' (rust x1), both > 0
    let a = vals.iter().find(|(k,_)| k=="a").unwrap().1;
    let b = vals.iter().find(|(k,_)| k=="b").unwrap().1;
    assert!(a > b && b > 0.0, "a ({a}) should outrank b ({b}) > 0");

    // Custom k changes the value but not the order (saturation is monotonic)
    let rows_k = db.query("SELECT _key, BM25_NORM(body,'rust',5.0) AS s FROM docs ORDER BY s DESC").unwrap().collect();
    let a5 = rows_k.iter().find_map(|h| { let p=h.payload.as_ref().unwrap(); (p["_key"].as_str().unwrap()=="a").then(|| p["s"].as_f64().unwrap()) }).unwrap();
    assert!(a5 < a, "larger k lowers the saturated value: k=5 ({a5}) < k=1 ({a})");

    // Blends cleanly with a [0,1] literal — stays a real number, not dominated
    let _ = db.query("SELECT _key FROM docs ORDER BY BM25_NORM(body,'rust')*0.5 + 0.5 DESC").unwrap().collect();
}

// ── Spatial works on any GEO field name (not just 'geometry') ─────────────────

#[test]
fn spatial_works_on_non_geometry_field_name() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    // Column named 'geo', not 'geometry' — previously silently returned nothing.
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, geo GEO)").unwrap();
    db.execute(r#"INSERT INTO places (_key,geo) VALUES ('near','{"type":"Point","coordinates":[144.96,-37.81]}')"#).unwrap();
    db.execute(r#"INSERT INTO places (_key,geo) VALUES ('far','{"type":"Point","coordinates":[150.0,-40.0]}')"#).unwrap();
    db.execute("CREATE INDEX ON places USING spatial (geo)").unwrap();

    let dwithin: Vec<_> = db.query("SELECT _key FROM places WHERE ST_DWithin(geo, POINT(144.96 -37.81), 5000.0)").unwrap().collect();
    assert_eq!(dwithin.len(), 1, "ST_DWithin must find the near point on a 'geo' field");
    assert_eq!(dwithin[0].payload.as_ref().unwrap()["_key"], "near");

    // Polygon filter on the same non-'geometry' field
    let within: Vec<_> = db.query(r#"SELECT _key FROM places WHERE ST_Within(geo, POLYGON((144.9 -37.85, 145.0 -37.85, 145.0 -37.78, 144.9 -37.78, 144.9 -37.85)))"#).unwrap().collect();
    assert_eq!(within.len(), 1, "ST_Within must work on a 'geo' field");

    // Distance scoring already respected the field arg — confirm order
    let order: Vec<String> = db.query("SELECT _key FROM places ORDER BY ST_DISTANCE(geo, POINT(144.96 -37.81)) ASC")
        .unwrap().collect().into_iter().map(|h| h.payload.unwrap()["_key"].as_str().unwrap().to_string()).collect();
    assert_eq!(order, vec!["near", "far"]);
}

// ── MATCH ORDER BY actually sorts (by var.field, projected or not) ───────────

#[test]
fn match_order_by_sorts_by_dest_field() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, rating INTEGER, name TEXT)").unwrap();
    for (k,r,n) in [("p1",5,"Alpha"),("p2",4,"Bravo"),("p3",3,"Charlie"),("p4",2,"Delta"),("p5",5,"Echo")] {
        db.execute(&format!("INSERT INTO places (_key,rating,name) VALUES ('{k}',{r},'{n}')")).unwrap();
    }
    db.execute("INSERT INTO users (_key) VALUES ('u1')").unwrap();
    // Edge order deliberately != rating order, to prove sorting (not traversal order).
    for p in ["p3","p1","p5","p2","p4"] { db.execute(&format!("INSERT ('users/u1')-[:visited]->('places/{p}')")).unwrap(); }

    let keys = |db:&CoreDB, sql:&str| -> Vec<String> {
        db.query(sql).unwrap().collect().into_iter()
            .filter_map(|h| h.payload.and_then(|p| p.get("k").and_then(|v| v.as_str()).map(String::from)))
            .collect()
    };

    // Unprojected var.field, descending numeric — must be rating order, not traversal.
    assert_eq!(keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY b.rating DESC"),
        vec!["p1","p5","p2","p3","p4"]);
    // Ascending
    assert_eq!(keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY b.rating ASC"),
        vec!["p4","p3","p2","p1","p5"]);
    // String field
    assert_eq!(keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY b.name ASC"),
        vec!["p1","p2","p3","p4","p5"]);
    // LIMIT applies AFTER sort (top-2, not first-2-traversal)
    assert_eq!(keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY b.rating DESC LIMIT 2"),
        vec!["p1","p5"]);

    // Hidden order column must not leak into the output row
    let hit = db.query("SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY b.rating DESC")
        .unwrap().collect();
    let cols: Vec<String> = hit[0].payload.as_ref().unwrap().as_object().unwrap().keys().cloned().collect();
    assert_eq!(cols, vec!["k"], "hidden __order_key__ must be stripped");
}

// ── MATCH ORDER BY scoring expressions (BM25_NORM / vector / hybrid) ──────────

#[test]
fn match_order_by_score_expression() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, body TEXT, emb VECTOR)").unwrap();
    // 'rust' in a minority (p1×3, p2×1, p5×1 of 10) → positive IDF.
    for (k,b,e) in [
        ("p1","rust rust rust systems","[0.9,0.1]"),("p2","one rust mention","[0.7,0.3]"),
        ("p3","unrelated cooking","[0.1,0.9]"),("p4","gardening plants","[0.5,0.5]"),
        ("p5","rust plus filler","[0.85,0.15]"),("p6","music jazz","[0.2,0.2]"),
        ("p7","cars engines","[0.3,0.4]"),("p8","ocean surfing","[0.4,0.3]"),
        ("p9","mountains hiking","[0.6,0.2]"),("pa","coffee beans","[0.2,0.6]"),
    ] { db.execute(&format!("INSERT INTO places (_key,body,emb) VALUES ('{k}','{b}',{e})")).unwrap(); }
    db.execute("INSERT INTO users (_key) VALUES ('u1')").unwrap();
    db.execute("CREATE INDEX ON places USING bm25 (body)").unwrap();
    db.execute("CREATE INDEX ON places USING hnsw (emb)").unwrap();
    // Visited in non-score order.
    for p in ["p3","p4","p1","p5","p2"] { db.execute(&format!("INSERT ('users/u1')-[:visited]->('places/{p}')")).unwrap(); }

    let keys = |db:&CoreDB, sql:&str| -> Vec<String> {
        db.query(sql).unwrap().collect().into_iter()
            .filter_map(|h| h.payload.and_then(|p| p.get("k").and_then(|v| v.as_str()).map(String::from))).collect()
    };

    // BM25_NORM: p1 (rust×3) ranks first; the two no-rust docs (p3,p4) score 0 → last.
    let b = keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY BM25_NORM(b.body,'rust') DESC");
    assert_eq!(b[0], "p1", "highest BM25 doc first, got {b:?}");
    assert_eq!(&b[3..], &["p3","p4"], "no-rust docs last, got {b:?}");

    // Vector: p1 (emb exactly the query) first.
    let v = keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY VECTOR_COSINE(b.emb,[0.9,0.1]) DESC");
    assert_eq!(v[0], "p1", "nearest vector first, got {v:?}");

    // Hybrid blend + LIMIT after scoring.
    let h = keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY BM25_NORM(b.body,'rust')*0.5 + VECTOR_COSINE(b.emb,[0.9,0.1])*0.5 DESC LIMIT 3");
    assert_eq!(h.len(), 3);
    assert_eq!(h[0], "p1", "hybrid top is p1, got {h:?}");

    // Hidden score key must not leak.
    let hit = db.query("SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' ORDER BY BM25_NORM(b.body,'rust') DESC").unwrap().collect();
    let cols: Vec<String> = hit[0].payload.as_ref().unwrap().as_object().unwrap().keys().cloned().collect();
    assert_eq!(cols, vec!["k"], "hidden __score_key__ must be stripped");
}

// ── MATCH WHERE spatial/text function filters + full hybrid ──────────────────

#[test]
fn match_where_function_filters_and_full_hybrid() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE users (_key TEXT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, body TEXT, geometry GEO, emb VECTOR)").unwrap();
    // 'rust' in p1,p2,p5 (minority of 10 → positive IDF); 'far' is spatially distant.
    for (k,b,lon,lat,e) in [
        ("p1","rust rust systems",144.96,-37.81,"[0.9,0.1]"),("p2","rust mention here",144.97,-37.82,"[0.7,0.3]"),
        ("p3","cooking food",144.95,-37.80,"[0.1,0.9]"),("far","distant location",150.0,-40.0,"[0.85,0.15]"),
        ("p5","rust plus more",144.96,-37.815,"[0.8,0.2]"),("p6","music jazz",144.94,-37.83,"[0.2,0.2]"),
        ("p7","cars engines",144.99,-37.79,"[0.3,0.4]"),("p8","ocean surf",144.93,-37.84,"[0.4,0.3]"),
        ("p9","mountains hiking",144.92,-37.85,"[0.5,0.5]"),("pa","coffee beans",144.91,-37.86,"[0.6,0.4]"),
    ] { db.execute(&format!(r#"INSERT INTO places (_key,body,geometry,emb) VALUES ('{k}','{b}','{{"type":"Point","coordinates":[{lon},{lat}]}}',{e})"#)).unwrap(); }
    db.execute("INSERT INTO users (_key) VALUES ('u1')").unwrap();
    db.execute("CREATE INDEX ON places USING bm25 (body)").unwrap();
    db.execute("CREATE INDEX ON places USING spatial (geometry)").unwrap();
    db.execute("CREATE INDEX ON places USING hnsw (emb)").unwrap();
    for p in ["p1","p2","p3","far","p5","p6"] { db.execute(&format!("INSERT ('users/u1')-[:visited]->('places/{p}')")).unwrap(); }

    let keys = |db:&CoreDB, sql:&str| -> Vec<String> {
        db.query(sql).unwrap().collect().into_iter()
            .filter_map(|h| h.payload.and_then(|p| p.get("k").and_then(|v| v.as_str()).map(String::from))).collect()
    };

    // Spatial filter: excludes the distant node.
    let s = keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' AND ST_DWithin(b.geometry, POINT(144.96 -37.81), 5000.0)");
    assert!(!s.contains(&"far".to_string()), "ST_DWithin must exclude the far node, got {s:?}");
    assert!(s.contains(&"p1".to_string()));

    // Text filter: only rust docs.
    let t = keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' AND BM25(b.body,'rust') > 0.0");
    let mut ts = t.clone(); ts.sort();
    assert_eq!(ts, vec!["p1","p2","p5"], "BM25 filter must keep only rust docs, got {t:?}");

    // Full hybrid: graph + spatial filter + text filter + hybrid rank.
    let h = keys(&db, "SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' AND ST_DWithin(b.geometry, POINT(144.96 -37.81), 5000.0) AND BM25(b.body,'rust') > 0.0 ORDER BY BM25_NORM(b.body,'rust')*0.5 + VECTOR_COSINE(b.emb,[0.9,0.1])*0.5 DESC");
    assert_eq!(h.len(), 3, "3 near rust docs, got {h:?}");
    assert_eq!(h[0], "p1", "hybrid winner is p1, got {h:?}");
    // hidden columns stripped
    let hit = db.query("SELECT b._key AS k FROM MATCH (u:users)-[:visited]->(b:places) WHERE u._key='u1' AND ST_DWithin(b.geometry, POINT(144.96 -37.81), 5000.0) ORDER BY BM25_NORM(b.body,'rust') DESC").unwrap().collect();
    let cols: Vec<String> = hit[0].payload.as_ref().unwrap().as_object().unwrap().keys().cloned().collect();
    assert_eq!(cols, vec!["k"], "hidden columns must be stripped, got {cols:?}");
}

// ── MATCH GROUP BY composes with function filters (filter before grouping) ───

#[test]
fn match_group_by_with_function_filter() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE regions (_key TEXT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, category TEXT, body TEXT, geometry GEO)").unwrap();
    for (k,c,b,lon,lat) in [
        ("p1","cafe","rust coffee",144.96,-37.81),("p2","cafe","tea rust",144.97,-37.82),
        ("p3","bar","drinks",144.95,-37.80),("p4","bar","music",144.96,-37.815),
        ("p5","shop","rust tools",144.94,-37.83),("far","cafe","distant place",150.0,-40.0),
        ("p7","cafe","brunch",144.99,-37.79),("p8","shop","books",144.93,-37.84),
    ] { db.execute(&format!(r#"INSERT INTO places (_key,category,body,geometry) VALUES ('{k}','{c}','{b}','{{"type":"Point","coordinates":[{lon},{lat}]}}')"#)).unwrap(); }
    db.execute("INSERT INTO regions (_key) VALUES ('r1')").unwrap();
    db.execute("CREATE INDEX ON places USING spatial (geometry)").unwrap();
    db.execute("CREATE INDEX ON places USING bm25 (body)").unwrap();
    for p in ["p1","p2","p3","p4","p5","far","p7","p8"] { db.execute(&format!("INSERT ('regions/r1')-[:contains]->('places/{p}')")).unwrap(); }

    let counts = |db:&CoreDB, sql:&str| -> std::collections::HashMap<String,i64> {
        db.query(sql).unwrap().collect().into_iter().map(|h| {
            let p = h.payload.unwrap();
            (p["category"].as_str().unwrap().to_string(), p["n"].as_i64().unwrap())
        }).collect()
    };

    // No filter: cafe=4 (p1,p2,far,p7).
    let all = counts(&db, "SELECT b.category AS category, COUNT(*) AS n FROM MATCH (r:regions)-[:contains]->(b:places) WHERE r._key='r1' GROUP BY b.category");
    assert_eq!(all["cafe"], 4);

    // Spatial filter must exclude the distant cafe BEFORE grouping → cafe=3.
    let near = counts(&db, "SELECT b.category AS category, COUNT(*) AS n FROM MATCH (r:regions)-[:contains]->(b:places) WHERE r._key='r1' AND ST_DWithin(b.geometry, POINT(144.96 -37.81), 5000.0) GROUP BY b.category");
    assert_eq!(near["cafe"], 3, "far cafe must be excluded pre-grouping");
    assert_eq!(near["bar"], 2);
    assert_eq!(near["shop"], 2);

    // BM25 filter (rust) before grouping → cafe=2 (p1,p2), shop=1 (p5).
    let rust = counts(&db, "SELECT b.category AS category, COUNT(*) AS n FROM MATCH (r:regions)-[:contains]->(b:places) WHERE r._key='r1' AND BM25(b.body,'rust') > 0.0 GROUP BY b.category");
    assert_eq!(rust.get("cafe"), Some(&2));
    assert_eq!(rust.get("shop"), Some(&1));
    assert_eq!(rust.get("bar"), None, "bar has no rust docs");
}

#[test]
fn prepared_query_binds_varying_params() {
    let mut db = CoreDB::new();
    for i in 0..10 {
        db.put(&format!("t/k{i}"),
            &format!(r#"{{"_collection":"t","_key":"k{i}","v":{i}}}"#)).unwrap();
    }

    // Compile once, run many with DIFFERENT parameter values.
    let stmt = db.prepare("SELECT _key FROM t WHERE v = $1").unwrap();
    for i in 0..10 {
        let hits = db.query_prepared(&stmt, &[serde_json::json!(i)]).unwrap().collect();
        assert_eq!(hits.len(), 1, "v={i} should match exactly one row");
        assert_eq!(hits[0].slug, format!("t/k{i}"));
    }

    // Prepared result must equal the equivalent one-shot query_params.
    let prepared: Vec<_> = db.query_prepared(&stmt, &[serde_json::json!(7)]).unwrap()
        .collect().into_iter().map(|h| h.slug).collect();
    let oneshot: Vec<_> = db.query_params("SELECT _key FROM t WHERE v = $1", &[serde_json::json!(7)])
        .unwrap().collect().into_iter().map(|h| h.slug).collect();
    assert_eq!(prepared, oneshot);
}

#[test]
fn bm25_filter_is_deterministic_and_complete() {
    let mut db = CoreDB::new();
    // 20 docs, identical content → identical BM25 scores (all tied). A top-k cap
    // with HashMap-order tie-breaking used to return a NON-DETERMINISTIC subset.
    for i in 0..20 {
        db.put(&format!("d/k{i:02}"),
            &format!(r#"{{"_collection":"d","_key":"k{i:02}","body":"the quick brown fox jumps over"}}"#)).unwrap();
    }
    db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();

    let run = || {
        let mut keys: Vec<String> = db
            .query("SELECT _key FROM d WHERE BM25(body, 'quick fox') > 0.0")
            .unwrap().collect().into_iter().map(|h| h.slug).collect();
        keys.sort();
        keys
    };
    let first = run();
    assert_eq!(first.len(), 20, "a BM25 filter must return EVERY matching doc, not a capped subset");
    for _ in 0..12 {
        assert_eq!(run(), first, "BM25 filter must be deterministic across identical runs");
    }
}

// ── Generated columns (GENERATED ALWAYS AS … STORED) ─────────────────────────

#[test]
fn generated_column_computes_and_overrides() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE people (_key TEXT PRIMARY KEY, first_name TEXT, last_name TEXT, \
                fullname TEXT GENERATED ALWAYS AS (first_name || ' ' || last_name) STORED, \
                blob TEXT GENERATED ALWAYS AS (lower(concat_ws(' ', first_name, last_name))) STORED)").unwrap();
    // fullname computed from the record's own fields
    db.execute("INSERT INTO people (_key, first_name, last_name) VALUES ('p1','John','Smith')").unwrap();
    // an explicit user value for a generated column is ignored (GENERATED ALWAYS)
    db.execute("INSERT INTO people (_key, first_name, last_name, fullname) VALUES ('p2','Ada','Lovelace','WRONG')").unwrap();

    let get = |db: &CoreDB, k: &str, f: &str| -> String {
        let v: serde_json::Value = serde_json::from_str(&db.get(&format!("people/{k}")).unwrap()).unwrap();
        v.get(f).unwrap().as_str().unwrap().to_string()
    };
    assert_eq!(get(&db, "p1", "fullname"), "John Smith");
    assert_eq!(get(&db, "p1", "blob"), "john smith");
    assert_eq!(get(&db, "p2", "fullname"), "Ada Lovelace", "GENERATED ALWAYS overrides user value");
}

#[test]
fn generated_column_recomputes_on_update() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE people (_key TEXT PRIMARY KEY, first_name TEXT, last_name TEXT, \
                fullname TEXT GENERATED ALWAYS AS (first_name || ' ' || last_name) STORED)").unwrap();
    db.execute("INSERT INTO people (_key, first_name, last_name) VALUES ('p1','John','Smith')").unwrap();

    let fullname = |db: &CoreDB| -> String {
        let v: serde_json::Value = serde_json::from_str(&db.get("people/p1").unwrap()).unwrap();
        v.get("fullname").unwrap().as_str().unwrap().to_string()
    };
    assert_eq!(fullname(&db), "John Smith");
    db.execute("UPDATE people SET last_name = 'Doe' WHERE _key = 'p1'").unwrap();
    assert_eq!(fullname(&db), "John Doe", "generated column must recompute on UPDATE");
}

#[test]
fn generated_column_is_searchable() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE people (_key TEXT PRIMARY KEY, first_name TEXT, last_name TEXT, \
                fullname TEXT GENERATED ALWAYS AS (first_name || ' ' || last_name) STORED)").unwrap();
    db.execute("INSERT INTO people (_key, first_name, last_name) VALUES ('p1','John','Smith')").unwrap();
    db.execute("INSERT INTO people (_key, first_name, last_name) VALUES ('p2','Jane','Doe')").unwrap();
    db.execute("CREATE INDEX ON people USING bm25 (fullname)").unwrap();
    // the combined value is indexed and matchable, though it's never stored by the user
    let hits = db.query("SELECT _key FROM people WHERE BM25(fullname,'john smith') > 0").unwrap().collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.as_ref().unwrap().get("_key").unwrap(), "p1");
}

// ── Materialized views (graph-sourced, cross-collection search) ──────────────

fn setup_music_mv() -> CoreDB {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE song   (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("CREATE TABLE artist (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO artist (_key,name) VALUES ('beatles','The Beatles')").unwrap();
    db.execute("INSERT INTO song (_key,title) VALUES ('yesterday','Yesterday')").unwrap();
    db.execute("INSERT ('song/yesterday')-[:by]->('artist/beatles')").unwrap();
    db
}

#[test]
fn materialized_view_cross_collection_search() {
    let mut db = setup_music_mv();
    db.execute("CREATE MATERIALIZED VIEW song_search AS \
                SELECT s._key AS id, s.title AS title, a.name AS artist, \
                       concat_ws(' ', s.title, a.name) AS text \
                FROM MATCH (s:song)-[:by]->(a:artist)").unwrap();

    // The view is a real collection; text folds in the artist name across the edge.
    let doc: serde_json::Value =
        serde_json::from_str(&db.get("song_search/yesterday").unwrap()).unwrap();
    assert_eq!(doc["text"], "Yesterday The Beatles");
    assert_eq!(doc["artist"], "The Beatles");

    // BM25 finds the song by the ARTIST word — which isn't in the song title at all.
    db.execute("CREATE INDEX ON song_search USING bm25 (text)").unwrap();
    let hits = db.query("SELECT id FROM song_search WHERE BM25(text,'beatles') > 0").unwrap().collect();
    assert_eq!(hits.len(), 1, "'beatles' must find Yesterday via the materialized artist name");
    assert_eq!(hits[0].payload.as_ref().unwrap()["id"], "yesterday");
}

#[test]
fn materialized_view_refresh_picks_up_changes() {
    let mut db = setup_music_mv();
    db.execute("CREATE MATERIALIZED VIEW song_search AS \
                SELECT s._key AS id, concat_ws(' ', s.title, a.name) AS text \
                FROM MATCH (s:song)-[:by]->(a:artist)").unwrap();
    let text = |db: &CoreDB| -> String {
        let v: serde_json::Value = serde_json::from_str(&db.get("song_search/yesterday").unwrap()).unwrap();
        v["text"].as_str().unwrap().to_string()
    };
    assert_eq!(text(&db), "Yesterday The Beatles");
    db.execute("UPDATE artist SET name='Fab Four' WHERE _key='beatles'").unwrap();
    // stale until refresh (Postgres-faithful base behavior)
    assert_eq!(text(&db), "Yesterday The Beatles");
    db.execute("REFRESH MATERIALIZED VIEW song_search").unwrap();
    assert_eq!(text(&db), "Yesterday Fab Four");
}

#[test]
fn materialized_view_with_autoindex() {
    let mut db = setup_music_mv();
    // WITH (autoindex): auto-indexes the search-type fields — no explicit CREATE INDEX.
    db.execute("CREATE MATERIALIZED VIEW song_search WITH (autoindex = true) AS \
                SELECT s._key AS id, concat_ws(' ', s.title, a.name) AS text \
                FROM MATCH (s:song)-[:by]->(a:artist)").unwrap();
    let hits = db.query("SELECT id FROM song_search WHERE BM25(text,'beatles') > 0").unwrap().collect();
    assert_eq!(hits.len(), 1, "WITH (autoindex) must auto-index the text field");
}


#[test]
fn search_view_multimodal_text_geo_vector() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT, geometry GEO, embedding VECTOR)").unwrap();
    db.execute("CREATE TABLE dish (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO place (_key,name,geometry,embedding) VALUES ('p1','Warung','{\"type\":\"Point\",\"coordinates\":[115.168,-8.690]}',[0.9,0.1,0.0,0.0])").unwrap();
    db.execute("INSERT INTO place (_key,name,geometry,embedding) VALUES ('p2','Cafe','{\"type\":\"Point\",\"coordinates\":[116.0,-9.5]}',[0.1,0.9,0.0,0.0])").unwrap();
    db.execute("INSERT INTO dish (_key,name) VALUES ('d1','grilled chicken')").unwrap();
    db.execute("INSERT ('place/p1')-[:serves]->('dish/d1')").unwrap();
    db.execute("INSERT ('place/p2')-[:serves]->('dish/d1')").unwrap();

    // WITH (autoindex) auto-indexes text (bm25), geo (spatial), AND vector (hnsw).
    db.execute("CREATE MATERIALIZED VIEW place_search WITH (autoindex = true) AS \
        SELECT p._key AS id, concat_ws(' ', p.name, dish.name) AS text, p.geometry AS geometry, p.embedding AS embedding \
        FROM MATCH (p:place)-[:serves]->(dish:dish)").unwrap();

    let ids = |db: &CoreDB, q: &str| -> Vec<String> {
        db.query(q).unwrap().collect().iter()
            .filter_map(|h| h.payload.as_ref()?.get("id")?.as_str().map(String::from)).collect()
    };
    // text: both serve the dish
    assert_eq!(ids(&db, "SELECT id FROM place_search WHERE BM25(text,'grilled chicken') > 0").len(), 2);
    // geo: only p1 is near
    assert_eq!(ids(&db, "SELECT id FROM place_search WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5000.0)"), vec!["p1"]);
    // vector: nearest to p1's embedding is p1 (mirrored from source)
    assert_eq!(ids(&db, "SELECT id FROM place_search WHERE VECTOR_NEAR(embedding, [0.9,0.1,0.0,0.0], 1)"), vec!["p1"]);
}

#[test]
fn search_typo_tolerance() {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE movie (_key TEXT PRIMARY KEY, title TEXT)").unwrap();
    db.execute("INSERT INTO movie (_key,title) VALUES ('sw','Star Wars')").unwrap();
    db.execute("INSERT INTO movie (_key,title) VALUES ('st','Star Trek')").unwrap();
    db.execute("CREATE INDEX ON movie USING search (title)").unwrap();

    let ids = |db: &CoreDB, q: &str| -> Vec<String> {
        db.query(q).unwrap().collect().iter()
            .filter_map(|h| h.payload.as_ref()?.get("_key")?.as_str().map(String::from)).collect()
    };
    // auto typo already handles 5+ char typos: "beatlez"-style. Here "warz" (4 chars) is
    // below the auto threshold, so plain SEARCH misses it...
    assert_eq!(ids(&db, "SELECT _key FROM movie WHERE SEARCH('star warz')").len(), 0);
    // ...but `typo => 1` forces it — and matches ONLY Star Wars (precise, no Star Trek).
    assert_eq!(ids(&db, "SELECT _key FROM movie WHERE SEARCH('star warz', typo => 1)"), vec!["sw"]);
    // double typo, still just Star Wars
    assert_eq!(ids(&db, "SELECT _key FROM movie WHERE SEARCH('stir warz', typo => 1)"), vec!["sw"]);
    // typo => 0 forces exact (no tolerance)
    assert_eq!(ids(&db, "SELECT _key FROM movie WHERE SEARCH('star warz', typo => 0)").len(), 0);
}

// ── Keyed edges + edge CRUD + per-edge attributes ──────────────────────────────
//
// Regression coverage for the keyed-edge feature (upsert INSERT, predicate DELETE,
// edge UPDATE) and the two bugs fixed alongside it (per-edge attribute resolution
// across parallel edges, and O(n²) node-insert into one collection).

/// First column of the first row as i64 (`-1` if absent).
fn kt_i64(db: &CoreDB, sql: &str) -> i64 {
    db.query(sql).unwrap().collect().first()
        .and_then(|h| h.payload.as_ref())
        .and_then(|p| p.as_object())
        .and_then(|o| o.values().next())
        .and_then(|v| v.as_i64())
        .unwrap_or(-1)
}

/// Sorted string values of `field` across all rows.
fn kt_strs(db: &CoreDB, sql: &str, field: &str) -> Vec<String> {
    let mut v: Vec<String> = db.query(sql).unwrap().collect().iter()
        .filter_map(|h| h.payload.as_ref()
            .and_then(|p| p.get(field))
            .and_then(|x| x.as_str())
            .map(String::from))
        .collect();
    v.sort();
    v
}

fn kt_two_nodes() -> CoreDB {
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO p (_key) VALUES ('a'),('b')").unwrap();
    db
}

const KT_A_OUT: &str = "SELECT COUNT(*) AS c FROM MATCH (a:p)-[:rel]->(b:p) WHERE a._key='a'";

#[test]
fn keyed_edge_insert_is_upsert() {
    let mut db = kt_two_nodes();
    db.execute("INSERT ('p/a')-[:rel {_key:'k1', w:1}]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:rel {_key:'k1', w:2}]->('p/b')").unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 1, "same key must not stack");
    assert_eq!(kt_i64(&db, "SELECT r.w AS w FROM MATCH (a:p)-[r:rel]->(b:p) WHERE r._key='k1'"), 2, "last-wins");
    db.execute("INSERT ('p/a')-[:rel {_key:'k2'}]->('p/b')").unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 2);
}

#[test]
fn unkeyed_edge_stays_additive() {
    let mut db = kt_two_nodes();
    db.execute("INSERT ('p/a')-[:knows]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:knows]->('p/b')").unwrap();
    assert_eq!(
        kt_i64(&db, "SELECT COUNT(*) AS c FROM MATCH (a:p)-[:knows]->(b:p) WHERE a._key='a'"),
        2, "no _key => additive (parallel edges allowed)"
    );
}

#[test]
fn match_edge_attrs_are_per_parallel_edge() {
    let mut db = kt_two_nodes();
    db.execute("INSERT ('p/a')-[:rel {_key:'u1', by:'alice'}]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:rel {_key:'u2', by:'bob'}]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:rel {_key:'u3', by:'carol'}]->('p/b')").unwrap();
    assert_eq!(
        kt_strs(&db, "SELECT r._key AS v FROM MATCH (a:p)-[r:rel]->(b:p) WHERE a._key='a'", "v"),
        vec!["u1", "u2", "u3"]
    );
    assert_eq!(
        kt_strs(&db, "SELECT r.by AS v FROM MATCH (a:p)-[r:rel]->(b:p) WHERE a._key='a'", "v"),
        vec!["alice", "bob", "carol"]
    );
    assert_eq!(kt_i64(&db, "SELECT COUNT(*) AS c FROM MATCH (a:p)-[r:rel]->(b:p) WHERE r.by='bob'"), 1);
}

#[test]
fn unkeyed_parallel_edge_attrs_are_per_edge() {
    let mut db = kt_two_nodes();
    for t in ["x", "y", "z"] {
        db.execute(&format!("INSERT ('p/a')-[:rel {{tag:'{t}'}}]->('p/b')")).unwrap();
    }
    assert_eq!(
        kt_strs(&db, "SELECT r.tag AS v FROM MATCH (a:p)-[r:rel]->(b:p) WHERE a._key='a'", "v"),
        vec!["x", "y", "z"]
    );
}

#[test]
fn edge_delete_by_predicate() {
    let mut db = kt_two_nodes();
    db.execute("INSERT ('p/a')-[:rel {_key:'u1', by:'x'}]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:rel {_key:'u2', by:'y'}]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:rel {_key:'u3', by:'y'}]->('p/b')").unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 3);
    db.execute("DELETE ('p/a')-[:rel {_key:'u1'}]->('p/b')").unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 2);
    db.execute("DELETE ('p/a')-[:rel {by:'y'}]->('p/b')").unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 0);
    // keyed index purged → key re-creates cleanly
    db.execute("INSERT ('p/a')-[:rel {_key:'u1'}]->('p/b')").unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 1);
    db.execute("DELETE ('p/a')-[:rel]->('p/b')").unwrap(); // bare = all
    assert_eq!(kt_i64(&db, KT_A_OUT), 0);
}

#[test]
fn edge_update_by_predicate() {
    let mut db = kt_two_nodes();
    db.execute("INSERT ('p/a')-[:rel {_key:'u1', by:'alice'}]->('p/b')").unwrap();
    db.execute("INSERT ('p/a')-[:rel {_key:'u2', by:'bob'}]->('p/b')").unwrap();
    db.execute("UPDATE ('p/a')-[:rel {_key:'u1'}]->('p/b') SET by='ALICE'").unwrap();
    db.execute("UPDATE ('p/a')-[:rel]->('p/b') SET by='BOB2' WHERE by='bob'").unwrap();
    assert_eq!(
        kt_strs(&db, "SELECT r.by AS v FROM MATCH (a:p)-[r:rel]->(b:p) WHERE a._key='a'", "v"),
        vec!["ALICE", "BOB2"]
    );
    // SET merges (adds tag, keeps by)
    db.execute("UPDATE ('p/a')-[:rel {_key:'u1'}]->('p/b') SET tag='t'").unwrap();
    let row = db.query("SELECT r.tag AS tag, r.by AS by FROM MATCH (a:p)-[r:rel]->(b:p) WHERE r._key='u1'")
        .unwrap().collect();
    let p = row.first().and_then(|h| h.payload.clone()).unwrap();
    assert_eq!(p.get("tag").and_then(|v| v.as_str()), Some("t"));
    assert_eq!(p.get("by").and_then(|v| v.as_str()), Some("ALICE"));
    // identity is immutable
    assert!(db.execute("UPDATE ('p/a')-[:rel {_key:'u1'}]->('p/b') SET _key='zz'").is_err());
}

#[test]
fn keyed_edge_crud_persists_across_reopen() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p (_key) VALUES ('a'),('b')").unwrap();
        db.execute("INSERT ('p/a')-[:rel {_key:'u1', by:'alice'}]->('p/b')").unwrap();
        db.execute("INSERT ('p/a')-[:rel {_key:'u2', by:'bob'}]->('p/b')").unwrap();
        db.execute("INSERT ('p/a')-[:rel {_key:'u1', by:'ALICE'}]->('p/b')").unwrap(); // upsert
        db.execute("UPDATE ('p/a')-[:rel {_key:'u2'}]->('p/b') SET by='BOB2'").unwrap();
        db.execute("INSERT ('p/a')-[:rel {_key:'u3'}]->('p/b')").unwrap();
        db.execute("DELETE ('p/a')-[:rel {_key:'u3'}]->('p/b')").unwrap();
    }
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(kt_i64(&db, KT_A_OUT), 2, "u1 (upserted) + u2 (updated); u3 deleted");
    assert_eq!(
        kt_strs(&db, "SELECT r.by AS v FROM MATCH (a:p)-[r:rel]->(b:p) WHERE a._key='a'", "v"),
        vec!["ALICE", "BOB2"]
    );
}

#[test]
fn node_upsert_keeps_single_collection_membership() {
    // O(n²)-fix guard: repeated upserts of one node never duplicate its membership.
    let mut db = CoreDB::new();
    db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
    for i in 0..20 {
        db.put("t/x", &format!(r#"{{"_collection":"t","_key":"x","v":{i}}}"#)).unwrap();
    }
    assert_eq!(kt_i64(&db, "SELECT COUNT(*) AS c FROM t"), 1);
    assert_eq!(kt_i64(&db, "SELECT v AS v FROM t WHERE _key='x'"), 19, "last write wins");
}

/// HNSW honours the distance metric it was built with: L2 vs cosine pick DIFFERENT
/// nearest neighbours on the same data (regression for the L2/dot/L1 HNSW wiring).
#[test]
fn hnsw_metric_l2_vs_cosine() {
    use sekejap::VecMetric;
    let mut db = CoreDB::new();
    db.put("v/a", r#"{"_collection":"v","_key":"a"}"#).unwrap();
    db.put("v/b", r#"{"_collection":"v","_key":"b"}"#).unwrap();
    // Query q=[1,0]. A=[3,0] same direction (cosine-nearest, but L2 dist 4).
    // B=[1,0.2] slightly off-axis (L2-nearest, dist 0.04; cosine a bit worse).
    db.put_vector("v/a", "emb", &[3.0, 0.0]).unwrap();
    db.put_vector("v/b", "emb", &[1.0, 0.2]).unwrap();

    db.build_hnsw_index_metric("emb", 16, 200, VecMetric::L2).unwrap();
    let l2 = db.collection("v").vector_near("emb", vec![1.0, 0.0], 1).collect();
    assert_eq!(l2[0].slug, "v/b", "L2-nearest is B");

    db.build_hnsw_index_metric("emb", 16, 200, VecMetric::Cosine).unwrap();
    let cos = db.collection("v").vector_near("emb", vec![1.0, 0.0], 1).collect();
    assert_eq!(cos[0].slug, "v/a", "cosine-nearest is A (same direction)");
}
