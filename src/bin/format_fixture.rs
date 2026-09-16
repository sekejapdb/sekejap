//! Reference fixture generator for format v1. Uses only the public typed API
//! (`collections::Database` over PageWalStore). Prints nothing on success.
//!
//! Close does not checkpoint: `Database` / `PageWalStore` have no Drop that
//! folds the WAL. A wal-pending fixture is `commit()` then `drop` with no
//! `checkpoint()`. A checkpointed fixture calls `checkpoint()` before drop.
use e4_prototype::{
    collections::{Clock, CollectionOptions, Database},
    pagewal::{create_compact_cells, set_create_compact_cells},
    Kind,
};
use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

const CACHE: usize = 1 << 20;
const CLOCK: i64 = 1_700_000_000;
const PEOPLE: u64 = 200;
const EVENTS: u64 = 80;
const DELETED_SLOT: u64 = 13;
const REINSERT_SLOT: u64 = 7;
const OVERFLOW_JSON: usize = 5000;

fn cfg() -> Config {
    Config {
        budget_bytes: CACHE,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn limits() -> ResourceLimits {
    ResourceLimits {
        data_bytes: 4 << 20,
        wal_bytes: 2 << 20,
        tracked_pages: 8192,
        readers: 8,
        record_bytes: 32 << 10,
        recovery_bytes: 64 << 10,
    }
}

struct FixedClock(AtomicI64);
impl FixedClock {
    fn new(t: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(t)))
    }
}
impl Clock for FixedClock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn key(slot: u64) -> String {
    format!("p{slot:08}")
}

/// Deterministic document. `version` is 0 at first insert and 1 after a
/// delete-then-reinsert of the same external key.
fn person(slot: u64, version: u64) -> Value {
    const FIRST: &[&str] = &[
        "Aisha", "Budi", "Chloe", "Dewi", "Ethan", "Fatima", "Gabriel", "Hana", "Imani", "José",
        "Kai", "Linh", "Mei", "Noah", "Olivia", "Priya", "Ravi", "Sofia", "Tariq", "Yuki",
    ];
    const LAST: &[&str] = &[
        "Adams",
        "Bakker",
        "Chen",
        "Davis",
        "Evans",
        "Fernández",
        "Gupta",
        "Hassan",
        "Ibrahim",
        "Jones",
        "Kim",
        "Lestari",
        "Martin",
        "Nguyen",
        "Okafor",
        "Patel",
        "Rossi",
        "Santos",
        "Tanaka",
        "Wijaya",
    ];
    let year = 1940 + slot % 80;
    let language = ["en", "id", "es", "ja"][(slot % 4) as usize];
    let fullname = format!(
        "{} {}",
        FIRST[(slot % FIRST.len() as u64) as usize],
        LAST[((slot / 20) % LAST.len() as u64) as usize]
    );
    let income = if slot % 10 == 0 {
        Value::Null
    } else {
        json!(20_000.0 + (slot * 73 % 180_000) as f64 + 0.25)
    };
    let mut d = json!({
        "fullname": fullname,
        "born": year,
        "income": income,
        "location": {"type": "Point", "coordinates": [
            (slot % 360) as f64 - 180.0,
            (slot % 170) as f64 - 85.0
        ]},
        "profile": {
            "languages": [language, "en"],
            "preferences": {"contact": slot % 2 == 0, "score": (slot % 100) as f64 / 4.0},
            "tags": ["person", null],
            "household": {"size": 1 + slot % 7}
        },
        "note": if version > 0 {
            format!("reinserted-v{version} 東京-é")
        } else if slot % 5 == 0 {
            "東京-é Fernández".to_string()
        } else {
            format!("n{slot}")
        },
    });
    if slot % 3 == 0 {
        d["source"] = json!("survey");
    }
    if slot % 7 == 0 {
        d["extra"] = json!({"notes": [null, "東京-é"], "unsigned": u64::MAX});
    }
    d
}

fn event(slot: u64, with_status: bool) -> Value {
    let mut d = json!({"title": format!("event-{slot}")});
    if with_status {
        d["status"] = json!(if slot % 2 == 0 { "open" } else { "closed" });
    }
    d
}

