//! Bounded cross-binary compatibility fixture helper for the V2 typed
//! collection engine. See docs/core/V2_COMPAT_FIXTURES.md for the method, the
//! exact subcommands, the run sequence, and — importantly — what this tool
//! does NOT prove. Source of truth for scope/limitations/status:
//! .integration-loop/compat-status.md.
use sekejap_core::{
    collections::{Clock, CollectionOptions, Database},
    pagewal::PageWalStore,
    Kind, Result,
};
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

const CACHE_BYTES: usize = 1 << 20;
const OVERFLOW_TEXT_BYTES: usize = 6_000; // exceeds the ~4KB page threshold (D5)
const CLOCK_CREATED: i64 = 1_700_000_000;
const CLOCK_UPDATED: i64 = 1_700_003_600;
const CLOCK_VERIFY: i64 = 1_700_007_200;

fn cfg() -> Config {
    Config {
        budget_bytes: CACHE_BYTES,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn require(cond: bool, msg: &str) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(msg.into())
    }
}

struct FixedClock(AtomicI64);
impl FixedClock {
    fn new(t: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(t)))
    }
    fn set(&self, t: i64) {
        self.0.store(t, Ordering::Relaxed);
    }
}
impl Clock for FixedClock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("make-reference") => {
            let root = args
                .get(1)
                .ok_or("usage: v2_compat_fixture make-reference ROOT")?;
            cmd_make_reference(Path::new(root))
        }
        Some("verify-and-update") => {
            let root = args
                .get(1)
                .ok_or("usage: v2_compat_fixture verify-and-update ROOT --confirm-copy")?;
            let confirmed = args.iter().skip(2).any(|a| a == "--confirm-copy");
            cmd_verify_and_update(Path::new(root), confirmed)
        }
        Some("verify-raw") => {
            let root = args
                .get(1)
                .ok_or("usage: v2_compat_fixture verify-raw ROOT")?;
            cmd_verify_raw(Path::new(root))
        }
        Some("legacy-replay") => {
            let target = args.get(1).ok_or(
                "usage: v2_compat_fixture legacy-replay TARGET_DIR OUT_DIR --explicit-test-mode",
            )?;
            let out = args.get(2).ok_or(
                "usage: v2_compat_fixture legacy-replay TARGET_DIR OUT_DIR --explicit-test-mode",
            )?;
            let explicit = args.iter().skip(3).any(|a| a == "--explicit-test-mode");
            cmd_legacy_replay(Path::new(target), Path::new(out), explicit)
        }
        _ => Err(
            "usage: v2_compat_fixture <make-reference|verify-and-update|verify-raw|legacy-replay> ..."
                .into(),
        ),
    }
}

// ---------------------------------------------------------------- make-reference

fn cmd_make_reference(root: &Path) -> Result<()> {
    require(
        !root.exists(),
        &format!("make-reference: ROOT must not already exist: {}", root.display()),
    )?;
    fs::create_dir_all(root)?;
    let old_source = root.join("old-source");
    let scenario = build_reference(&old_source)?;

    let pairs = collect_kv_pairs(&old_source)?;
    let projected = root.join("projected");
    fs::create_dir_all(&projected)?;
    project(&projected.join("checkpointed"), &pairs, true)?;
    project(&projected.join("wal-pending"), &pairs, false)?;

    let manifest = json!({
        "format_version": 1,
        "generated_by": "v2_compat_fixture make-reference",
        "method": "raw-projection: logical KV pairs from an old kernel-Store-backed \
            Database were streamed into a fresh PageWalStore through the old source's \
            own already-accepted public PageWalStore API. This does not exercise a \
            Database-to-PageWalStore integration writer; see docs/core/V2_COMPAT_FIXTURES.md. \
            This baseline is pre-release, not the frozen release format.",
        "old_source_dir": "old-source",
        "targets": {"checkpointed": "projected/checkpointed", "wal_pending": "projected/wal-pending"},
        "kv_pair_count": pairs.len(),
        "scenario": scenario,
    });
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "root": root.display().to_string(),
            "kv_pair_count": pairs.len(),
            "entities": scenario["entities"].as_array().map(Vec::len).unwrap_or(0),
            "deleted": scenario["deleted"].as_array().map(Vec::len).unwrap_or(0),
        }))?
    );
    Ok(())
}

