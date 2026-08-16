//! Nodes on pages, addressed by slug hash — the last structure a compaction rebuilds.
//!
//! # What this replaces
//!
//! Four files, all written whole by every compaction:
//!
//! | file | holds | why it cannot be written into |
//! |---|---|---|
//! | `nodes.bin` | fixed records by dense id | a dense id is a *position*; inserting one node renumbers the rest |
//! | `idx.bin` | sorted `(hash, dense id)` | a sorted array has nowhere to put a new entry |
//! | `slugs.bin` | dense id → slug, via an offsets array | same offsets problem as CSR adjacency |
//! | `collections.bin` | per-collection member lists | a list that grows shifts every list after it |
//!
//! Each is excellent at being read and incapable of being changed, so writes pile
//! up in RAM until a rebuild folds them back — the rebuild whose cost is set by the
//! size of the store rather than the size of the change.
//!
//! Here a node is one record in slotted pages, found through a B+tree on its slug
//! hash. Writing a node touches its record and its index entry. Nothing is
//! renumbered, because nothing was numbered.
//!
//! # The record
//!
//! ```text
//!   0  collection_hash u64     which collection the node belongs to
//!   8  payload_offset  u64     where its JSON lives (a byte offset, or a record id)
//!  16  payload_len     u32
//!  20  flags           u32     bit 0: a spatial extent follows
//!  24  [6 x f64]               centroid lat/lon and bounding box, when flagged
//!  ..  slug bytes              to the end of the record
//! ```
//!
//! The slug runs to the end so it needs no length of its own, and the spatial
//! extent is present only for nodes that have geometry — it is 48 bytes, and
//! inline it would cost that on every node in the database whether or not it is a
//! place.
//!
//! Roughly 40 bytes for a typical node against the 48 the two files it replaces
//! spend (32 in `nodes.bin`, 16 in `slugs.bin`), plus the index. See
//! `the_disk_cost_against_the_files_it_replaces`, which measures rather than
//! claims it.
//!
//! # Collections
//!
//! Membership is a second B+tree keyed by `(collection hash, node hash)` packed
//! into one `u128`. Every member of a collection is then a contiguous run, so
//! "scan this table" is a range scan rather than a structure that has to be
//! rebuilt to stay sorted. Adding a node to a collection is one insert.

use super::btree::BTree;
use super::pagedstore::PagedStore;
use std::io;
use std::path::Path;

/// A node as stored: everything about it except its edges and its payload bytes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredNode {
    pub collection: u64,
    pub payload_offset: u64,
    pub payload_len: u32,
    /// `[centroid_lat, centroid_lon, min_lat, min_lon, max_lat, max_lon]`.
    pub spatial: Option<[f64; 6]>,
    pub slug: String,
}

const HEADER: usize = 24;
const SPATIAL_BYTES: usize = 48;
const FLAG_SPATIAL: u32 = 1;

fn rd32(b: &[u8], at: usize) -> u32 { u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) }
fn rd64(b: &[u8], at: usize) -> u64 { u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) }

fn encode(n: &StoredNode, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(HEADER + n.slug.len() + if n.spatial.is_some() { SPATIAL_BYTES } else { 0 });
    out.extend_from_slice(&n.collection.to_le_bytes());
    out.extend_from_slice(&n.payload_offset.to_le_bytes());
    out.extend_from_slice(&n.payload_len.to_le_bytes());
    out.extend_from_slice(&if n.spatial.is_some() { FLAG_SPATIAL } else { 0 }.to_le_bytes());
    if let Some(s) = &n.spatial {
        for v in s { out.extend_from_slice(&v.to_le_bytes()) }
    }
    out.extend_from_slice(n.slug.as_bytes());
}

