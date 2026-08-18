//! Graph traversal against a breadth-first search computed in the test.
//!
//! An oracle, like `spatial_oracle`: the expected answer is worked out here from
//! the edge list, independently of the engine, rather than by asking the engine a
//! second way. Agreement checks cannot see a fault shared by both paths; this can.
//!
//! The graph is the headline feature, and the traversal machinery has changed in
//! this branch — adjacency moved into slotted pages, the read path was rewritten
//! to decode straight from the mapped page, and the edge records gained an owner
//! and a checksum. All of that is invisible to a test that only counts rows.

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 { if n == 0 { 0 } else { self.next() % n } }
}

const NODES: usize = 40;

/// Build a random directed graph and return its edge list, so the test knows the
/// truth without asking the database.
fn build(db: &mut CoreDB, rng: &mut Rng) -> Vec<(usize, usize, &'static str)> {
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    for i in 0..NODES {
        db.put(&format!("p/n{i}"),
               &json!({"_collection": "p", "_key": format!("n{i}"), "n": i as i64}).to_string())
          .unwrap();
    }
    let mut edges: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut seen: HashSet<(usize, usize, &'static str)> = HashSet::new();
    for from in 0..NODES {
        for _ in 0..rng.below(4) {
            let to = rng.below(NODES as u64) as usize;
            if to == from { continue }                       // no self-loops
            let kind = if rng.below(2) == 0 { "next" } else { "other" };
            if !seen.insert((from, to, kind)) { continue }    // no duplicates
            db.link(&format!("p/n{from}"), &format!("p/n{to}"), kind);
            edges.push((from, to, kind));
        }
    }
    db.compact().unwrap();
    edges
}

/// Everything reachable from `start` in exactly `hops` steps along `kind`.
fn reachable_in(edges: &[(usize, usize, &str)], start: usize, hops: usize, kind: &str)
    -> HashSet<usize>
{
    let mut frontier: HashSet<usize> = HashSet::from([start]);
    for _ in 0..hops {
        let mut next = HashSet::new();
        for &(f, t, k) in edges {
            if k == kind && frontier.contains(&f) { next.insert(t); }
        }
        frontier = next;
    }
    frontier
}

/// Shortest-path distance from `start` along any edge type, by BFS.
fn distances(edges: &[(usize, usize, &str)], start: usize) -> HashMap<usize, usize> {
    let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(f, t, _) in edges { adj.entry(f).or_default().push(t); }
    let mut dist = HashMap::from([(start, 0usize)]);
    let mut q = VecDeque::from([start]);
    while let Some(u) = q.pop_front() {
        let d = dist[&u];
        for &v in adj.get(&u).into_iter().flatten() {
            if !dist.contains_key(&v) { dist.insert(v, d + 1); q.push_back(v); }
        }
    }
    dist
}

/// A `MATCH` projection carries its answer in the payload and leaves the slug
/// empty — the row is a combination of bound variables, not one node. Reading
/// `slug` gives a set of empty strings, which is how this test first "failed".
fn keys(db: &CoreDB, sql: &str) -> HashSet<String> {
    db.query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .map(|h| {
            if !h.slug.is_empty() { return h.slug.clone() }
            // The projected column is named after the variable it came from —
            // `b._key`, not `_key` — and its value is the bare key rather than
            // the slug, so take whatever single string the row carries.
            h.payload.as_ref()
                .and_then(|p| p.as_object())
                .and_then(|m| m.values().find_map(|v| v.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_default()
        })
        .collect()
}

#[test]
fn one_hop_matches_the_edge_list() {
    for (label, cfg) in [("default", Config::default()), ("resident", Config::resident())] {
        for round in 0..6u64 {
            let dir = tempfile::TempDir::new().unwrap();
            let mut rng = Rng(0x6A4Fu64.wrapping_add(round.wrapping_mul(0x9E37_79B9)));
            let edges = {
                let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
                build(&mut db, &mut rng)
            };
            let db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();

            // `forward` from every node, against the edge list.
            for from in 0..NODES {
                let want: HashSet<String> = edges.iter()
                    .filter(|(f, _, k)| *f == from && *k == "next")
                    .map(|(_, t, _)| format!("p/n{t}"))
                    .collect();
                let got: HashSet<String> = db.one(&format!("p/n{from}"))
                    .forward("next").collect()
                    .iter().map(|h| h.slug.clone()).collect();
                assert_eq!(got, want,
                    "[{label}] round {round}: forward('next') from p/n{from} disagrees \
                     with the edge list\n  engine = {got:?}\n  oracle = {want:?}");
            }

            // And the whole one-hop relation through MATCH.
            // Bare keys here: the projection returns `b._key`, which is the key
            // itself, where `forward()` above returns whole nodes with slugs.
            let want: HashSet<String> = edges.iter()
                .filter(|(_, _, k)| *k == "next")
                .map(|(_, t, _)| format!("n{t}"))
                .collect();
            let got = keys(&db, "SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)");
            assert_eq!(got, want,
                "[{label}] round {round}: MATCH (a)-[:next]->(b) disagrees with the \
                 edge list\n  engine = {got:?}\n  oracle = {want:?}");
        }
    }
}

#[test]
fn two_hops_match_a_breadth_first_search() {
    for round in 0..6u64 {
        let dir = tempfile::TempDir::new().unwrap();
        let mut rng = Rng(0x2B0F5u64.wrapping_add(round.wrapping_mul(0x9E37_79B9)));
        let edges = {
            let mut db = CoreDB::open(dir.path()).unwrap();
            build(&mut db, &mut rng)
        };
        let db = CoreDB::open(dir.path()).unwrap();

        for start in [0usize, 3, 11, 27] {
            let want: HashSet<String> = reachable_in(&edges, start, 2, "next")
                .into_iter().map(|n| format!("p/n{n}")).collect();
            let got: HashSet<String> = db.one(&format!("p/n{start}"))
                .forward("next").forward("next").collect()
                .iter().map(|h| h.slug.clone()).collect();
            assert_eq!(got, want,
                "round {round}: two hops of 'next' from p/n{start} disagree with a \
                 breadth-first search\n  engine = {got:?}\n  oracle = {want:?}");
        }
    }
}

/// `MATCH SHORTEST` must find a path exactly when a BFS finds one.
#[test]
fn shortest_finds_a_path_exactly_when_one_exists() {
    for round in 0..6u64 {
        let dir = tempfile::TempDir::new().unwrap();
        let mut rng = Rng(0x5B0C7u64.wrapping_add(round.wrapping_mul(0x9E37_79B9)));
        let edges = {
            let mut db = CoreDB::open(dir.path()).unwrap();
            build(&mut db, &mut rng)
        };
        let db = CoreDB::open(dir.path()).unwrap();

        let start = 0usize;
        let dist = distances(&edges, start);
        let mut checked = 0;
        for target in [1usize, 5, 9, 17, 23, 31, 39] {
            let sql = format!(
                "SELECT b._key FROM MATCH SHORTEST (a)-[r*]->(b) \
                 WHERE a._key = 'p/n{start}' AND b._key = 'p/n{target}'");
            let found = !keys(&db, &sql).is_empty();
            let expected = target != start && dist.contains_key(&target);
            assert_eq!(found, expected,
                "round {round}: SHORTEST from p/n{start} to p/n{target} said \
                 {found}, a breadth-first search says {expected} (distance {:?})",
                dist.get(&target));
            checked += 1;
        }
        assert_eq!(checked, 7);
    }
}