fn expected_event(slot: u64, with_status: bool) -> Value {
    let mut d = event(slot, with_status);
    d["_created_unix"] = json!(CLOCK);
    d["_updated_unix"] = json!(CLOCK);
    d
}

fn embedding(slot: u64) -> Vec<f64> {
    (0..1536)
        .map(|i| ((slot + i as u64) % 17) as f64 * 0.25)
        .collect()
}

fn blob(slot: u64) -> Value {
    match slot {
        0 => json!({
            "embedding": embedding(0),
            "payload": {"k": "small", "n": 0},
            "where": {"type": "Point", "coordinates": [1.0, 2.0]},
            "score": 0.5,
        }),
        1 => json!({
            "embedding": embedding(1),
            "payload": {"k": "vec-only"},
            "where": {"type": "Point", "coordinates": [3.0, 4.0]},
            "score": 1.25,
        }),
        2 => json!({
            "embedding": (0..1536).map(|_| 0.0).collect::<Vec<f64>>(),
            "payload": {"blob": "x".repeat(OVERFLOW_JSON), "n": 2},
            "where": {"type": "Point", "coordinates": [0.0, 0.0]},
            "score": 2.0,
        }),
        _ => json!({
            "embedding": (0..1536).map(|_| 0.0).collect::<Vec<f64>>(),
            "payload": {"blob": "y".repeat(OVERFLOW_JSON), "n": 3, "bin": {"unsigned": u64::MAX, "z": null}},
            "where": {"type": "Point", "coordinates": [-1.0, 1.0]},
            "score": Value::Null,
        }),
    }
}

