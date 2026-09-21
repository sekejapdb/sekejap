//! Parity with the paper's embedded-intelligence demo: the assistant's tool
//! layer drives four search models. This test mirrors the demo scenario's
//! SHAPE (venues serving dishes + reviewed titles, generic data) and pins
//! that three of the four models answer TODAY through the kernel API; the
//! spatial call is phase 2i's gate fixture and is marked below.

use kernel::graph::{Graph, Metric};
use kernel::spatial::Geom;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

fn cfg() -> Config {
    Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

/// The demo's hash-bag embedding, reproduced independently (32 dims).
fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; 32];
    for tok in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if tok.is_empty() { continue; }
        let mut h: u64 = 0xcbf29ce484222325;
        for b in tok.bytes() { h ^= b as u64; h = h.wrapping_mul(0x100000001b3); }
        v[(h % 32) as usize] += 1.0;
        v[((h >> 8) % 32) as usize] += 0.5;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / n).collect()
}

const L_VENUE: u64 = 1;
const L_DISH: u64 = 2;
const L_TITLE: u64 = 3;
const REL_SERVES: u64 = 1;
const F_DESC: u64 = 1;
const F_REVIEW: u64 = 2;

#[test]
fn the_demo_tool_layer_maps_onto_the_kernel() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();

    // venues serving dishes (graph + fulltext on dish descriptions)
    // (name, (lon, lat), menu) -- three venues around a town centre
    let venues = [
        ("The Copper Kettle", (145.1290, -37.9105), vec![
            ("Fried Rice Special", "fried rice with chicken and prawn"),
            ("Grilled Fowl", "grilled chicken with sweet glaze and rice")]),
        ("Harbour Grill", (145.1320, -37.9150), vec![
            ("Mixed Grill Plate", "grilled chicken and lamb skewers with rice"),
            ("Chickpea Wrap", "chickpea patties with garlic sauce")]),
        ("Noodle Corner", (145.1205, -37.9179), vec![
            ("Beef Noodle Soup", "beef noodle soup with herbs"),
            ("Chicken Baguette", "chicken baguette with pickled carrot")]),
    ];
    let mut venue_ids = Vec::new();
    for (name, (lon, lat), menu) in &venues {
        let vid = g.add_node(None, L_VENUE, name.as_bytes()).unwrap();
        g.set_geo(1, vid, &Geom::Point(*lon, *lat)).unwrap();
        venue_ids.push(vid);
        for (dish, desc) in menu {
            let did = g.add_node(None, L_DISH, dish.as_bytes()).unwrap();
            g.index_text(F_DESC, did, &format!("{dish} {desc}")).unwrap();
            g.add_edge(0, vid, REL_SERVES, did, b"").unwrap();
        }
    }

    // reviewed titles with embeddings (vector + fulltext on reviews)
    let titles = [
        ("Layered Dreams", "a thief enters dreams within dreams to plant an idea, mind bending heist across layered dream worlds"),
        ("The Simulation", "a hacker discovers reality is a simulation, mind bending story about machines and simulated worlds"),
        ("Beyond the Wormhole", "explorers travel through a wormhole in space, time dilation and black holes"),
        ("The Bathhouse", "a girl wanders into a spirit world bathhouse, magical animated fantasy adventure"),
    ];
    let mut title_ids = Vec::new();
    for (t, review) in &titles {
        let id = g.add_node(None, L_TITLE, t.as_bytes()).unwrap();
        title_ids.push(id);
        g.index_text(F_REVIEW, id, review).unwrap();
        g.set_vec(VF, id, &embed(review)).unwrap();
    }
    g.commit().unwrap();

    // TOOL 1 -- graph: "menu at The Copper Kettle"
    let menu: Vec<Vec<u8>> = g.out_edges(0, venue_ids[0], Some(REL_SERVES)).unwrap()
        .map(|e| e.unwrap().2).map(|did| g.get_node(did).unwrap().unwrap().1)
        .collect();
    assert_eq!(menu.len(), 2);
    assert!(menu.iter().any(|p| p == b"Fried Rice Special"));

    // TOOL 2 -- fulltext + graph: "grilled chicken" -> dishes -> their venue
    let hits = g.text_search(F_DESC, "grilled chicken", 5).unwrap();
    assert!(!hits.is_empty());
    let top_dish = hits[0].0;
    // in_edges yields (true_source, ty, queried_dst, props)
    let venue = g.in_edges(0, top_dish, Some(REL_SERVES)).unwrap()
        .next().unwrap().unwrap().0;
    assert!(venue_ids.contains(&venue), "dish must trace back to a venue");

    // TOOL 3 -- vector: "something like Layered Dreams", self excluded
    let q = embed("mind bending story about dream worlds");
    let sim: Vec<u64> = g.nearest(VF, &q, 3, Metric::Cosine, 8).unwrap()
        .into_iter().map(|(id, _)| id)
        .filter(|id| *id != title_ids[0]) // exclude the seed title itself
        .collect();
    assert_eq!(sim.first(), Some(&title_ids[1]),
               "The Simulation must be the nearest non-self title");

    // TOOL 4 -- fulltext relevance: "space wormhole"
    let ft = g.text_search(F_REVIEW, "space wormhole", 3).unwrap();
    assert_eq!(ft[0].0, title_ids[2], "Beyond the Wormhole must rank first");

    // TOOL 5 -- the flagship combo, real at last (2i): "closest grilled
    // chicken" = ST_DWithin radius, hop the serves edges, BM25-filter the
    // dishes, order by TRUE metres -- the demo's nearest_serving.
    let (user_lat, user_lon) = (-37.9152, 145.1290);
    let near = g.within_radius(1, user_lat, user_lon, 6_000.0, 10).unwrap();
    assert_eq!(near.len(), 3, "all three venues are inside 6km");
    let mut best: Option<(u64, u64, f64)> = None; // (venue, dish, dist)
    for (vid, dist) in &near {
        for e in g.out_edges(0, *vid, Some(REL_SERVES)).unwrap() {
            let did = e.unwrap().2;
            let hit = g.text_search(F_DESC, "grilled chicken", 50).unwrap()
                .iter().any(|(id, _)| *id == did);
            if hit && best.map(|(_, _, d)| *dist < d).unwrap_or(true) {
                best = Some((*vid, did, *dist));
            }
        }
    }
    let (venue, _dish, dist) = best.expect("someone nearby grills chicken");
    // Harbour Grill is nearest to the user among grilled-chicken venues
    assert_eq!(venue, venue_ids[1], "nearest grilled chicken venue");
    assert!(dist > 1.0 && dist < 1_000.0, "sane metres: {dist}");
    // and the ST_Distance atom prices any candidate for hybrid scoring
    let d0 = g.st_distance(1, venue_ids[0], user_lat, user_lon).unwrap().unwrap();
    assert!(d0 > dist, "Copper Kettle is farther than the winner");
}
