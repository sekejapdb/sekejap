//! Crash and damage behaviour of the paged storage configuration.
//!
//! The paged stores are a different durability story from the append-only ones,
//! and none of the existing crash tests touched them — `crash_recovery.rs`,
//! `persistence.rs` and `stress.rs` do not mention a single paged flag. That gap
//! is what this file closes, because "is it safe to make paged the default" is a
//! question about torn writes and kills, not about throughput.
//!
//! # What is different about these stores
//!
//! A page store keeps its header — high water mark, free-list head — in page 0,
//! and writes it only on `sync`. That is deliberate: a page write per allocation
//! would defeat the point. The claimed consequence is that a crash loses *the free
//! list*, not data: pages freed since the last sync stay allocated and are never
//! reused, which leaks space but cannot corrupt, because a leaked page is one
//! nothing points at.
//!
//! That claim is exactly the kind that is true in the design and false in the
//! code. These tests try to falsify it.
//!
//! # The standard every case is held to
//!
//! Damage may cost data that was not yet durable. It may **never**:
//!
//! - panic, or read outside a buffer
//! - report success while serving less than it holds
//! - turn a row into a *different* row
//!
//! A store that cannot be read must say so. A store that can be read must be right.

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::path::Path;

fn paged() -> Config {
    Config {
        paged_topology: true,
        paged_payloads: true,
        paged_adjacency: true,
        paged_nodes: true,
        ..Config::default()
    }
}

fn row(i: usize) -> String {
    json!({
        "_collection": "p",
        "_key": format!("n{i}"),
        "n": i as i64,
        "body": format!("record {i} on the lazy riverbank"),
    })
    .to_string()
}

/// Build a paged store with `n` rows and `n/2` edges, compacted.
fn build(dir: &Path, n: usize) {
    let mut db = CoreDB::open_with_config(dir, paged()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
    for i in 0..n {
        db.put(&format!("p/n{i}"), &row(i)).unwrap();
    }
    for i in 0..n / 2 {
        db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
    }
    db.compact().unwrap();
}

fn rows(db: &CoreDB) -> usize {
    db.query("SELECT _key FROM p").unwrap().collect().len()
}

/// Every paged file, so a test can damage each in turn without naming them by hand
/// — a file added later is then covered automatically rather than silently missed.
fn paged_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            n.starts_with("nodesp") || n.starts_with("adjp") || n == "payloads.bin"
        })
        .collect();
    v.sort();
    v
}

// ── the ordinary paths ───────────────────────────────────────────────────────

/// Writes that were never compacted must come back from the WAL.
#[test]
fn uncompacted_writes_replay_into_the_paged_stores() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 60);
    {
        // No compact, no graceful anything: this is a process death.
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 60..90 {
            db.put(&format!("p/n{i}"), &row(i)).unwrap();
        }
        db.remove("p/n5");
        db.link("p/n61", "p/n62", "next");
        drop(db);
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 89, "WAL replay lost writes that were never compacted");
    assert!(db.get("p/n80").is_some(), "a replayed row is missing");
    assert!(db.get("p/n5").is_none(), "a replayed delete came back");
    assert_eq!(db.one("p/n61").forward("next").collect().len(), 1,
               "a replayed edge is missing");
}

/// The same, but the writes were folded into the paged stores by a compaction and
/// the log truncated — so recovery reads them out of the pages, not the log.
#[test]
fn compacted_writes_survive_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 200);
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 200..260 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        db.execute("DELETE FROM p WHERE n < 10").unwrap();
        db.compact().unwrap();
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 250, "a compacted paged store did not reopen intact");
    assert!(db.get("p/n255").is_some());
    assert!(db.get("p/n3").is_none(), "a deleted row survived the fold");
    assert_eq!(db.one("p/n40").forward("next").collect().len(), 1,
               "edges did not survive the fold");
}

/// Repeated crash-and-reopen, each round adding writes without a graceful close.
/// A leak that only shows up after several rounds shows up here.
#[test]
fn many_crash_cycles_stay_consistent() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);
    let mut expected = 40usize;
    for round in 0..8 {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        let base = 100 + round * 20;
        for i in base..base + 20 {
            db.put(&format!("p/n{i}"), &row(i)).unwrap();
        }
        expected += 20;
        // Compact on alternate rounds; the others die with a dirty log.
        if round % 2 == 1 {
            db.compact().unwrap();
        }
        assert_eq!(rows(&db), expected, "round {round}: live count wrong before the crash");
        drop(db);

        let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        assert_eq!(rows(&db), expected, "round {round}: writes lost across a crash");
        assert!(db.get(&format!("p/n{}", base + 5)).is_some(),
                "round {round}: a row from this round is missing");
    }
}

// ── damage ───────────────────────────────────────────────────────────────────

