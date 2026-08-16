// Why does the spatial query on a materialized view intermittently return nothing?
use sekejap::CoreDB;
fn main() {
    for run in 0..1 {
        let mut db = CoreDB::new();
        db.execute("CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT, geometry GEO, embedding VECTOR)").unwrap();
        db.execute("CREATE TABLE dish (_key TEXT PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO place (_key,name,geometry,embedding) VALUES ('p1','Warung','{\"type\":\"Point\",\"coordinates\":[115.168,-8.690]}',[0.9,0.1,0.0,0.0])").unwrap();
        db.execute("INSERT INTO place (_key,name,geometry,embedding) VALUES ('p2','Cafe','{\"type\":\"Point\",\"coordinates\":[116.0,-9.5]}',[0.1,0.9,0.0,0.0])").unwrap();
        db.execute("INSERT INTO dish (_key,name) VALUES ('d1','grilled chicken')").unwrap();
        db.execute("INSERT ('place/p1')-[:serves]->('dish/d1')").unwrap();
        db.execute("INSERT ('place/p2')-[:serves]->('dish/d1')").unwrap();
        db.execute("CREATE MATERIALIZED VIEW place_search WITH (autoindex = true) AS \
            SELECT p._key AS id, concat_ws(' ', p.name, dish.name) AS text, p.geometry AS geometry, p.embedding AS embedding \
            FROM MATCH (p:place)-[:serves]->(dish:dish)").unwrap();

        let rows = db.query("SELECT id FROM place_search").unwrap().collect();
        let spatial = db.query("SELECT id FROM place_search WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5000.0)")
            .map(|s| s.collect().len()).unwrap_or(9999);
        // what the sampled row actually looked like
        // exactly what the test does: pull "id" out of each hit's payload
        let ids = |q: &str| -> Vec<String> {
            db.query(q).unwrap().collect().iter()
                .filter_map(|h| h.payload.as_ref()?.get("id")?.as_str().map(String::from)).collect()
        };
        // the test runs BM25 FIRST — replicate that ordering
        let bm25_n = ids("SELECT id FROM place_search WHERE BM25(text,'grilled chicken') > 0").len();
        let geo_ids = ids("SELECT id FROM place_search WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5000.0)");
        let vec_ids = ids("SELECT id FROM place_search WHERE VECTOR_NEAR(embedding, [0.9,0.1,0.0,0.0], 1)");
        let st = db.stats();
        // Is the SOURCE vector there, and did it get mirrored onto the view row?
        let src_p1  = db.get_vector("place/p1", "embedding").is_some();
        let view_p1 = db.get_vector("place_search/p1", "embedding").is_some();
        let view_p2 = db.get_vector("place_search/p2", "embedding").is_some();
        print!("bm25={bm25_n} vector={vec_ids:?} hnsw_idx={} src_p1={src_p1} view_p1={view_p1} view_p2={view_p2} ",
            st.hnsw_indexes);
        let raw = db.query("SELECT id FROM place_search WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5000.0)")
            .unwrap().collect();
        let payloads: Vec<String> = raw.iter()
            .map(|h| h.payload.as_ref().map(|p| p.to_string()).unwrap_or_else(|| "<no payload>".into()))
            .collect();
        println!("hits={} ids={:?} payloads={:?}", raw.len(), geo_ids, payloads);
        let _ = (run, rows, spatial);
    }
}