/// Runs against the OLD source tree only: builds a small, deterministic,
/// bounded database through the existing public Database API and returns a
/// manifest of its durable state, captured live (not hand-computed).
fn build_reference(dir: &Path) -> Result<Value> {
    let clock = FixedClock::new(CLOCK_CREATED);
    let mut db = Database::create(dir, cfg())?;
    db.set_clock(clock.clone());

    let docs = db.create_collection(
        "docs",
        vec![
            ("title".into(), Kind::Text),
            ("body".into(), Kind::Json),
            ("loc".into(), Kind::Point),
        ],
        CollectionOptions { timestamps: false },
    )?;
    db.put(
        docs,
        "alpha",
        &json!({"title":"Alpha","body":{"tags":["a","b"],"n":1},
            "loc":{"type":"Point","coordinates":[1.5,-2.25]}}),
    )?;
    db.put(
        docs,
        "beta",
        &json!({"title":"Beta","body":{"tags":[],"n":2},
            "loc":{"type":"Point","coordinates":[10.0,20.0]}}),
    )?;
    db.put(
        docs,
        "overflow",
        &json!({"title":"x".repeat(OVERFLOW_TEXT_BYTES),"body":{"n":3},
            "loc":{"type":"Point","coordinates":[0.0,0.0]}}),
    )?;
    db.put(
        docs,
        "to-delete",
        &json!({"title":"Gone","body":{"n":4},
            "loc":{"type":"Point","coordinates":[5.0,5.0]}}),
    )?;
    db.commit()?;
    require(
        db.delete(docs, "to-delete")?,
        "to-delete must exist before removal",
    )?;

    let vecs = db.create_collection(
        "vecs",
        vec![("embedding".into(), Kind::Vector(4))],
        CollectionOptions { timestamps: true },
    )?;
    db.put(vecs, "v1", &json!({"embedding":[0.1,0.2,0.3,0.4]}))?;
    db.put(vecs, "v2", &json!({"embedding":[1.0,-1.0,0.5,-0.5]}))?;
    db.commit()?;

    clock.set(CLOCK_UPDATED);
    let before_layout = db.collection_info(vecs)?.layout.id;
    let after_layout = db.alter_collection(
        vecs,
        vec![
            ("embedding".into(), Kind::Vector(4)),
            ("label".into(), Kind::Text),
        ],
    )?;
    require(
        before_layout != after_layout,
        "schema evolution must allocate a new layout id",
    )?;
    db.put(
        vecs,
        "v3",
        &json!({"embedding":[0.0,0.0,0.0,1.0],"label":"new-layout"}),
    )?;
    db.update(vecs, "v1", &json!({"embedding":[0.9,0.8,0.7,0.6]}))?;
    require(db.delete(vecs, "v2")?, "v2 must exist before removal")?;
    db.commit()?;

    let manifest = snapshot_manifest(
        &db,
        &[("docs", docs), ("vecs", vecs)],
        &[("docs", "to-delete"), ("vecs", "v2")],
    )?;
    drop(db);
    Ok(manifest)
}

/// Captures the live, durable state of the given collections plus a fixed
/// deterministic clock record. Called right after a commit, from a still-open
/// Database, so it reflects exactly what a fresh reopen would also see.
fn snapshot_manifest(
    db: &Database,
    collections: &[(&str, sekejap_core::collections::CollectionId)],
    deleted: &[(&str, &str)],
) -> Result<Value> {
    let mut coll_json = Vec::new();
    let mut entity_json = Vec::new();
    for (name, id) in collections {
        let info = db.collection_info(*id)?;
        let fields: Vec<Value> = info
            .layout
            .fields
            .iter()
            .map(|(n, k)| json!({"name": n, "kind": format!("{k:?}")}))
            .collect();
        coll_json.push(json!({
            "name": name,
            "id": id.0,
            "layout_id": info.layout.id,
            "timestamps": info.timestamps,
            "fields": fields,
        }));
        for row in db.scan(*id, None)? {
            let e = row?;
            entity_json.push(json!({
                "collection": name,
                "collection_id": id.0,
                "sequence": e.id.sequence,
                "key": e.key,
                "document": e.document,
            }));
        }
    }
    let deleted_json: Vec<_> = deleted
        .iter()
        .map(|(c, k)| json!({"collection": c, "key": k}))
        .collect();
    Ok(json!({
        "clock": {"created": CLOCK_CREATED, "updated": CLOCK_UPDATED},
        "collections": coll_json,
        "entities": entity_json,
        "deleted": deleted_json,
    }))
}