/// Truncating any paged file, at any point, must not panic and must not invent
/// data. Losing rows is allowed — the file was cut in half — but a store that
/// answers at all has to answer truthfully.
#[test]
fn a_truncated_paged_file_never_panics_or_invents() {
    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 120);
    let files = paged_files(dir0.path());
    drop(dir0);

    for file in files {
        for fraction in [0usize, 1, 3, 7] {
            let dir = tempfile::TempDir::new().unwrap();
            build(dir.path(), 120);
            let path = dir.path().join(&file);
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let len = meta.len();
            if len == 0 { continue }
            // 0 = empty, then progressively more of the file kept.
            let keep = len * fraction as u64 / 8;
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(keep).unwrap();
            drop(f);

            // Opening may fail — that is a legitimate answer to a destroyed file.
            let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else { continue };
            let got = rows(&db);
            assert!(got <= 120,
                    "{file} cut to {keep}/{len} bytes reported {got} rows, more than the \
                     120 the database ever held — damage invented data");
            // Whatever it does return has to be real rows, readable and correct.
            for hit in db.query("SELECT _key FROM p").unwrap().collect() {
                let Some(raw) = db.get(&hit.slug) else { continue };
                let v: serde_json::Value = serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{file}@{keep}: unparseable payload: {e}"));
                let key = v["_key"].as_str().unwrap_or("");
                assert!(hit.slug.ends_with(key),
                        "{file} cut to {keep} bytes: index says {} but the record says {key} \
                         — damage turned one row into another", hit.slug);
            }
        }
    }
}

/// Garbage written over a paged file must be declined, not misread.
#[test]
fn a_paged_file_full_of_garbage_is_declined() {
    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 60);
    let files = paged_files(dir0.path());
    drop(dir0);

    for file in files {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 60);
        let path = dir.path().join(&file);
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        std::fs::write(&path, vec![0xA5u8; meta.len() as usize]).unwrap();

        match CoreDB::open_with_config(dir.path(), paged()) {
            Err(_) => {} // refusing is correct
            Ok(db) => {
                let got = rows(&db);
                assert!(got <= 60,
                        "{file} filled with garbage produced {got} rows out of a 60-row store");
                for hit in db.query("SELECT _key FROM p").unwrap().collect() {
                    if let Some(raw) = db.get(&hit.slug) {
                        let v: serde_json::Value = serde_json::from_str(&raw)
                            .unwrap_or_else(|e| panic!("{file}: garbage became a payload: {e}"));
                        let key = v["_key"].as_str().unwrap_or("");
                        assert!(hit.slug.ends_with(key),
                                "{file}: garbage turned {} into {key}", hit.slug);
                    }
                }
            }
        }
    }
}

/// **The free-list claim, tested.** The page-store header is written on sync, so a
/// crash between a free and a sync leaves the header pointing at an older free
/// list. The design says that leaks pages and cannot corrupt. This reverts the
/// header to a previous copy and checks that the store still reads correctly.
#[test]
fn a_stale_page_header_leaks_space_but_never_data() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 150);

    // Capture the header of each record file as it stood, then churn hard enough to
    // free and reuse pages, then put the old header back — the state a crash
    // between a free and the next sync produces.
    let files: Vec<String> = paged_files(dir.path())
        .into_iter()
        .filter(|f| f.ends_with(".rec") || f == "payloads.bin")
        .collect();

    let mut headers: Vec<(String, Vec<u8>)> = Vec::new();
    for f in &files {
        let path = dir.path().join(f);
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() >= 64 { headers.push((f.clone(), bytes[..64].to_vec())) }
        }
    }
    assert!(!headers.is_empty(), "no paged record files to test");

    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 0..150 { db.put(&format!("p/n{i}"), &row(i + 10_000)).unwrap(); }
        db.execute("DELETE FROM p WHERE n >= 10100").unwrap();
        db.compact().unwrap();
    }

    for (f, header) in &headers {
        use std::io::{Seek, SeekFrom, Write};
        let mut fh = std::fs::OpenOptions::new().write(true).open(dir.path().join(f)).unwrap();
        fh.seek(SeekFrom::Start(0)).unwrap();
        fh.write_all(header).unwrap();
    }

    // Every row the store still claims to have must be readable and be itself.
    let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else {
        return; // declining a store with a rewound header is also a valid answer
    };
    for hit in db.query("SELECT _key FROM p").unwrap().collect() {
        let Some(raw) = db.get(&hit.slug) else { continue };
        let v: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("stale header in {files:?}: unparseable payload: {e}"));
        let key = v["_key"].as_str().unwrap_or("");
        assert!(hit.slug.ends_with(key),
                "a rewound page header turned {} into {key}", hit.slug);
    }
}

/// Stale `.tmp` files a process killed mid-compaction could leave, including the
/// paged ones. Reopening must ignore them entirely.
#[test]
fn stale_temp_files_do_not_affect_a_paged_reopen() {
    for leftover in [
        "payloads.bin.tmp", "snapshot.json.tmp", "nodes.bin.tmp",
        "nodesp.rec.tmp", "nodesp.idx.tmp", "adjp_fwd.rec.tmp", "edge_types.tmp",
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 50);
        std::fs::write(dir.path().join(leftover), b"PARTIAL GARBAGE \x00\x01\x02").unwrap();

        let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        assert_eq!(rows(&db), 50, "{leftover}: a stale temp file changed what was served");
        assert!(db.get("p/n7").is_some(), "{leftover}: payloads unreadable after a stale temp");
        drop(db);

        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.compact().unwrap();
        assert_eq!(rows(&db), 50, "{leftover}: compaction after a crash lost rows");
    }
}

