//! Graph ops vs plain collections. Any disagreement is the graph's.
//! Mutation-checked below writing: label-scan skip, redge skip, and a
//! non-persisted id allocator must each make this fail.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};
use std::collections::{BTreeMap, BTreeSet};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

fn cfg() -> Config {
    Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

#[test]
fn graph_ops_agree_with_a_model() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let mut rng = Rng(0xBEEF);

    // model
    let mut nodes: BTreeMap<u64, (u64, Vec<u8>)> = BTreeMap::new();
    let mut exts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let mut edges: BTreeSet<(u64, u64, u64)> = BTreeSet::new();
    let labels = [7u64, 8, 9];
    let types = [1u64, 2];

    for batch in 0..20 {
        for _ in 0..300 {
            match rng.below(10) {
                0..=3 => {
                    let l = labels[rng.below(3) as usize];
                    let props = vec![b'p'; 1 + rng.below(200) as usize];
                    let ext = format!("uuid-{}-{}", batch, rng.below(100000)).into_bytes();
                    let id = g.add_node(Some(&ext), l, &props).unwrap();
                    assert!(!nodes.contains_key(&id), "id {id} handed out twice");
                    nodes.insert(id, (l, props));
                    exts.insert(ext, id);
                }
                _ => {
                    if nodes.len() < 2 { continue; }
                    let ids: Vec<u64> = nodes.keys().copied().collect();
                    let s = ids[rng.below(ids.len() as u64) as usize];
                    let t = ids[rng.below(ids.len() as u64) as usize];
                    let ty = types[rng.below(2) as usize];
                    g.add_edge(0, s, ty, t, b"ep").unwrap();
                    edges.insert((s, ty, t));
                }
            }
        }
        g.commit().unwrap();

        // full agreement check
        for (&id, (l, props)) in &nodes {
            let got = g.get_node(id).unwrap().unwrap();
            assert_eq!(got, (*l, props.clone()), "node {id}");
        }
        for (ext, &id) in &exts {
            assert_eq!(g.resolve(ext).unwrap(), Some(id), "resolve {ext:?}");
        }
        assert_eq!(g.resolve(b"no-such-ext").unwrap(), None);
        for &l in &labels {
            let want: BTreeSet<u64> =
                nodes.iter().filter(|(_, (nl, _))| *nl == l).map(|(&i, _)| i).collect();
            let got: BTreeSet<u64> =
                g.nodes_with_label(l).unwrap().map(|r| r.unwrap()).collect();
            assert_eq!(got, want, "label {l}");
        }
        for &id in nodes.keys() {
            let out: BTreeSet<(u64, u64)> = g.out_edges(0, id, None).unwrap()
                .map(|r| { let (_, ty, dst, _) = r.unwrap(); (ty, dst) }).collect();
            let want: BTreeSet<(u64, u64)> = edges.iter()
                .filter(|(s, _, _)| *s == id).map(|(_, ty, dst)| (*ty, *dst)).collect();
            assert_eq!(out, want, "out {id}");
            let inn: BTreeSet<(u64, u64)> = g.in_edges(0, id, None).unwrap()
                .map(|r| { let (src, ty, _, _) = r.unwrap(); (ty, src) }).collect();
            let wanti: BTreeSet<(u64, u64)> = edges.iter()
                .filter(|(_, _, dd)| *dd == id).map(|(s, ty, _)| (*ty, *s)).collect();
            assert_eq!(inn, wanti, "in {id}");
        }
    }

    // typed hop narrows the range
    if let Some((&any, _)) = nodes.iter().next() {
        let t1: BTreeSet<u64> = g.out_edges(0, any, Some(1)).unwrap()
            .map(|r| r.unwrap().2).collect();
        let want: BTreeSet<u64> = edges.iter()
            .filter(|(s, ty, _)| *s == any && *ty == 1).map(|(_, _, d)| *d).collect();
        assert_eq!(t1, want, "typed hop");
    }

    // bfs agrees with model reachability
    let ids: Vec<u64> = nodes.keys().copied().collect();
    let root = ids[0];
    let got: BTreeSet<u64> = g.bfs(0, root, None, 3).unwrap().into_iter().collect();
    let mut want = BTreeSet::new();
    let mut fr = vec![root];
    let mut seen: BTreeSet<u64> = [root].into();
    for _ in 0..3 {
        let mut nx = vec![];
        for s in fr {
            for (_, _, d) in edges.iter().filter(|(ss, _, _)| *ss == s) {
                if seen.insert(*d) { want.insert(*d); nx.push(*d); }
            }
        }
        fr = nx;
    }
    assert_eq!(got, want, "bfs reachability");

    // id allocator survives a reopen: new ids must not collide with old.
    let max_id = *nodes.keys().max().unwrap();
    g.checkpoint().unwrap();
    drop(g);
    let mut g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    let fresh = g.add_node(None, 7, b"x").unwrap();
    assert!(fresh > max_id, "reopened allocator reused id {fresh} (max was {max_id})");
    // and everything still reads
    for (ext, &id) in exts.iter().take(50) {
        assert_eq!(g.resolve(ext).unwrap(), Some(id), "post-reopen resolve");
    }
}
