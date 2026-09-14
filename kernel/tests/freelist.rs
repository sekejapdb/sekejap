//! 2n: page recycling. Step A -- every page that reaches disk carries the
//! generation of the epoch that wrote it (the formerly reserved lsn field),
//! the ordering signal recovery needs once page numbers are reused.

use kernel::graph::Graph;
use kernel::store::{Config, Store};

const PAGE_SIZE: usize = 4096;

/// Read (page_no -> stamped generation) for every page that verifies as
/// non-free, straight off the file -- no engine code in the read path.
fn stamps(dir: &std::path::Path) -> std::collections::BTreeMap<u32, u64> {
    let data = std::fs::read(dir.join("data")).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for (i, page) in data.chunks_exact(PAGE_SIZE).enumerate() {
        let magic = u32::from_le_bytes(page[0..4].try_into().unwrap());
        let kind = u16::from_le_bytes(page[6..8].try_into().unwrap());
        if magic != 0x53454B32 || kind == 0 { continue; } // not a page / Free
        let no = u32::from_le_bytes(page[12..16].try_into().unwrap());
        if no as usize != i { continue; }
        let gen = u64::from_le_bytes(page[24..32].try_into().unwrap());
        out.insert(no, gen);
    }
    out
}

#[test]
fn every_flushed_page_carries_its_publishing_generation() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
    for i in 1..=500u64 {
        g.add_node(None, 7, format!("railway ledger row {i}").as_bytes()).unwrap();
    }
    g.commit().unwrap();
    g.checkpoint().unwrap();
    let first = stamps(d.path());
    assert!(!first.is_empty());
    let gens: std::collections::HashSet<u64> = first.values().copied().collect();
    assert_eq!(gens.len(), 1, "one epoch published: one generation everywhere, got {gens:?}");
    let g1 = *gens.iter().next().unwrap();
    assert!(g1 > 0, "stamp must be a real generation, not the reserved 0");

    // second epoch: touched pages advance, untouched pages keep their stamp
    for i in 501..=550u64 {
        g.add_node(None, 7, format!("second epoch row {i}").as_bytes()).unwrap();
    }
    g.commit().unwrap();
    g.checkpoint().unwrap();
    let second = stamps(d.path());
    let g2max = *second.values().max().unwrap();
    assert!(g2max > g1, "the second epoch must stamp a higher generation");
    assert!(second.values().any(|&v| v == g1),
            "untouched pages keep the first epoch's stamp");
}

/// Step B: recovery's duplicate-key winner is (generation, page_no), not
/// page number alone. Forge the exact shape recycling produces -- a STALE
/// copy of a key surviving on a HIGHER page number than the current copy --
/// and recover must keep the higher-generation original.
#[test]
fn recovery_prefers_higher_generation_over_higher_page_number() {
    use kernel::store::{Config, Store};
    const PS: usize = 4096;
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut s = Store::create(d.path(), Config::default()).unwrap();
        for i in 0..2_000u64 {
            s.put(&i.to_be_bytes(), b"current-generation-v").unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }
    let path = d.path().join("data");
    let mut bytes = std::fs::read(&path).unwrap();
    // find a verified leaf and decode its first record's key + value offset
    let mut found: Option<(usize, Vec<u8>, usize, usize)> = None; // (page, key, val_off_in_page, val_len)
    for (i, page) in bytes.chunks_exact(PS).enumerate() {
        let magic = u32::from_le_bytes(page[0..4].try_into().unwrap());
        let kind = u16::from_le_bytes(page[6..8].try_into().unwrap());
        let no = u32::from_le_bytes(page[12..16].try_into().unwrap());
        let nentries = u16::from_le_bytes(page[10..12].try_into().unwrap());
        let tid = u16::from_le_bytes(page[8..10].try_into().unwrap());
        if magic != 0x53454B32 || kind != 2 || tid != 1 || no as usize != i || nentries < 10 { continue; }
        let off = u16::from_le_bytes(page[40..42].try_into().unwrap()) as usize;
        let raw = u16::from_le_bytes(page[off..off + 2].try_into().unwrap()) as usize;
        let compact = cfg!(feature = "compact-cells") && raw & 0xf000 == 0x4000;
        let klen = if compact { raw & 0x0fff } else { raw };
        let key = page[off + 2..off + 2 + klen].to_vec();
        let (value_at, vlen) = if compact {
            let slot_len = u16::from_le_bytes(page[42..44].try_into().unwrap()) as usize;
            (off + 2 + klen, slot_len - 2 - klen)
        } else {
            (off + 4 + klen, u16::from_le_bytes(page[off + 2 + klen..off + 4 + klen].try_into().unwrap()) as usize)
        };
        if vlen != 20 { continue; } // want a plain value record, not a marker
        found = Some((i, key, value_at, vlen));
        break;
    }
    let (low, key, val_off, val_len) = found.expect("a decodable leaf");
    // duplicate that leaf at the END of the file (higher page number), with
    // the target key's value REWRITTEN and an OLDER generation stamp
    let end_no = bytes.len() / PS;
    let mut dup = bytes[low * PS..(low + 1) * PS].to_vec();
    dup[12..16].copy_from_slice(&(end_no as u32).to_le_bytes());
    dup[val_off..val_off + val_len].copy_from_slice(b"stale-generation-vXX");
    let orig_gen = u64::from_le_bytes(dup[24..32].try_into().unwrap());
    assert!(orig_gen >= 1, "pages must carry a stamp by now");
    kernel::page::seal(&mut dup, orig_gen.saturating_sub(1));
    bytes.extend_from_slice(&dup);
    std::fs::write(&path, &bytes).unwrap();

    kernel::recover::recover(d.path(), Config::default()).unwrap();
    let s = Store::open(d.path(), Config::default()).unwrap();
    let v = s.get(&key).unwrap().expect("the key must survive recovery");
    assert_eq!(&v, b"current-generation-v",
        "the higher-generation copy must win over a higher page number");
}