/// A crash during the fold — the paged stores have taken some of the overlay but
/// the compaction never finished — must leave a store that reopens with everything,
/// because the log was not truncated.
#[test]
fn a_crash_during_a_fold_loses_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 100);
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        for i in 100..200 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        // Drop mid-flight: the writes are in the WAL and the overlay, and the fold
        // that would have made them durable in the pages never ran.
        drop(db);
    }
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 200, "writes were lost when the fold never ran");
    for i in [0usize, 99, 100, 199] {
        assert!(db.get(&format!("p/n{i}")).is_some(), "row {i} missing after an interrupted fold");
    }
}

/// The two halves of a damaged store must not *contradict* each other.
///
/// The first version of this required the index and the records to agree exactly,
/// and that standard is wrong. They live in different files. Truncating one and
/// not the other makes them disagree by definition — the key really is still in
/// the index, and the record really is gone, and both statements are true. Any
/// store built from more than one file has this property; demanding otherwise
/// would be demanding that partial damage be impossible.
///
/// What must hold is weaker and actually meaningful:
///
/// - nothing readable exceeds what is listed (no rows conjured from damage)
/// - every row that *can* be read is the row it was indexed as
/// - an edge that survives points at a row that survives, or is dropped
/// - and on an **undamaged** store the two halves agree exactly, so the tolerance
///   above is spent on damage rather than hiding an everyday inconsistency
#[test]
fn a_damaged_store_never_contradicts_itself() {
    // The baseline first: with nothing damaged, listed and readable must match.
    {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 100);
        let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        let listed = db.query("SELECT _key FROM p").unwrap().collect();
        let readable = listed.iter().filter(|h| db.get(&h.slug).is_some()).count();
        assert_eq!(readable, listed.len(),
                   "an undamaged paged store lists {} rows but can read {readable}",
                   listed.len());
        assert_eq!(listed.len(), 100, "an undamaged paged store lost rows");
    }

    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 100);
    let files = paged_files(dir0.path());
    drop(dir0);

    for file in files {
        for keep_eighths in [2usize, 5, 7] {
            let dir = tempfile::TempDir::new().unwrap();
            build(dir.path(), 100);
            let path = dir.path().join(&file);
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.len() == 0 { continue }
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(meta.len() * keep_eighths as u64 / 8).unwrap();
            drop(f);

            let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else { continue };
            let listed = db.query("SELECT _key FROM p").unwrap().collect();
            assert!(listed.len() <= 100,
                    "{file}@{keep_eighths}/8 listed {} rows in a 100-row store",
                    listed.len());
            let mut readable = 0usize;
            for h in &listed {
                let Some(raw) = db.get(&h.slug) else { continue };
                readable += 1;
                let v: serde_json::Value = serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{file}@{keep_eighths}/8: unparseable: {e}"));
                let key = v["_key"].as_str().unwrap_or("");
                assert!(h.slug.ends_with(key),
                        "{file}@{keep_eighths}/8: listed {} but the record is {key} — \
                         damage turned one row into another", h.slug);
            }
            assert!(readable <= listed.len(), "more readable than listed, which is impossible");
            // Every edge that is still reported must point at a row still present.
            for h in listed.iter().take(30) {
                for e in db.edges_from(&h.slug) {
                    if let Some(to) = &e.to_slug {
                        assert!(db.get(to).is_some() || db.get(&h.slug).is_none(),
                                "{file}@{keep_eighths}/8: edge {} -> {to} points at a row \
                                 that is not there", h.slug);
                    }
                }
            }
        }
    }
}

// ── fuzz ─────────────────────────────────────────────────────────────────────

/// Deterministic xorshift, so a failure is reproducible from its seed rather than
/// being a story about a run that once went wrong.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 { if n == 0 { 0 } else { self.next() % n } }
}