/// Decode a stored node, or `None` if the bytes cannot be one.
///
/// Records come off disk. A record shorter than its own header, or one flagged as
/// carrying geometry without room for it, is refused rather than read past —
/// every length here is checked against what was actually returned.
fn decode(b: &[u8]) -> Option<StoredNode> {
    if b.len() < HEADER { return None }
    let flags = rd32(b, 20);
    let mut at = HEADER;
    let spatial = if flags & FLAG_SPATIAL != 0 {
        if b.len() < HEADER + SPATIAL_BYTES { return None }
        let mut s = [0f64; 6];
        for (i, slot) in s.iter_mut().enumerate() {
            *slot = f64::from_le_bytes(b[at + i * 8..at + i * 8 + 8].try_into().unwrap());
        }
        at += SPATIAL_BYTES;
        Some(s)
    } else {
        None
    };
    Some(StoredNode {
        collection: rd64(b, 0),
        payload_offset: rd64(b, 8),
        payload_len: rd32(b, 16),
        spatial,
        // A slug that is not valid UTF-8 is damage, not a node. Losing the slug
        // would make the node unaddressable, so the record is refused instead.
        slug: std::str::from_utf8(&b[at..]).ok()?.to_string(),
    })
}

/// `(collection, node)` as one key, so a collection's members are a contiguous run.
fn member_key(collection: u64, node: u64) -> u128 {
    ((collection as u128) << 64) | node as u128
}

pub(crate) struct NodeStore {
    store: PagedStore,
    /// `(collection hash, node hash) -> 1`. The value is unused; the key is the
    /// whole point, because a range over one collection's prefix is its membership.
    members: BTree,
    scratch: Vec<u8>,
}

impl NodeStore {
    pub(crate) fn open(dir: &Path, page_size: usize) -> io::Result<Self> {
        let members_path = dir.join("nodesp_coll.idx");
        let members = match BTree::open(&members_path)? {
            Some(t) => t,
            None => BTree::create(&members_path, page_size)?,
        };
        Ok(Self {
            store: PagedStore::open_named(dir, "nodesp", page_size)?,
            members,
            scratch: Vec::new(),
        })
    }

    pub(crate) fn len(&self) -> u64 { self.store.len() }

    /// Pages held by the node records, their index, and the membership index.
    pub(crate) fn page_counts(&self) -> (u64, u64, u64) {
        let (rec, idx) = self.store.page_counts();
        (rec, idx, self.members.page_count())
    }

    pub(crate) fn get(&self, hash: u64) -> io::Result<Option<StoredNode>> {
        Ok(self.store.get(hash as u128)?.and_then(|b| decode(&b)))
    }

    /// Whether the store holds this node, without reading its record.
    pub(crate) fn contains(&self, hash: u64) -> io::Result<bool> {
        Ok(self.store.get(hash as u128)?.is_some())
    }

    /// Store a node, replacing any previous version.
    ///
    /// A node that changes collection has to leave the old one's membership as well
    /// as join the new one's, or it would be returned by scans of both. That is the
    /// reason this reads the previous record first.
    pub(crate) fn put(&mut self, hash: u64, node: &StoredNode) -> io::Result<()> {
        let previous = self.get(hash)?;
        let mut scratch = std::mem::take(&mut self.scratch);
        encode(node, &mut scratch);
        let r = self.store.put(hash as u128, &scratch);
        self.scratch = scratch;
        r?;
        match previous {
            Some(p) if p.collection == node.collection => {}
            Some(p) => {
                self.members.remove(member_key(p.collection, hash))?;
                self.members.insert(member_key(node.collection, hash), 1)?;
            }
            None => { self.members.insert(member_key(node.collection, hash), 1)?; }
        }
        Ok(())
    }

    pub(crate) fn delete(&mut self, hash: u64) -> io::Result<bool> {
        let Some(node) = self.get(hash)? else { return Ok(false) };
        self.store.delete(hash as u128)?;
        self.members.remove(member_key(node.collection, hash))?;
        Ok(true)
    }

    /// Every member of one collection, as node hashes.
    ///
    /// A range over the collection's prefix — the members are stored adjacently, so
    /// this reads only the pages holding them. `collections.bin` answered the same
    /// question and had to be rebuilt in full to stay sorted.
    pub(crate) fn members(&self, collection: u64) -> io::Result<Vec<u64>> {
        Ok(self.members
            .range(member_key(collection, 0), member_key(collection, u64::MAX))?
            .into_iter()
            .map(|(k, _)| k as u64)
            .collect())
    }