/// The bug 2n's probe unearthed (present since 2h): delete_prefix's
/// whole-leaf clear re-initialised the page header, zeroing next_leaf; a
/// MIDDLE leaf then looked rightmost to the append fast path, later
/// inserts wrote past their range, and committed subtrees went silently
/// unreachable -- a fold+insert+fold cycle lost every folded segment.
/// This drives three full cycles and counts every row it is owed.
#[test]
fn fold_insert_fold_cycles_lose_nothing() {
    use kernel::graph::Graph;
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
    let count = |g: &Graph| -> usize {
        let mut n = 0;
        let it = g.store_ref().scan(&[0x0C]).unwrap();
        it.for_each_ref(|k, _| {
            if k.first() == Some(&0x0C) { n += 1; true } else { false }
        }).unwrap();
        n
    };
    let mut expected_seg_rows = 0usize;
    for round in 1..=3u64 {
        for i in 0..100u64 {
            let id = round * 1000 + i;
            g.index_text(1, id, &format!("railway survey number {id}")).unwrap();
        }
        g.commit().unwrap();
        g.checkpoint().unwrap();
        g.fold_text(1).unwrap();
        g.checkpoint().unwrap();
        // each fold adds one segment: 100 unique number-terms + railway +
        // survey + "number" -- and every earlier segment must still be there
        expected_seg_rows += 103;
        assert_eq!(count(&g), expected_seg_rows,
                   "round {round}: all folded segments must survive");
        // and the data actually answers: a term folded in ROUND 1
        let hits = g.text_search(1, "1000", 5).unwrap();
        assert!(!hits.is_empty(), "round {round}: round-1 data must stay searchable");
    }
}