/// Random byte damage, everywhere, over and over.
///
/// The hand-written corruptions above test the failures I could think of, which is
/// a bounded and biased set — every bug found so far was in a place I had not
/// thought to look until something made me. This flips bytes at random across
/// every paged file and requires the same thing of all of them: answer or refuse,
/// but do not panic, do not hang, and do not invent.
///
/// Seeded, so any failure reproduces exactly.
#[test]
fn random_byte_damage_never_panics_or_hangs() {
    let dir0 = tempfile::TempDir::new().unwrap();
    build(dir0.path(), 80);
    let files = paged_files(dir0.path());
    drop(dir0);
    assert!(!files.is_empty(), "no paged files to fuzz");

    // The committed run is small enough to keep the suite quick. A campaign is
    // `SK_FUZZ_ROUNDS=5000 SK_FUZZ_SEED=... cargo test --release --test
    // paged_durability random_byte` — same code, more of it, so a campaign that
    // finds something produces a seed the committed test can be pinned to.
    let rounds: u64 = std::env::var("SK_FUZZ_ROUNDS").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(240);
    let seed: u64 = std::env::var("SK_FUZZ_SEED").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(0x5EE_D0_5EE_D);
    let mut rng = Rng(seed);

    for round in 0..rounds {
        let dir = tempfile::TempDir::new().unwrap();
        build(dir.path(), 80);

        // One to three files at a time, a handful of bytes each — enough to break
        // structure, not so much that every case degenerates into "the file is
        // gone". Damaging several at once matters because the stores cross-
        // reference: an index in one file points at a record in another.
        let how_many = 1 + rng.below(3);
        let mut damage = Vec::new();
        for _ in 0..how_many {
            let file = files[rng.below(files.len() as u64) as usize].clone();
            let path = dir.path().join(&file);
            let Ok(mut bytes) = std::fs::read(&path) else { continue };
            if bytes.is_empty() { continue }
            let flips = 1 + rng.below(12);
            let mut where_ = Vec::new();
            for _ in 0..flips {
                let at = rng.below(bytes.len() as u64) as usize;
                let val = (rng.next() & 0xFF) as u8;
                bytes[at] = val;
                where_.push((at, val));
            }
            std::fs::write(&path, &bytes).unwrap();
            damage.push((file, where_));
        }
        if damage.is_empty() { continue }

        let ctx = format!("round {round} (seed {seed}): {damage:?}");
        let round_started = std::time::Instant::now();

        // Opening may fail. That is an answer.
        let Ok(db) = CoreDB::open_with_config(dir.path(), paged()) else { continue };

        // Everything a caller might do, none of which may panic or hang.
        let listed = db.query("SELECT _key FROM p").unwrap_or_default_hits();
        assert!(listed.len() <= 80, "{ctx}: listed {} rows in an 80-row store", listed.len());
        for h in listed.iter().take(40) {
            if let Some(raw) = db.get(&h.slug) {
                // A payload whose own bytes were flipped comes back flipped: records
                // carry no checksum, so the store cannot know. What it must not do
                // is hand back *another row's* payload — a record boundary read
                // wrong is a very different failure from a byte read wrong, and
                // only the first is the store's fault.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    // Distinguishing the two failures that look alike from outside.
                    // A byte flipped *inside* the right record is not the store's
                    // fault — payload records carry no checksum and are not being
                    // asked to. A record **boundary** read wrong is: the store
                    // handed back another row and said it was this one.
                    //
                    // Reading `_key` alone cannot tell them apart, and said so
                    // wrongly: one flipped digit turns `n51` into `n58`, which is
                    // also a real key of an 80-row dataset, so a single-byte flip
                    // was reported as a substitution. That cost a fuzz campaign and
                    // a wrong diagnosis before the payload was printed in full and
                    // showed `{"_key":"n58", "n":51, "_id":"p/n51"}` — n51's record
                    // with one character changed.
                    //
                    // So it takes **two** independent fields to agree that this is
                    // a different row. `_key`, `_id` and `n` are written from the
                    // same source and stored apart; one flip can move one of them.
                    // A boundary error moves all three at once.
                    let key_says = v["_key"].as_str()
                        .and_then(|k| k.strip_prefix('n'))
                        .and_then(|d| d.parse::<usize>().ok())
                        .filter(|n| *n < 80);
                    let id_says = v["_id"].as_str()
                        .and_then(|s| s.strip_prefix("p/n"))
                        .and_then(|d| d.parse::<usize>().ok())
                        .filter(|n| *n < 80);
                    let n_says = v["n"].as_u64().map(|n| n as usize).filter(|n| *n < 80);
                    let asked = h.slug.strip_prefix("p/n")
                        .and_then(|d| d.parse::<usize>().ok());
                    if let Some(asked) = asked {
                        let disagree = [key_says, id_says, n_says]
                            .into_iter()
                            .flatten()
                            .filter(|got| *got != asked)
                            .collect::<Vec<_>>();
                        assert!(disagree.len() < 2,
                                "{ctx}: asked for {} and got a record claiming to be \
                                 {disagree:?} in more than one field — a record \
                                 boundary was read wrong, not a byte", h.slug);
                    }
                }
            }
            // Edge lists are records too, keyed by owner hash and holding anonymous
            // bytes — the same exposure the node records had. The dataset links
            // n{i} -> n{i+1} and nothing else, so an edge from n{i} to anything but
            // n{i+1} is another node's edge list returned as this one's.
            if let Some(i) = h.slug.strip_prefix("p/n").and_then(|d| d.parse::<usize>().ok()) {
                for e in db.edges_from(&h.slug) {
                    let Some(to) = e.to_slug.as_deref() else { continue };
                    let Some(j) = to.strip_prefix("p/n").and_then(|d| d.parse::<usize>().ok())
                    else { continue };
                    assert_eq!(j, i + 1,
                               "{ctx}: {} has an edge to {to}, but this store only ever \
                                linked each node to the next — that is another node's \
                                edge list", h.slug);
                }
            }
            let _ = db.one(&h.slug).forward("next").collect();
        }
        let _ = db.query("SELECT _key FROM p WHERE n > 10 AND n < 40").map(|s| s.collect());
        let _ = db.query("SELECT COUNT(*) AS c FROM p").map(|s| s.collect());
        let _ = db.query("SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)").map(|s| s.collect());
        let _ = db.node_count();
        let _ = db.collection_names();

        // Per round, not for the whole loop.
        //
        // This was one 1800-second budget for the entire run, which is a sensible
        // bound for the committed 240 rounds and a wrong one for a campaign: a
        // 3000-round run legitimately passes it, so it aborted at whatever round
        // happened to cross the line, reported "something is spinning", and stopped
        // the campaign exploring the rounds it had not reached yet. A guard that
        // fires on healthy work is worse than no guard — it makes every long
        // campaign look like a failure and hides whatever the remaining rounds
        // would have found.
        //
        // What it is actually watching for is *one* round that does not terminate:
        // a corrupt page pointing back at itself, a walk with no bound. A healthy
        // round here takes well under a second, so a round that takes half a minute
        // is spinning whatever the campaign length.
        assert!(round_started.elapsed().as_secs() < 30,
                "{ctx}: this round took {}s — a healthy one takes under a second, so \
                 something is spinning rather than answering",
                round_started.elapsed().as_secs());
    }
}