    /// Every node hash, one index page at a time.
    ///
    /// Streams: a store's worth of hashes is 8 bytes each with nothing bounding it,
    /// and holding them is the RAM-proportional-to-the-store Law 1 forbids.
    /// `f` returning `false` stops the walk.
    pub(crate) fn for_each_hash(&self, mut f: impl FnMut(u64) -> bool) -> io::Result<()> {
        self.store.for_each_key(|k, _| f(k as u64))
    }

    pub(crate) fn sync(&mut self) -> io::Result<()> {
        self.store.sync()?;
        self.members.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pagestore::DEFAULT_PAGE_SIZE;
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;

    fn store(dir: &tempfile::TempDir) -> NodeStore {
        NodeStore::open(dir.path(), DEFAULT_PAGE_SIZE).unwrap()
    }
    fn h(i: u64) -> u64 { i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5851_F42D_4C95_7F2D }
    fn coll(name: &str) -> u64 { crate::sk_hash(name) }

    fn node(i: u64, c: &str) -> StoredNode {
        StoredNode {
            collection: coll(c),
            payload_offset: i * 97,
            payload_len: (i % 500) as u32,
            spatial: None,
            slug: format!("{c}/n{i}"),
        }
    }
    fn geo_node(i: u64, c: &str) -> StoredNode {
        StoredNode {
            spatial: Some([-37.81 + i as f64 * 1e-4, 144.96, -37.9, 144.9, -37.7, 145.0]),
            ..node(i, c)
        }
    }

    #[test]
    fn nodes_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..5_000 { s.put(h(i), &node(i, "p")).unwrap(); }
        assert_eq!(s.len(), 5_000);
        for i in 0..5_000 {
            assert_eq!(s.get(h(i)).unwrap().as_ref(), Some(&node(i, "p")), "node {i}");
            assert!(s.contains(h(i)).unwrap());
        }
        assert_eq!(s.get(h(99_999)).unwrap(), None);
        assert!(!s.contains(h(99_999)).unwrap());
    }