/// THE 2n gate, Law-shaped: constant live data => constant file. Rounds of
/// overwriting the SAME keys churn shadows through the freelist; after the
/// pipeline fills, the file must stop growing entirely.
#[test]
fn overwrite_churn_plateaus_the_file() {
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();
    let mut s = Store::create(d.path(), Config::default()).unwrap();
    let mut size_at = Vec::new();
    for round in 0..6u64 {
        for i in 0..2_000u64 {
            s.put(format!("churn-{i:05}").as_bytes(),
                  format!("round {round} value of {i}").as_bytes()).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
        size_at.push(std::fs::metadata(d.path().join("data")).unwrap().len());
    }
    assert_eq!(size_at[2], size_at[5],
        "constant live data must mean a constant file: {size_at:?}");
}

/// The steady-state fail-closed gate: an already registered reader whose
/// durable marker is corrupt must pin every old generation. This deliberately
/// opens the snapshot before any churn; the separate concurrent store test
/// covers registration racing metadata selection.
#[test]
fn a_corrupt_registered_reader_marker_halts_recycling() {
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();
    let mut s = Store::create(d.path(), Config::default()).unwrap();
    for i in 0..2_000u64 {
        s.put(format!("base-{i:05}").as_bytes(), format!("stable value {i}").as_bytes()).unwrap();
    }
    s.commit().unwrap();
    s.checkpoint().unwrap();
    // a STARVED reader pool: every read past ~16 frames re-reads the disk,
    // so recycled pages cannot hide behind the reader's own cache (the
    // first form of this test cached the whole tree and could not fail).
    let tiny = Config { budget_bytes: 1 << 16, ..Config::default() };
    let reader = Store::open_snapshot(d.path(), tiny).unwrap();
    // Corrupt the live reader's durable generation marker while its lock is
    // still held. The writer must interpret the ambiguity as generation zero,
    // so every old page remains pinned despite the churn below.
    let marker = std::fs::read_dir(d.path().join("readers"))
        .unwrap().next().unwrap().unwrap().path();
    let mut marker_bytes = std::fs::read(&marker).unwrap();
    let last = marker_bytes.len() - 1;
    marker_bytes[last] ^= 0x80;
    std::fs::write(&marker, marker_bytes).unwrap();
    let before: Vec<_> = (0..2_000u64)
        .map(|i| reader.get(format!("base-{i:05}").as_bytes()).unwrap()).collect();
    // churn far past the reader: many epochs, heavy shadow traffic
    for round in 0..8u64 {
        for i in 0..2_000u64 {
            s.put(format!("base-{i:05}").as_bytes(),
                  format!("round {round} rewrite {i}").as_bytes()).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }
    let after: Vec<_> = (0..2_000u64)
        .map(|i| reader.get(format!("base-{i:05}").as_bytes()).unwrap()).collect();
    assert_eq!(before, after, "a pinned reader's world moved");
    drop(reader);
}

/// Step D: the freelist survives a clean reopen -- pages freed before the
/// restart are still recyclable after it (the sidecar file), and a deleted
/// sidecar only leaks (fresh empty list), never corrupts.
#[test]
fn the_freelist_survives_reopen() {
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut s = Store::create(d.path(), Config::default()).unwrap();
        for round in 0..3u64 {
            for i in 0..2_000u64 {
                s.put(format!("churn-{i:05}").as_bytes(),
                      format!("round {round} value {i}").as_bytes()).unwrap();
            }
            s.commit().unwrap();
            s.checkpoint().unwrap();
        }
        assert!(s.pool_ref().free_pages_pending() > 10, "churn must have freed pages");
    }
    let s = Store::open(d.path(), Config::default()).unwrap();
    assert!(s.pool_ref().free_pages_pending() > 10,
            "the persisted freelist must survive a reopen");
}

/// The leak-only posture, exercised: a corrupt (or truncated) freelist
/// sidecar must neither refuse the open nor poison anything -- it just
/// means no recycling until new frees accrue.
#[test]
fn a_corrupt_freelist_sidecar_only_leaks() {
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut s = Store::create(d.path(), Config::default()).unwrap();
        for i in 0..1_000u64 {
            s.put(format!("row-{i:05}").as_bytes(), b"value bytes here").unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }
    std::fs::write(d.path().join("free"), b"not a freelist at all").unwrap();
    let s = Store::open(d.path(), Config::default()).unwrap();
    assert_eq!(s.pool_ref().free_pages_pending(), 0, "corrupt sidecar: empty list");
    assert_eq!(s.get(b"row-00500").unwrap().as_deref(), Some(&b"value bytes here"[..]),
               "data untouched by sidecar corruption");
}

/// F5a: generation 2's reusable-page list still names pages generation 3
/// recycles into its published tree. Restoring that older list beside the
/// generation-3 data file is the exact disk image left by a crash after the
/// data publication and before the sidecar replacement. Reopen must reject
/// the mismatched list, not hand its page numbers out again.
#[test]
fn a_crash_after_data_publish_cannot_reimport_the_previous_freelist() {
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();
    let cfg = Config::default();
    let stale = {
        let mut s = Store::create(d.path(), cfg).unwrap();
        for round in 0..2u64 {
            for i in 0..2_000u64 {
                s.put(format!("row-{i:05}").as_bytes(),
                      format!("round {round} value of row {i}").as_bytes()).unwrap();
            }
            s.commit().unwrap();
            s.checkpoint().unwrap();
        }
        assert!(s.pool_ref().free_pages_pending() > 0,
                "generation 2 must persist pages generation 3 can recycle");
        let stale = std::fs::read(d.path().join("free")).unwrap();

        for i in 0..2_000u64 {
            s.put(format!("row-{i:05}").as_bytes(),
                  format!("published generation 3 row {i}").as_bytes()).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
        stale
    };

    // The crash loses only the sidecar replacement, not the already durable
    // data/meta publication.
    std::fs::write(d.path().join("free"), stale).unwrap();
    let mut reopened = Store::open(d.path(), cfg).unwrap();
    assert_eq!(reopened.pool_ref().free_pages_pending(), 0,
               "a sidecar from another published generation must be discarded");

    for i in 0..2_000u64 {
        reopened.put(format!("after-{i:05}").as_bytes(), b"allocate after crash").unwrap();
    }
    reopened.commit().unwrap();
    reopened.checkpoint().unwrap();
    for i in 0..2_000u64 {
        assert_eq!(reopened.get(format!("row-{i:05}").as_bytes()).unwrap().as_deref(),
                   Some(format!("published generation 3 row {i}").as_bytes()),
                   "post-crash allocation overwrote committed row {i}");
    }
}

/// DEFECT GATE: recovery rebuilds and RENUMBERS the data file but leaves the
/// old freelist sidecar in place. The sidecar names pages of a file that no
/// longer exists in that shape — including, possibly, the page the rebuilt
/// tree now uses as its root. Once the reuse horizon advances, an allocation
/// hands out a page that is live.
///
/// The property, stated without reference to the mechanism: after a recovery,
/// every row that was durable before it must still be readable after further
/// writes have had the chance to recycle pages.
#[test]
fn recovery_does_not_leave_a_freelist_that_claims_live_pages() {
    use kernel::store::{Config, Store};
    let d = tempfile::TempDir::new().unwrap();

    // churn first, so the sidecar has real entries in it
    {
        let mut s = Store::create(d.path(), Config::default()).unwrap();
        for round in 0..4u64 {
            for i in 0..2_000u64 {
                s.put(format!("row-{i:05}").as_bytes(),
                      format!("round {round} value of row {i}").as_bytes()).unwrap();
            }
            s.commit().unwrap();
            s.checkpoint().unwrap();
        }
        assert!(s.pool_ref().free_pages_pending() > 0, "the churn must have freed pages");
    }
    assert!(d.path().join("free").exists(), "and persisted them");

    let old_freelist = std::fs::read(d.path().join("free")).unwrap();

    // Recover rebuilds and renumbers the file. Its reusable-page list must be
    // rebuilt empty before that new numbering becomes authoritative.
    kernel::recover::recover(d.path(), Config::default()).unwrap();
    let rebuilt_freelist = std::fs::read(d.path().join("free")).unwrap();
    assert_ne!(rebuilt_freelist, old_freelist,
               "recovery must not leave the old file's page-number list beside the rebuild");

    let after_recovery = Store::open(d.path(), Config::default()).unwrap();
    assert_eq!(after_recovery.pool_ref().free_pages_pending(), 0,
               "a rebuilt file starts with no reusable pages");
    drop(after_recovery);

    // Also recreate the exact crash image that an unsafe retire-after-publish
    // order would leave: rebuilt data plus the old sidecar. Its generation
    // mismatch must independently make it unusable.
    std::fs::write(d.path().join("free"), old_freelist).unwrap();

    // now write enough to consume any freelist the recovery wrongly trusted
    let mut s = Store::open(d.path(), Config::default()).unwrap();
    assert_eq!(s.pool_ref().free_pages_pending(), 0,
               "a pre-recovery sidecar must never survive renumbering logically or physically");
    for round in 0..4u64 {
        for i in 0..2_000u64 {
            s.put(format!("after-{i:05}").as_bytes(),
                  format!("post-recovery round {round} row {i}").as_bytes()).unwrap();
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }

    // every pre-recovery row must still read correctly
    for i in 0..2_000u64 {
        let got = s.get(format!("row-{i:05}").as_bytes()).unwrap();
        assert_eq!(got.as_deref(), Some(format!("round 3 value of row {i}").as_bytes()),
                   "row {i} was lost or overwritten after recovery recycled pages");
    }
}