/// Small helper so the fuzz body reads as what it checks rather than as error
/// plumbing: a query that fails is a legitimate answer to a damaged store.
trait OrNoHits {
    fn unwrap_or_default_hits(self) -> Vec<sekejap::Hit>;
}
impl<E> OrNoHits for Result<sekejap::Set<'_>, E> {
    fn unwrap_or_default_hits(self) -> Vec<sekejap::Hit> {
        self.map(|s| s.collect()).unwrap_or_default()
    }
}

/// Snapshot reads are unavailable in the paged configuration, and that has to be
/// visible rather than assumed.
///
/// A snapshot shares the durable base by `Arc` and freezes a copy of the overlay.
/// That is sound because the base is immutable: a compaction writes a *new*
/// generation, so a snapshot holding the old one keeps reading the old one. Paged
/// stores are mutated in place, so a snapshot sharing them would watch the writer
/// change rows underneath it — not a stale photograph but an inconsistent one.
///
/// So `snapshot_db()` returns `None` and the caller falls back to locked reads.
/// This test exists so that stays a decision: if page-level copy-on-write is built
/// and snapshots become possible, this test fails and is deleted deliberately.
#[test]
fn paged_mode_without_snapshots_says_so() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 60);
    {
        let mut db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
        db.put("p/n999", &row(999)).unwrap();
        assert!(db.snapshot_db().is_none(),
                "a paged store offered a snapshot; if copy-on-write now makes that \
                 sound, this test should be removed on purpose rather than left \
                 passing by accident");
        // And the reads it falls back to still answer correctly.
        assert_eq!(rows(&db), 61);
    }
    // The same database without the paged stores does offer one, so the `None`
    // above is about the configuration and not about the data.
    let dir2 = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open_with_config(dir2.path(), Config {
            paged_topology: true, ..Config::resident() }).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
        for i in 0..60 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        db.compact().unwrap();
    }
    let mut db = CoreDB::open_with_config(dir2.path(), Config {
        paged_topology: true, ..Config::resident() }).unwrap();
    db.put("p/n999", &row(999)).unwrap();
    let snap = db.snapshot_db().expect("paged topology alone must still be snapshottable");
    assert_eq!(snap.query("SELECT _key FROM p").unwrap().collect().len(), 61,
               "the snapshot disagrees with the store it was taken from");
}