    /// Geometry is present only on nodes that have it, so a node with it must not
    /// disturb the ones around it — the flag is what says where the slug starts.
    #[test]
    fn geometry_rides_with_the_node_that_has_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..1_000 {
            if i % 3 == 0 { s.put(h(i), &geo_node(i, "p")).unwrap() }
            else { s.put(h(i), &node(i, "p")).unwrap() }
        }
        for i in 0..1_000 {
            let got = s.get(h(i)).unwrap().expect("node vanished");
            if i % 3 == 0 {
                assert_eq!(got, geo_node(i, "p"), "node {i} lost its geometry");
            } else {
                assert_eq!(got, node(i, "p"), "node {i} gained geometry it never had");
                assert!(got.spatial.is_none());
            }
            assert_eq!(got.slug, format!("p/n{i}"), "node {i} slug shifted");
        }
    }

    /// A slug is arbitrary user text and runs to the end of the record, so anything
    /// that changes its length has to still come back exactly.
    #[test]
    fn awkward_slugs_survive() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let slugs = [
            "p/",                       // empty key
            "p/a",
            "p/ünïcødé-ключ-鍵",         // multi-byte
            "p/with spaces and 'quotes'",
            &format!("p/{}", "x".repeat(3_000)),   // longer than a page's free space
            &format!("p/{}", "y".repeat(60_000)),  // spans several pages
        ];
        for (i, slug) in slugs.iter().enumerate() {
            s.put(h(i as u64), &StoredNode { slug: slug.to_string(), ..node(i as u64, "p") })
                .unwrap();
        }
        for (i, slug) in slugs.iter().enumerate() {
            assert_eq!(s.get(h(i as u64)).unwrap().unwrap().slug, *slug, "slug {i}");
        }
    }

    /// Membership is the question `SELECT ... FROM p` asks, and it has to stay
    /// right through a node changing collection, which is the case a separate
    /// per-collection list gets wrong by leaving the node in both.
    #[test]
    fn collection_membership_tracks_the_nodes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..900 {
            let c = ["p", "q", "r"][(i % 3) as usize];
            s.put(h(i), &node(i, c)).unwrap();
        }
        for c in ["p", "q", "r"] {
            assert_eq!(s.members(coll(c)).unwrap().len(), 300, "collection {c}");
        }
        assert!(s.members(coll("absent")).unwrap().is_empty());

        // Move every third node of p into q.
        let moved: Vec<u64> = (0..900).filter(|i| i % 9 == 0).collect();
        for &i in &moved { s.put(h(i), &node(i, "q")).unwrap(); }
        assert_eq!(s.members(coll("p")).unwrap().len(), 300 - moved.len());
        assert_eq!(s.members(coll("q")).unwrap().len(), 300 + moved.len());
        let p: HashSet<u64> = s.members(coll("p")).unwrap().into_iter().collect();
        for &i in &moved {
            assert!(!p.contains(&h(i)), "node {i} is still a member of the collection it left");
        }

        // And deleting takes the node out of membership as well as out of the store.
        for i in (0..900).step_by(2) { assert!(s.delete(h(i)).unwrap()); }
        let total: usize = ["p", "q", "r"].iter().map(|c| s.members(coll(c)).unwrap().len()).sum();
        assert_eq!(total as u64, s.len(), "membership and the store disagree on how many nodes exist");
    }

    #[test]
    fn a_mixed_workload_matches_an_oracle() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let mut oracle: HashMap<u64, StoredNode> = HashMap::new();

        for i in 0..6_000u64 {
            let key = h(i % 1_500);
            let c = ["p", "q", "r"][(i % 3) as usize];
            let n = if i % 7 == 0 { geo_node(i, c) } else { node(i, c) };
            s.put(key, &n).unwrap();
            oracle.insert(key, n);

            if i % 4 == 0 {
                let victim = h((i * 11) % 1_500);
                assert_eq!(s.delete(victim).unwrap(), oracle.remove(&victim).is_some(),
                           "delete disagreed at step {i}");
            }
        }

        assert_eq!(s.len() as usize, oracle.len(), "node count drifted");
        for (hash, want) in &oracle {
            assert_eq!(s.get(*hash).unwrap().as_ref(), Some(want), "node {hash:x}");
        }
        // Every collection's membership must be exactly the oracle's.
        for c in ["p", "q", "r"] {
            let got: HashSet<u64> = s.members(coll(c)).unwrap().into_iter().collect();
            let want: HashSet<u64> = oracle.iter()
                .filter(|(_, n)| n.collection == coll(c))
                .map(|(h, _)| *h)
                .collect();
            assert_eq!(got, want, "collection {c} membership disagreed");
        }
        // And a full walk must find every node exactly once.
        let mut seen = Vec::new();
        s.for_each_hash(|h| { seen.push(h); true }).unwrap();
        assert_eq!(seen.len(), oracle.len(), "the walk found a different number of nodes");
        assert_eq!(seen.iter().copied().collect::<HashSet<_>>(),
                   oracle.keys().copied().collect::<HashSet<_>>());
    }

    #[test]
    fn nodes_survive_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut s = store(&dir);
            for i in 0..3_000 { s.put(h(i), &node(i, if i % 2 == 0 { "p" } else { "q" })).unwrap(); }
            s.put(h(11), &geo_node(11, "q")).unwrap();
            s.delete(h(5)).unwrap();
            s.sync().unwrap();
        }
        let s = store(&dir);
        assert_eq!(s.len(), 2_999, "node count did not survive");
        assert_eq!(s.get(h(7)).unwrap().unwrap().slug, "q/n7");
        assert_eq!(s.get(h(11)).unwrap().unwrap().spatial, geo_node(11, "q").spatial);
        assert_eq!(s.get(h(5)).unwrap(), None, "a deleted node came back");
        assert_eq!(s.members(coll("p")).unwrap().len() + s.members(coll("q")).unwrap().len(),
                   2_999, "membership did not survive");
    }

    /// A record that is too short, or claims geometry it does not carry, must be
    /// refused rather than read past. These bytes come off disk.
    #[test]
    fn a_damaged_record_is_refused_rather_than_read_past() {
        let mut full = Vec::new();
        encode(&geo_node(1, "p"), &mut full);
        assert!(decode(&full).is_some());
        for cut in 0..full.len() {
            let got = decode(&full[..cut]);
            if cut < HEADER + SPATIAL_BYTES {
                assert!(got.is_none(), "a record cut to {cut} bytes decoded anyway");
            } else {
                // Past the geometry only the slug is truncated, which is damage the
                // decoder cannot detect — but it must still not read out of bounds.
                assert!(got.is_some_and(|n| n.slug.len() <= 5), "cut {cut}");
            }
        }
        // A slug that is not valid UTF-8 is damage, not a node.
        let mut bad = Vec::new();
        encode(&node(1, "p"), &mut bad);
        let last = bad.len() - 1;
        bad[last] = 0xFF;
        assert!(decode(&bad).is_none(), "a record with an unreadable slug was accepted");
    }

    /// **The measurement the direction exists for.**
    ///
    /// `nodes.bin`, `idx.bin` and `slugs.bin` cannot take a new node at all — a
    /// dense id is a position, so one insert renumbers the rest, and writes pile up
    /// in RAM until a rebuild folds them back at a cost set by the size of the
    /// store. Here a node touches its own record and its own index entries.
    #[test]
    fn writing_a_node_costs_the_same_at_every_size() {
        let batch = 5_000u64;
        let mut timings = Vec::new();

        for &preload in &[20_000u64, 100_000, 400_000] {
            let dir = tempfile::TempDir::new().unwrap();
            let mut s = store(&dir);
            for i in 0..preload { s.put(h(i), &node(i, "p")).unwrap(); }

            let t = Instant::now();
            for i in preload..preload + batch { s.put(h(i), &node(i, "p")).unwrap(); }
            timings.push((preload, t.elapsed().as_secs_f64() * 1e6 / batch as f64));
        }

        for (preload, us) in &timings {
            println!("  {preload:>7} nodes already stored → {us:.2} us per node written");
        }
        let (smallest, largest) = (timings[0].1, timings[timings.len() - 1].1);
        assert!(
            largest < smallest * 3.0,
            "a node costs {largest:.2} us on a 400k-node store against {smallest:.2} us \
             on a 20k one — the cost is tracking the size of the store rather than the \
             size of the change, which is the failure this design exists to remove",
        );
    }

    /// What the four files cost per node, against what this costs.
    ///
    /// `nodes.bin` spends 32 bytes and `slugs.bin` about 16, so the record should
    /// come out slightly ahead — it drops the offsets array and stores geometry
    /// only for nodes that have it. The indexes are where it pays: `idx.bin` is a
    /// packed 16 bytes per node and a B+tree is 24 at about 57% occupancy.
    #[test]
    fn the_disk_cost_against_the_files_it_replaces() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let n = 50_000u64;
        for i in 0..n { s.put(h(i), &node(i, ["p", "q", "r"][(i % 3) as usize])).unwrap(); }
        s.sync().unwrap();

        let (rec, idx, coll_idx) = s.page_counts();
        let per = |pages: u64| (pages * DEFAULT_PAGE_SIZE as u64) as f64 / n as f64;
        let total = per(rec) + per(idx) + per(coll_idx);
        // nodes.bin 32 + slugs.bin ~16.4 + idx.bin 16 + collections.bin ~1.25,
        // as measured in examples/topo_bytes.rs.
        let files = 32.0 + 16.4 + 16.0 + 1.25;

        println!("  {n} nodes");
        println!("    records        {:>6.2} bytes/node", per(rec));
        println!("    hash index     {:>6.2} bytes/node", per(idx));
        println!("    collections    {:>6.2} bytes/node", per(coll_idx));
        println!("    total          {:>6.2} against {files:.2} for the four files it \
                  replaces → {:.2}x", total, total / files);
        assert!(total / files < 2.0,
                "{total:.2} bytes a node against the files' {files:.2} is {:.2}x, which \
                 is more than the design was accepted on",
                total / files);
    }
}