fn kind_name(k: &Kind) -> String {
    format!("{k:?}")
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut padded = data.to_vec();
    let bit_len = (data.len() as u64).saturating_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h;
        for i in 0..64 {
            let s1 = a[4].rotate_right(6) ^ a[4].rotate_right(11) ^ a[4].rotate_right(25);
            let ch = (a[4] & a[5]) ^ ((!a[4]) & a[6]);
            let t1 = a[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a[0].rotate_right(2) ^ a[0].rotate_right(13) ^ a[0].rotate_right(22);
            let maj = (a[0] & a[1]) ^ (a[0] & a[2]) ^ (a[1] & a[2]);
            let t2 = s0.wrapping_add(maj);
            a[7] = a[6];
            a[6] = a[5];
            a[5] = a[4];
            a[4] = a[3].wrapping_add(t1);
            a[3] = a[2];
            a[2] = a[1];
            a[1] = a[0];
            a[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(a[i]);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn provenance() -> Result<(String, String), Box<dyn std::error::Error>> {
    let head = Command::new("git").args(["rev-parse", "HEAD"]).output()?;
    if !head.status.success() {
        return Err("git rev-parse HEAD failed".into());
    }
    let diff = Command::new("git").args(["diff", "HEAD"]).output()?;
    if !diff.status.success() {
        return Err("git diff HEAD failed".into());
    }
    Ok((
        String::from_utf8(head.stdout)?.trim().to_string(),
        hex(&sha256(&diff.stdout)),
    ))
}

fn file_records(dir: &Path) -> Result<BTreeMap<String, Value>, Box<dyn std::error::Error>> {
    let mut files = BTreeMap::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        if !e.file_type()?.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "MANIFEST.json" {
            continue;
        }
        let bytes = fs::read(e.path())?;
        files.insert(
            name,
            json!({"sha256": hex(&sha256(&bytes)), "bytes": bytes.len()}),
        );
    }
    Ok(files)
}

fn dir_bytes(files: &BTreeMap<String, Value>) -> u64 {
    files
        .values()
        .map(|v| v["bytes"].as_u64().unwrap_or(0))
        .sum()
}

fn coll_entry(
    name: &str,
    id: u32,
    layout_id: u64,
    timestamps: bool,
    fields: &[(String, Kind)],
) -> Value {
    json!({
        "name": name,
        "id": id,
        "layout_id": layout_id,
        "timestamps": timestamps,
        "fields": fields.iter().map(|(n, k)| json!({"name": n, "kind": kind_name(k)})).collect::<Vec<_>>(),
    })
}

fn expected_state() -> (Vec<Value>, Vec<Value>, Vec<Value>, usize) {
    let people_fields = vec![
        ("fullname".into(), Kind::Text),
        ("born".into(), Kind::Int),
        ("income".into(), Kind::Real),
        ("location".into(), Kind::Point),
        ("profile".into(), Kind::Json),
        ("note".into(), Kind::Text),
    ];
    let events_fields = vec![
        ("title".into(), Kind::Text),
        ("status".into(), Kind::Text),
        ("_created_unix".into(), Kind::Int),
        ("_updated_unix".into(), Kind::Int),
    ];
    let blobs_fields = vec![
        ("embedding".into(), Kind::Vector(1536)),
        ("payload".into(), Kind::Json),
        ("where".into(), Kind::Point),
        ("score".into(), Kind::Real),
    ];
    // Allocation order: people (id 1, layout 1), events (id 2, layout 2 then
    // alter -> layout 3), blobs (id 3, layout 4).
    let collections = vec![
        coll_entry("people", 1, 1, false, &people_fields),
        coll_entry("events", 2, 3, true, &events_fields),
        coll_entry("blobs", 3, 4, false, &blobs_fields),
    ];
    let mut entities = Vec::new();
    for slot in 0..PEOPLE {
        if slot == DELETED_SLOT {
            continue;
        }
        let version = if slot == REINSERT_SLOT { 1 } else { 0 };
        entities.push(json!({
            "collection": "people",
            "key": key(slot),
            "document": person(slot, version),
        }));
    }
    for slot in 0..EVENTS {
        let with_status = slot >= EVENTS / 2;
        entities.push(json!({
            "collection": "events",
            "key": key(slot),
            "document": expected_event(slot, with_status),
        }));
    }
    for slot in 0..4u64 {
        entities.push(json!({
            "collection": "blobs",
            "key": key(slot),
            "document": blob(slot),
        }));
    }
    let deleted = vec![json!({"collection": "people", "key": key(DELETED_SLOT)})];
    let n = entities.len();
    (collections, entities, deleted, n)
}

fn populate(db: &mut Database) -> Result<(), Box<dyn std::error::Error>> {
    db.set_clock(FixedClock::new(CLOCK));
    let people = db.create_collection(
        "people",
        vec![
            ("fullname".into(), Kind::Text),
            ("born".into(), Kind::Int),
            ("income".into(), Kind::Real),
            ("location".into(), Kind::Point),
            ("profile".into(), Kind::Json),
            ("note".into(), Kind::Text),
        ],
        CollectionOptions { timestamps: false },
    )?;
    for slot in 0..PEOPLE {
        db.put(people, &key(slot), &person(slot, 0))?;
    }
    db.commit()?;
    if !db.delete(people, &key(DELETED_SLOT))? {
        return Err("deleted slot missing".into());
    }
    if !db.delete(people, &key(REINSERT_SLOT))? {
        return Err("reinsert slot missing".into());
    }
    db.put(people, &key(REINSERT_SLOT), &person(REINSERT_SLOT, 1))?;
    db.commit()?;

    let events = db.create_collection(
        "events",
        vec![("title".into(), Kind::Text)],
        CollectionOptions { timestamps: true },
    )?;
    for slot in 0..EVENTS / 2 {
        db.put(events, &key(slot), &event(slot, false))?;
    }
    db.commit()?;
    db.alter_collection(
        events,
        vec![
            ("title".into(), Kind::Text),
            ("status".into(), Kind::Text),
        ],
    )?;
    for slot in EVENTS / 2..EVENTS {
        db.put(events, &key(slot), &event(slot, true))?;
    }
    db.commit()?;

    let blobs = db.create_collection(
        "blobs",
        vec![
            ("embedding".into(), Kind::Vector(1536)),
            ("payload".into(), Kind::Json),
            ("where".into(), Kind::Point),
            ("score".into(), Kind::Real),
        ],
        CollectionOptions { timestamps: false },
    )?;
    for slot in 0..4u64 {
        db.put(blobs, &key(slot), &blob(slot))?;
    }
    db.commit()?;
    Ok(())
}

fn write_fixture(
    dir: &Path,
    compact: bool,
    checkpointed: bool,
    limited: bool,
    git_head: &str,
    diff_sha: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    let mut db = if limited {
        Database::create_limited(dir, cfg(), limits())?
    } else {
        Database::create(dir, cfg())?
    };
    populate(&mut db)?;
    if checkpointed {
        if !db.checkpoint()? {
            return Err("checkpoint deferred; no live reader was expected".into());
        }
    }
    drop(db);

    let (collections, entities, deleted, n) = expected_state();
    let files = file_records(dir)?;
    let features: Vec<&str> = if compact {
        vec!["COMPACT_CELLS"]
    } else {
        vec![]
    };
    let mut limits_json = Value::Null;
    if limited {
        let l = limits();
        limits_json = json!({
            "data_bytes": l.data_bytes,
            "wal_bytes": l.wal_bytes,
            "tracked_pages": l.tracked_pages,
            "readers": l.readers,
            "record_bytes": l.record_bytes,
            "recovery_bytes": l.recovery_bytes,
        });
    }
    let manifest = json!({
        "format": "e4-format-v1",
        "header_magic": "E4PWAL02",
        "features": features,
        "compact_cells": compact,
        "checkpointed": checkpointed,
        "limited": limited,
        "generator": {
            "source": "src/bin/format_fixture.rs",
            "git_head": git_head,
            "diff_sha256": diff_sha,
        },
        "files": files,
        "collections": collections,
        "entities": entities,
        "deleted": deleted,
        "counts": {
            "entities": n,
            "collections": collections.len(),
            "deleted": deleted.len(),
        },
        "limits": limits_json,
    });
    fs::write(
        dir.join("MANIFEST.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(json!({
        "name": dir.file_name().unwrap().to_string_lossy(),
        "bytes": dir_bytes(&files),
        "entities": n,
        "checkpointed": checkpointed,
        "compact_cells": compact,
        "limited": limited,
        "manifest_sha256": hex(&sha256(&fs::read(dir.join("MANIFEST.json"))?)),
    }))
}

fn run(out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if out.exists() {
        fs::remove_dir_all(out)?;
    }
    fs::create_dir_all(out)?;
    let (git_head, diff_sha) = provenance()?;
    let original = create_compact_cells();
    let restore = || {
        let _ = set_create_compact_cells(original);
    };
    let result = (|| {
        let mut fixtures = Vec::new();
        for compact in [false, true] {
            set_create_compact_cells(compact)?;
            for checkpointed in [true, false] {
                let name = format!(
                    "compact-{}-{}",
                    if compact { "on" } else { "off" },
                    if checkpointed {
                        "checkpointed"
                    } else {
                        "wal-pending"
                    }
                );
                fixtures.push(write_fixture(
                    &out.join(&name),
                    compact,
                    checkpointed,
                    false,
                    &git_head,
                    &diff_sha,
                )?);
            }
        }
        set_create_compact_cells(false)?;
        fixtures.push(write_fixture(
            &out.join("limited-checkpointed"),
            false,
            true,
            true,
            &git_head,
            &diff_sha,
        )?);
        let index = json!({
            "format": "e4-format-v1",
            "header_magic": "E4PWAL02",
            "generator": {
                "source": "src/bin/format_fixture.rs",
                "git_head": git_head,
                "diff_sha256": diff_sha,
            },
            "fixtures": fixtures,
        });
        fs::write(out.join("INDEX.json"), serde_json::to_vec_pretty(&index)?)?;
        Ok(())
    })();
    restore();
    result
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: format_fixture OUT_DIR");
        process::exit(2);
    };
    if let Err(e) = run(&PathBuf::from(out)) {
        eprintln!("{e}");
        process::exit(1);
    }
}