/// **Opening a paged store without the flags that wrote it must not destroy it.**
///
/// This was the most destructive bug the paged work produced, and it was reachable
/// from the public service API. `EngineBuilder::build()` called `open_paged`, which
/// sets `paged_topology` alone and had no way to ask for the others. Opening a
/// store written with the full configuration through it:
///
/// - reported itself healthy, `snapshot_reads` and all
/// - served a completely **empty** database — 0 rows, 0 count, empty MATCH
/// - **truncated `payloads.bin` from 8192 bytes to 0**, on open, with no write
///   issued, because the flat payload path takes a file it does not recognise
/// - and one write plus a compaction made the loss permanent
///
/// The first fix was to refuse the open. That stopped the destruction and left the
/// store unopenable by anything that did not already know how it was written —
/// which, for a service handed a directory, is everything.
///
/// The open now decides the layout from the files instead. A paged flag names a
/// file format, and the format was settled when the store was written; a config
/// that disagrees with the bytes is not a preference, it is wrong. So a wrong
/// config is corrected rather than obeyed or rejected, and what this test pins is
/// the property that mattered all along: however the store is opened, the rows are
/// still there afterwards.
#[test]
fn opening_a_paged_store_with_the_wrong_config_does_not_destroy_it() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path(), 40);
    // The files that hold the data, and how big they are before anything else
    // touches them. `db.lock` and the WAL are excluded: an open that succeeds is
    // entitled to write those.
    let sizes = |d: &std::path::Path| -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = std::fs::read_dir(d).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| (e.file_name().to_string_lossy().to_string(),
                      e.metadata().map(|m| m.len()).unwrap_or(0)))
            .filter(|(n, _)| n.ends_with(".bin") || n.ends_with(".rec"))
            .collect();
        v.sort();
        v
    };
    let before = sizes(dir.path());
    assert!(!before.is_empty(), "the fixture wrote no data files");

    for (label, cfg) in [
        ("what Engine used to do", Config { paged_topology: true, ..Config::resident() }),
        ("plain resident", Config::resident()),
        ("payloads only", Config { paged_topology: true, paged_payloads: true,
                                   ..Config::resident() }),
    ] {
        {
            let db = CoreDB::open_with_config(dir.path(), cfg)
                .unwrap_or_else(|e| panic!("[{label}] the open was refused: {e}"));
            assert_eq!(rows(&db), 40,
                       "[{label}] the store was opened as an empty database");
            assert_eq!(db.one("p/n0").forward("next").collect().len(), 1,
                       "[{label}] the graph was invisible");
        }
        for (name, len) in &before {
            let now = std::fs::metadata(dir.path().join(name)).map(|m| m.len()).unwrap_or(0);
            assert!(now >= *len,
                    "[{label}] {name} shrank from {len} to {now} bytes across an open");
        }
    }

    // And the config that wrote it still reads everything.
    let db = CoreDB::open_with_config(dir.path(), paged()).unwrap();
    assert_eq!(rows(&db), 40, "the store did not survive the opens");
    assert_eq!(db.one("p/n0").forward("next").collect().len(), 1);
}

/// **`UPDATE` must maintain the btree index in paged mode.**
///
/// `field_indexes` is the writable index map. On a paged store the index is loaded
/// as an mmap'd sidecar into `field_base` instead, so that map is empty and every
/// column looked unindexed to `UPDATE` — neither the old key was removed nor the
/// new one inserted, and the index went on answering from before the update.
///
/// The result is a query that lies in both directions and does not heal: `WHERE
/// n = 5` returns the row that is now 9999, `WHERE n = 9999` returns nothing, and
/// `MAX(n)` reports a value no row holds. It survives a compaction and a reopen.
///
/// `put_raw` hydrates the index before maintaining it. This path did not — which
/// made an update behave differently from a write to the same column.
#[test]
fn update_maintains_the_btree_index_in_paged_mode() {
    for (label, cfg) in [("paged", paged()), ("resident", Config::resident())] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
            for i in 0..60 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
            db.execute("CREATE INDEX ON p USING btree (n)").unwrap();
            db.compact().unwrap();
        }
        // Reopen, so a paged store serves the index from its mmap'd sidecar — the
        // state in which the writable map is empty.
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("UPDATE p SET n = 9999 WHERE n = 5").unwrap();

        let by = |db: &CoreDB, sql: &str| -> Vec<String> {
            let mut v: Vec<String> = db.query(sql).unwrap().collect()
                .iter().map(|h| h.slug.clone()).collect();
            v.sort();
            v
        };
        assert_eq!(by(&db, "SELECT _key FROM p WHERE n = 9999"), vec!["p/n5".to_string()],
                   "[{label}] the index does not know the new value");
        assert!(by(&db, "SELECT _key FROM p WHERE n = 5").is_empty(),
                "[{label}] the index still returns the row under its old value");
        let max = db.query("SELECT MAX(n) AS m FROM p").unwrap().collect();
        let m = max[0].payload.as_ref().unwrap()["m"].clone();
        let m = m.as_i64().or_else(|| m.as_f64().map(|f| f as i64));
        assert_eq!(m, Some(9999),
                   "[{label}] MAX answers from before the update (payload was {:?})",
                   max[0].payload);

        // And it must still be right after a compaction and a reopen, because the
        // original failure survived both.
        db.compact().unwrap();
        assert_eq!(by(&db, "SELECT _key FROM p WHERE n = 9999"), vec!["p/n5".to_string()],
                   "[{label}] the update was lost by the compaction");
    }
}