fn collect_kv_pairs(dir: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let store = Store::open_snapshot(dir, cfg())?;
    let mut pairs = Vec::new();
    for row in store.scan(&[])? {
        let (k, v) = row?;
        pairs.push((k, v));
    }
    Ok(pairs)
}

/// Streams already-encoded logical KV pairs into a fresh PageWalStore. Uses
/// only PageWalStore's existing, already-accepted public API (src/pagewal.rs
/// is not modified or reimplemented here).
fn project(dir: &Path, pairs: &[(Vec<u8>, Vec<u8>)], do_checkpoint: bool) -> Result<()> {
    let mut pw = PageWalStore::open(dir, true, CACHE_BYTES)?;
    for (k, v) in pairs {
        pw.put(k, v)?;
    }
    pw.commit()?;
    if do_checkpoint {
        let done = pw.checkpoint()?;
        require(
            done,
            "checkpoint expected to complete immediately (no live snapshots)",
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------ verify-and-update

fn cmd_verify_and_update(root: &Path, confirmed_copy: bool) -> Result<()> {
    require(
        confirmed_copy,
        "verify-and-update refuses to run without --confirm-copy: this command commits \
            new writes into ROOT's targets. Copy ROOT (or projected/* + manifest.json) \
            away from the make-reference original first, then pass the copy's root with \
            --confirm-copy. See docs/core/V2_COMPAT_FIXTURES.md.",
    )?;
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    let scenario = manifest
        .get("scenario")
        .ok_or("manifest.json missing \"scenario\"")?;
    for (label, rel) in manifest_targets(&manifest)? {
        verify_and_update_one(&root.join(&rel), scenario, &label)?;
    }
    println!("verify-and-update: all targets verified and advanced");
    Ok(())
}

fn manifest_targets(manifest: &Value) -> Result<Vec<(String, String)>> {
    let t = manifest
        .get("targets")
        .and_then(Value::as_object)
        .ok_or("manifest.json missing \"targets\"")?;
    let mut out = Vec::new();
    for label in ["checkpointed", "wal_pending"] {
        let rel = t
            .get(label)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("manifest.json missing targets.{label}"))?;
        out.push((label.to_string(), rel.to_string()));
    }
    Ok(out)
}

fn verify_and_update_one(dir: &Path, scenario: &Value, label: &str) -> Result<()> {
    let mut db = Database::open(dir, cfg())?;
    verify_scenario(&db, scenario, label)?;
    db.set_clock(FixedClock::new(CLOCK_VERIFY));

    let docs = db
        .collection("docs")?
        .ok_or_else(|| format!("{label}: docs collection missing"))?;
    let vecs = db
        .collection("vecs")?
        .ok_or_else(|| format!("{label}: vecs collection missing"))?;

    db.update(docs, "alpha", &json!({"title":"Alpha-v2"}))?;
    let expected_alpha = db
        .get(docs, "alpha")?
        .ok_or_else(|| format!("{label}: alpha missing right after update"))?
        .document;
    let gamma_doc = json!({"title":"Gamma","body":{"n":5},
        "loc":{"type":"Point","coordinates":[9.0,9.0]}});
    let gamma_id = db.put(docs, "gamma", &gamma_doc)?;
    require(db.delete(vecs, "v3")?, "v3 must exist before removal")?;
    db.commit()?;
    drop(db);

    let db = Database::open(dir, cfg())?;
    let alpha_after = db
        .get(docs, "alpha")?
        .ok_or_else(|| format!("{label}: alpha missing after reopen"))?;
    require(
        alpha_after.document == expected_alpha,
        &format!("{label}: alpha document mismatch after reopen"),
    )?;
    let gamma_after = db
        .get_by_id(gamma_id)?
        .ok_or_else(|| format!("{label}: gamma missing after reopen"))?;
    require(
        gamma_after.document == gamma_doc,
        &format!("{label}: gamma document mismatch after reopen"),
    )?;
    check_absent(&db, "vecs", "v3", label)?;
    check_absent(&db, "vecs", "v2", label)?;
    check_absent(&db, "docs", "to-delete", label)?;
    verify_scenario_subset(
        &db,
        scenario,
        &[("docs", "beta"), ("docs", "overflow"), ("vecs", "v1")],
        label,
    )?;
    println!("{label}: verify-and-update passed (update+insert+delete, commit, reopen)");
    Ok(())
}

fn verify_scenario(db: &Database, scenario: &Value, label: &str) -> Result<()> {
    let collections = scenario
        .get("collections")
        .and_then(Value::as_array)
        .ok_or("manifest scenario missing \"collections\"")?;
    for c in collections {
        let name = c["name"].as_str().ok_or("collection entry missing name")?;
        let id = db
            .collection(name)?
            .ok_or_else(|| format!("{label}: collection {name} missing"))?;
        let expected_id = c["id"].as_u64().ok_or("collection entry missing id")? as u32;
        require(
            id.0 == expected_id,
            &format!("{label}: collection id mismatch for {name}"),
        )?;
        let info = db.collection_info(id)?;
        let expected_ts = c["timestamps"]
            .as_bool()
            .ok_or("collection entry missing timestamps")?;
        require(
            info.timestamps == expected_ts,
            &format!("{label}: timestamps flag mismatch for {name}"),
        )?;
        let expected_layout = c["layout_id"]
            .as_u64()
            .ok_or("collection entry missing layout_id")?;
        require(
            info.layout.id == expected_layout,
            &format!("{label}: layout id mismatch for {name}"),
        )?;
        let fields_val = c["fields"].as_array().ok_or("collection entry missing fields")?;
        let mut expected_fields: Vec<(String, String)> = Vec::with_capacity(fields_val.len());
        for f in fields_val {
            let name = f["name"].as_str().ok_or("field entry missing name")?.to_string();
            let kind = f["kind"].as_str().ok_or("field entry missing kind")?.to_string();
            expected_fields.push((name, kind));
        }
        let actual_fields: Vec<(String, String)> = info
            .layout
            .fields
            .iter()
            .map(|(n, k)| (n.clone(), format!("{k:?}")))
            .collect();
        require(
            actual_fields == expected_fields,
            &format!("{label}: layout fields mismatch for {name}"),
        )?;
    }
    let entities = scenario
        .get("entities")
        .and_then(Value::as_array)
        .ok_or("manifest scenario missing \"entities\"")?;
    for e in entities {
        verify_one_entity(db, e, label)?;
    }
    let deleted = scenario
        .get("deleted")
        .and_then(Value::as_array)
        .ok_or("manifest scenario missing \"deleted\"")?;
    for d in deleted {
        let coll = d["collection"]
            .as_str()
            .ok_or("deleted entry missing collection")?;
        let key = d["key"].as_str().ok_or("deleted entry missing key")?;
        check_absent(db, coll, key, label)?;
    }
    println!(
        "{label}: baseline scenario verified ({} entities, {} deletions)",
        entities.len(),
        deleted.len()
    );
    Ok(())
}

fn verify_one_entity(db: &Database, e: &Value, label: &str) -> Result<()> {
    let coll = e["collection"]
        .as_str()
        .ok_or("entity entry missing collection")?;
    let key = e["key"].as_str().ok_or("entity entry missing key")?;
    let expected_seq = e["sequence"].as_u64().ok_or("entity entry missing sequence")?;
    let expected_doc = &e["document"];
    let id = db
        .collection(coll)?
        .ok_or_else(|| format!("{label}: collection {coll} missing"))?;
    let found = db
        .get(id, key)?
        .ok_or_else(|| format!("{label}: entity {coll}/{key} missing"))?;
    require(
        found.id.sequence == expected_seq,
        &format!("{label}: sequence mismatch for {coll}/{key}"),
    )?;
    require(
        &found.document == expected_doc,
        &format!("{label}: document mismatch for {coll}/{key}"),
    )?;
    let by_id = db
        .get_by_id(found.id)?
        .ok_or_else(|| format!("{label}: get_by_id missing for {coll}/{key}"))?;
    require(
        by_id.key == key,
        &format!("{label}: key mismatch via get_by_id for {coll}/{key}"),
    )
}

fn verify_scenario_subset(
    db: &Database,
    scenario: &Value,
    keep: &[(&str, &str)],
    label: &str,
) -> Result<()> {
    let entities = scenario
        .get("entities")
        .and_then(Value::as_array)
        .ok_or("manifest scenario missing \"entities\"")?;
    for e in entities {
        let coll = e["collection"]
            .as_str()
            .ok_or("entity entry missing collection")?;
        let key = e["key"].as_str().ok_or("entity entry missing key")?;
        if !keep.contains(&(coll, key)) {
            continue;
        }
        verify_one_entity(db, e, label)?;
    }
    Ok(())
}

fn check_absent(db: &Database, coll: &str, key: &str, label: &str) -> Result<()> {
    let id = db
        .collection(coll)?
        .ok_or_else(|| format!("{label}: collection {coll} missing"))?;
    require(
        db.get(id, key)?.is_none(),
        &format!("{label}: expected {coll}/{key} to be absent"),
    )
}

// ------------------------------------------------------------------- verify-raw

/// Bounded structural check only: confirms the page-WAL container opens and
/// scans cleanly. Says nothing about typed decoding — see the module doc and
/// docs/core/V2_COMPAT_FIXTURES.md.
fn cmd_verify_raw(root: &Path) -> Result<()> {
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    for (label, rel) in manifest_targets(&manifest)? {
        let dir = root.join(&rel);
        let pw = PageWalStore::open(&dir, false, CACHE_BYTES)?;
        let mut seen = 0usize;
        pw.scan(|_, _| {
            seen += 1;
            true
        })?;
        require(seen > 0, &format!("{label}: raw scan found no entries"))?;
        println!(
            "{label}: raw PageWalStore opened and scanned {seen} entries \
                (structural check only, not typed-reader compatibility)"
        );
    }
    Ok(())
}

// --------------------------------------------------------------- legacy-replay

/// Explicit test mode only. Tests record/catalog/layout ENCODING
/// compatibility by replaying an updated PageWalStore's KV pairs into a fresh
/// legacy kernel::store::Store and reading it back with the OLD typed
/// decoder. This is not an automatic migration path.
fn cmd_legacy_replay(target_dir: &Path, out_dir: &Path, explicit: bool) -> Result<()> {
    require(
        explicit,
        "legacy-replay requires --explicit-test-mode: it exercises the OLD typed decoder \
            against NEW-engine bytes to test record/catalog/layout ENCODING compatibility \
            only. It is not an automatic production migration path.",
    )?;
    require(
        !out_dir.exists(),
        &format!(
            "legacy-replay: OUT_DIR must not already exist: {}",
            out_dir.display()
        ),
    )?;
    let pw = PageWalStore::open(target_dir, false, CACHE_BYTES)?;
    let mut pairs = Vec::new();
    pw.scan(|k, v| {
        pairs.push((k.to_vec(), v.to_vec()));
        true
    })?;
    drop(pw);

    let mut legacy = Store::create(out_dir, cfg())?;
    for (k, v) in &pairs {
        legacy.put(k, v)?;
    }
    legacy.checkpoint()?;
    drop(legacy);

    let db = Database::open(out_dir, cfg())?;
    let mut checked = 0usize;
    for name in ["docs", "vecs"] {
        if let Some(id) = db.collection(name)? {
            for row in db.scan(id, None)? {
                row?;
                checked += 1;
            }
        }
    }
    require(checked > 0, "legacy-replay decoded zero entities")?;
    println!(
        "legacy-replay: old typed decoder read {checked} entities from {} replayed KV pairs \
            (encoding test only, not a migration guarantee)",
        pairs.len()
    );
    Ok(())
}