/// **`update_edge` and `unlink_where` must reach edges in the durable store.**
///
/// Both go straight to `EdgeStore`, which only ever sees the RAM overlay. Against
/// a compacted graph they reported a count and changed nothing: the edge kept its
/// old attributes, or stayed linked. `unlink` had exactly this hole and was fixed
/// by recording the withdrawal where base reads subtract it — these two are one
/// method over and never got the same treatment.
#[test]
fn edge_attribute_writes_reach_the_durable_store() {
    for (label, cfg) in [("paged", paged()), ("resident", Config::resident())] {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
            db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
            for i in 0..20 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
            for i in 0..10 {
                db.link_meta(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next",
                             &json!({"weight": i as i64}).to_string()).unwrap();
            }
            // Compact so every edge is durable rather than in the overlay.
            db.compact().unwrap();
        }
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        let before = db.edge_count();

        // update_edge against a durable edge.
        let n = db.update_edge("p/n3", "p/n4", "next", "{}", &json!({"weight": 777}).to_string());
        assert_eq!(n, 1, "[{label}] update_edge reported {n} edges updated");
        let got = db.edges_from("p/n3").into_iter()
            .find(|e| e.edge_type.as_deref() == Some("next"))
            .and_then(|e| e.meta)
            .and_then(|m| m.get("weight").and_then(|w| w.as_i64()));
        assert_eq!(got, Some(777),
                   "[{label}] the edge still carries its old attributes after update_edge");
        assert_eq!(db.edge_count(), before,
                   "[{label}] updating an edge changed how many there are");

        // unlink_where against a durable edge, matching on an attribute.
        let removed = db.unlink_where("p/n6", "p/n7", "next", &json!({"weight": 6}).to_string());
        assert_eq!(removed, 1, "[{label}] unlink_where reported {removed} removed");
        assert!(db.one("p/n6").forward("next").collect().is_empty(),
                "[{label}] the edge is still there after unlink_where");
        assert_eq!(db.edge_count(), before - 1,
                   "[{label}] the edge count did not follow the removal");

        // And both survive a compaction.
        db.compact().unwrap();
        assert!(db.one("p/n6").forward("next").collect().is_empty(),
                "[{label}] the removed edge came back after a compaction");
        let after = db.edges_from("p/n3").into_iter()
            .find(|e| e.edge_type.as_deref() == Some("next"))
            .and_then(|e| e.meta)
            .and_then(|m| m.get("weight").and_then(|w| w.as_i64()));
        assert_eq!(after, Some(777),
                   "[{label}] the updated attributes were lost by the compaction");
    }
}

/// **DDL that rewrites rows, and edge slug resolution, must reach the durable
/// store.**
///
/// All four read the RAM overlay directly — `self.collections`, `self.nodes`,
/// `self.slug_map` — which holds only what was written since the last compaction.
/// On a paged database that is nothing, so the schema changed and the data did
/// not; `DROP TABLE` was a no-op in *any* mode after a reopen, leaving rows
/// queryable under a table that no longer had a schema, and permanently
/// unreachable by a second DROP.
#[test]
fn ddl_and_edge_slugs_reach_the_durable_store() {
    for (label, cfg) in [("paged", paged()), ("resident", Config::resident())] {
        // Each case gets a store built, compacted and reopened, so nothing it
        // needs is left in the overlay.
        let fresh = || {
            let dir = tempfile::TempDir::new().unwrap();
            {
                let mut db = CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap();
                db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
                for i in 0..25 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
                for i in 0..10 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
                db.compact().unwrap();
            }
            (CoreDB::open_with_config(dir.path(), cfg.clone()).unwrap(), dir)
        };

        // Edge endpoints must resolve to slugs.
        {
            let (db, _d) = fresh();
            let e = db.edges_from("p/n0");
            assert_eq!(e.len(), 1, "[{label}] edges_from found nothing");
            assert_eq!(e[0].to_slug.as_deref(), Some("p/n1"),
                       "[{label}] edges_from could not name the far end");
            let back = db.edges_to("p/n1");
            assert_eq!(back[0].from_slug.as_deref(), Some("p/n0"),
                       "[{label}] edges_to could not name the near end");
        }

        // DROP COLUMN must actually leave the rows.
        {
            let (mut db, _d) = fresh();
            db.execute("ALTER TABLE p DROP COLUMN body").unwrap();
            let raw = db.get("p/n7").expect("row vanished");
            assert!(!raw.contains("\"body\""),
                    "[{label}] DROP COLUMN left the column in the row: {raw}");
        }

        // RENAME COLUMN must move the values.
        {
            let (mut db, _d) = fresh();
            db.execute("ALTER TABLE p RENAME COLUMN n TO idx").unwrap();
            let raw = db.get("p/n7").expect("row vanished");
            assert!(raw.contains("\"idx\"") && !raw.contains("\"n\":"),
                    "[{label}] RENAME COLUMN did not rewrite the row: {raw}");
        }

        // RENAME TABLE must take every row with it.
        {
            let (mut db, _d) = fresh();
            db.execute("ALTER TABLE p RENAME TO q").unwrap();
            assert_eq!(db.query("SELECT _key FROM q").unwrap().collect().len(), 25,
                       "[{label}] the renamed table did not bring its rows");
            // The old name stops naming anything, which is an error rather than
            // an empty table — the same answer PostgreSQL gives after a rename.
            assert!(matches!(db.query("SELECT _key FROM p"),
                             Err(sekejap::SqlError::UndefinedTable(_))),
                    "[{label}] the old name still resolves after the rename");
        }

        // DROP TABLE must actually drop.
        {
            let (mut db, _d) = fresh();
            db.execute("DROP TABLE p").unwrap();
            assert!(matches!(db.query("SELECT _key FROM p"),
                             Err(sekejap::SqlError::UndefinedTable(_))),
                    "[{label}] DROP TABLE left the name resolving");
            assert!(db.get("p/n7").is_none(), "[{label}] a dropped row is still readable");
        }
    }
}

/// **A bulk write must maintain btree indexes.**
///
/// `put_value_bulk` refreshed bm25, GIN and search and stopped there, so a row
/// written through it was invisible to every indexed `WHERE` — a full scan found
/// it, `WHERE t = 5` did not — until the process restarted and the index was
/// rebuilt from disk. That is the buffered prepared-insert route, so it is the IoT
/// write path specifically, and it is wrong in *both* storage modes.
#[test]
fn a_bulk_write_maintains_btree_indexes() {
    for (label, cfg) in [("paged", paged()), ("resident", Config::resident())] {
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
        db.put("p/n0", &row(0)).unwrap();
        db.execute("CREATE INDEX ON p USING btree (n)").unwrap();

        let rows: Vec<(String, serde_json::Value)> = (1..30)
            .map(|i| (format!("p/n{i}"), serde_json::from_str(&row(i)).unwrap()))
            .collect();
        db.put_value_bulk(rows).unwrap();

        let hits = |db: &CoreDB, sql: &str| db.query(sql).unwrap().collect().len();
        assert_eq!(hits(&db, "SELECT _key FROM p"), 30, "[{label}] rows are missing entirely");
        assert_eq!(hits(&db, "SELECT _key FROM p WHERE n = 17"), 1,
                   "[{label}] a bulk-written row is invisible to its own index");
        assert_eq!(hits(&db, "SELECT _key FROM p WHERE n > 5 AND n < 12"), 6,
                   "[{label}] a range over the index misses bulk-written rows");

        // And still right once the index has been through a compaction.
        db.compact().unwrap();
        assert_eq!(hits(&db, "SELECT _key FROM p WHERE n = 17"), 1,
                   "[{label}] the index lost the row at compaction");
    }
}

/// **Auto-compaction's overlay trigger actually fires.**
///
/// There are two triggers. The log-size one works. The other — "compact when the
/// RAM write-overlay holds this many nodes", documented as what bounds RAM growth
/// in paged mode — never fired at all, in any layout: 5 500 nodes against a
/// 1 000-node bound left `maybe_compact()` returning `false`.
///
/// It guarded on `!segments.is_empty()`, which asks whether a *mapped topology
/// segment* exists. The question it meant to ask is whether a compaction would
/// move these nodes out of RAM, and a mapped segment is only one of the two
/// things that make the answer yes — paged nodes are the other, and they are the
/// default. `compact()` had the identical bug and it was fixed there; the check
/// that decides whether to call `compact()` never got the same correction.
///
/// Underneath that was a second one. A compaction adopts what it writes — maps
/// the new topology as the base and empties the overlay it came from, which is
/// what returns the RAM — but only when a base already existed. So the *first*
/// compaction in a fresh process wrote the files and walked past them, and
/// nothing changed until the database was reopened. That is the service case
/// exactly: `open_as_service` uses this layout, and a service that creates its
/// database and never restarts got no overlay compaction at all.
///
/// The resident layout is the one that correctly does **not** fire: there is no
/// base, the maps *are* the database, and folding them would return nothing while
/// looping forever.
#[test]
fn the_overlay_threshold_triggers_a_compaction() {
    use sekejap::{AutoCompact, CompactThresholds};

    let run = |base: Config| -> (bool, usize) {
        let cfg = Config {
            auto_compact: AutoCompact::Manual,
            // Small enough to reach in a test, and the log bound put out of reach
            // so only the overlay trigger can fire.
            compact_thresholds: CompactThresholds { wal_bytes: 1 << 40, overlay_entries: 1_000 },
            ..base
        };
        let dir = tempfile::TempDir::new().unwrap();
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
        for i in 0..500 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        db.compact().unwrap();                       // establish a base
        for i in 500..6_000 { db.put(&format!("p/n{i}"), &row(i)).unwrap(); }
        let fired = db.maybe_compact().unwrap();
        (fired, rows(&db))
    };

    let (fired, n) = run(Config::default());
    assert!(fired, "the overlay threshold did not fire in the default layout — this \
                    is the bound that keeps RAM from growing with the writes");
    assert_eq!(n, 6_000, "the compaction it triggered lost rows");

    let (fired, n) = run(Config { paged_topology: true, ..Config::resident() });
    assert!(fired, "the overlay threshold did not fire with paged topology — a \
                    service that never restarts would never compact on it");
    assert_eq!(n, 6_000, "the compaction it triggered lost rows");

    let (fired, n) = run(Config::resident());
    assert!(!fired, "the overlay threshold fired in the resident layout, where the \
                     maps are the database and folding them returns nothing — that \
                     is a compaction loop, not a bound");
    assert_eq!(n, 6_000, "rows went missing without a compaction running at all");
}
