//! V2 foundation benchmark: a fair 1,000,000-person mixed-data comparison of
//! E4 typed collections (the public `sekejap_core::collections::Database`
//! API) against SQLite with typed columns + JSONB. See
//! docs/core/V2_BENCHMARK_PROTOCOL.md for the full protocol, conventions reused
//! from the existing benchmarks, and named deviations/asymmetries.
//!
//! Usage: v2_foundation_bench ROOT [N] [off|on]
//!   ROOT   authorized Linux/PVC directory for database + temp files (created)
//!   N      row count, default 1_000_000
//!   off|on managed-timestamp scenario, default off
//! Env: V2_VECTOR_DIM (default 8), V2_BATCH (default 1000),
//! V2_SQLITE_AUTOCHECKPOINT (default 1000 pages, matching E4's automatic
//! ~4MiB fold threshold; 0 is a separate, explicitly-labelled experiment,
//! never the main result).
use sekejap_core::collections::{Clock, CollectionId, CollectionOptions, Database};
use sekejap_core::Kind;
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const CACHE_BYTES: usize = 8 << 20;
const PROGRESS: u64 = 250_000;
const FIXED_TIME: i64 = 1_800_000_000;
/// E4's page-WAL folds automatically at roughly 4 MiB / half its allowance
/// (see src/pagewal.rs); 1000 SQLite pages at the shared 4096-byte page size
/// is ~4 MiB too, so this is the comparable default main-result policy.
/// `V2_SQLITE_AUTOCHECKPOINT=0` is available for a clearly separate,
/// explicitly-labelled no-auto-checkpoint experiment, never the main result.
const DEFAULT_SQLITE_AUTOCHECKPOINT: u32 = 1000;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn log(msg: String) {
    println!("{msg}");
    std::io::stdout().flush().unwrap();
}
fn cfg() -> Config {
    Config {
        budget_bytes: CACHE_BYTES,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn vector_dim() -> usize {
    std::env::var("V2_VECTOR_DIM").map_or(8, |s| s.parse().expect("V2_VECTOR_DIM"))
}
fn batch_size() -> u64 {
    std::env::var("V2_BATCH").map_or(1000, |s| s.parse().expect("V2_BATCH"))
}
fn sqlite_autocheckpoint_pages() -> u32 {
    std::env::var("V2_SQLITE_AUTOCHECKPOINT")
        .map_or(DEFAULT_SQLITE_AUTOCHECKPOINT, |s| {
            s.parse().expect("V2_SQLITE_AUTOCHECKPOINT")
        })
}
/// Deterministic "wall clock" driven by phase index, not real time, so both
/// engines' created/updated fields stay exactly oracle-predictable while
/// still genuinely advancing across phases (unlike a single frozen constant).
fn phase_time(phase: usize) -> i64 {
    FIXED_TIME + phase as i64
}
/// Shared with E4's `Clock` trait; the driver sets it once per phase, before
/// that phase's first write, to `phase_time(phase)`.
struct PhaseClock(std::sync::atomic::AtomicI64);
impl PhaseClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(std::sync::atomic::AtomicI64::new(FIXED_TIME)))
    }
    fn set(&self, phase: usize) {
        self.0
            .store(phase_time(phase), std::sync::atomic::Ordering::Relaxed);
    }
}
impl Clock for PhaseClock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}
fn fields(dim: usize) -> Vec<(String, Kind)> {
    vec![
        ("name".into(), Kind::Text),
        ("born".into(), Kind::Int),
        ("born_year".into(), Kind::Int),
        ("active".into(), Kind::Bool),
        ("income".into(), Kind::Real),
        ("location".into(), Kind::Point),
        ("profile".into(), Kind::Json),
        ("vector".into(), Kind::Vector(dim)),
    ]
}

// ---- deterministic keyspace: pure functions of a slot number, so nothing
// ---- needs to be held in RAM to generate, write or verify a phase.
fn orig_key(slot: u64) -> String {
    format!("person/{slot:010}")
}
fn reinsert_key(slot: u64) -> String {
    format!("person/reinsert-{slot:010}")
}
fn mixed_key(round: u32, slot: u64) -> String {
    format!("person/mixed{round}-{slot:010}")
}
fn is_update(slot: u64) -> bool {
    slot % 5 == 0
}
fn is_delete(slot: u64) -> bool {
    slot % 10 == 1
}
fn is_mixed1(slot: u64) -> bool {
    slot % 10 == 6
}
fn is_mixed2(slot: u64) -> bool {
    slot % 10 == 2
}
fn vector_values(slot: u64, dim: usize) -> Vec<f64> {
    // Pre-round to f32 so the value already matches what both engines
    // store/return (E4 vector lanes and the SQLite BLOB are both f32).
    (0..dim)
        .map(|j| {
            let v = (slot % 97) as f32 * 0.01 + j as f32 * 0.001;
            v as f64
        })
        .collect()
}
fn content(slot: u64, version: u64, dim: usize) -> Value {
    let year = 1940 + slot % 80;
    let language = ["en", "id", "es", "ja"][(slot % 4) as usize];
    json!({
        "name": format!("Person {slot:08} 東京-é"),
        "born": (year * 10000 + (1 + slot % 12) * 100 + 1 + slot % 28) as i64,
        "born_year": year as i64,
        "active": (slot + version) % 3 != 0,
        "income": if slot % 7 == 0 { Value::Null } else { json!(20000.0 + (slot * 73 % 180000) as f64 + 0.25) },
        "location": {"type":"Point","coordinates":[-179.0+(slot*37%358000) as f64/1000.0,-85.0+(slot*53%170000) as f64/1000.0]},
        "profile": {
            "languages": [language, "en"],
            "revision": version,
            "preferences": {"contact": slot % 2 == 0, "score": (slot % 100) as f64 / 4.0},
            "tags": ["person", null],
            "household": {"size": 1 + slot % 7}
        },
        "vector": vector_values(slot, dim)
    })
}
/// Stamps `_created_unix`/`_updated_unix` for an identity created at
/// `created_phase` and last written at `updated_phase` (>= created_phase).
/// `created_phase` never changes for a given identity once assigned;
/// `updated_phase` advances whenever that identity is written again.
fn with_timestamps_at(mut d: Value, times: bool, created_phase: usize, updated_phase: usize) -> Value {
    if times {
        d["_created_unix"] = json!(phase_time(created_phase));
        d["_updated_unix"] = json!(phase_time(updated_phase));
    }
    d
}
/// U-group version reached after a given phase (0=load..6=mixed2); 0 for
/// slots outside the update group, and for slots whose identity has moved
/// to a fresh key (they are never members of the update group; disjoint).
fn uversion(slot: u64, phase: usize) -> u64 {
    if !is_update(slot) {
        return 0;
    }
    match phase {
        0 => 0,
        1 => 1,
        2 | 3 | 4 => 2,
        5 => 3,
        _ => 4,
    }
}
/// Last phase at which the update group's *original* identity (still under
/// `orig_key`) was written; distinct from `uversion`'s content version
/// number, though both change on the same phases for this group.
fn u_updated_phase(slot: u64, phase: usize) -> usize {
    if !is_update(slot) {
        return 0;
    }
    match phase {
        0 => 0,
        1 => 1,
        2 | 3 | 4 => 2,
        5 => 5,
        _ => 6,
    }
}
/// Current external key for `slot` after `phase`, or None if deleted with
/// no replacement yet (only true for the delete group at phase 3).
fn expected_key(slot: u64, phase: usize) -> Option<String> {
    if is_delete(slot) {
        return match phase {
            0..=2 => Some(orig_key(slot)),
            3 => None,
            _ => Some(reinsert_key(slot)),
        };
    }
    if is_mixed1(slot) {
        return match phase {
            0..=4 => Some(orig_key(slot)),
            _ => Some(mixed_key(1, slot)),
        };
    }
    if is_mixed2(slot) {
        return match phase {
            0..=5 => Some(orig_key(slot)),
            _ => Some(mixed_key(2, slot)),
        };
    }
    Some(orig_key(slot))
}
/// Only called for a slot still alive under its own `orig_key` (checked by
/// the `surviving` filter in `expected_stream`): created at phase 0, updated
/// whenever the update group is written again.
fn expected_doc(slot: u64, phase: usize, dim: usize, times: bool) -> Value {
    let doc = content(slot, uversion(slot, phase), dim);
    with_timestamps_at(doc, times, 0, u_updated_phase(slot, phase))
}
fn count_residue(n: u64, modulus: u64, residue: u64) -> u64 {
    if residue >= n {
        0
    } else {
        (n - residue - 1) / modulus + 1
    }
}
/// Streams the expected (id, key, document) triples in the exact order the
/// real scan/SELECT returns them: surviving originals in ascending slot
/// order, then each later phase's freshly created identities in ascending
/// slot order within their group. IDs are not merely asserted increasing —
/// they are independently derived here from E4's own sequence-allocation
/// contract (a per-collection monotonic counter that only advances on a
/// genuine create, mirrored exactly by the SQLite arm's `next_id`): a
/// surviving original slot keeps its load-time id `slot + 1`; each later
/// group's fresh identities get consecutive ids starting right after the
/// highest id any earlier phase could have allocated (`n`, then `n + |D|`,
/// then `n + |D| + |M1|`), in the ascending-slot order those creates
/// actually run in (`enumerate()` on the same filtered range used to drive
/// `run_phase`, so the rank an id is derived from is the same rank the
/// create actually happened in — never recomputed via separate arithmetic
/// that could silently drift from the write order).
fn expected_stream(n: u64, phase: usize, dim: usize, times: bool) -> Box<dyn Iterator<Item = (u64, String, Value)>> {
    let surviving = (0..n)
        .filter(move |&s| expected_key(s, phase).as_deref() == Some(orig_key(s).as_str()))
        .map(move |s| (s + 1, orig_key(s), expected_doc(s, phase, dim, times)));
    let mut chain: Box<dyn Iterator<Item = (u64, String, Value)>> = Box::new(surviving);
    if phase >= 4 {
        chain = Box::new(chain.chain((0..n).filter(|&s| is_delete(s)).enumerate().map(
            move |(rank, s)| {
                (
                    n + rank as u64 + 1,
                    reinsert_key(s),
                    with_timestamps_at(content(s, 0, dim), times, 4, 4),
                )
            },
        )));
    }
    if phase >= 5 {
        let base = n + count_residue(n, 10, 1);
        chain = Box::new(chain.chain((0..n).filter(|&s| is_mixed1(s)).enumerate().map(
            move |(rank, s)| {
                (
                    base + rank as u64 + 1,
                    mixed_key(1, s),
                    with_timestamps_at(content(s, 0, dim), times, 5, 5),
                )
            },
        )));
    }
    if phase >= 6 {
        let base = n + count_residue(n, 10, 1) + count_residue(n, 10, 6);
        chain = Box::new(chain.chain((0..n).filter(|&s| is_mixed2(s)).enumerate().map(
            move |(rank, s)| {
                (
                    base + rank as u64 + 1,
                    mixed_key(2, s),
                    with_timestamps_at(content(s, 0, dim), times, 6, 6),
                )
            },
        )));
    }
    chain
}
fn population(n: u64, phase: usize) -> u64 {
    if phase == 3 {
        n - count_residue(n, 10, 1)
    } else {
        n
    }
}

// ---- disk accounting: recursive logical/allocated bytes, matching the
// ---- convention already used in dist/src/cli/collections.rs.
fn disk(dir: &Path) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let mut total = (0, 0);
    for e in fs::read_dir(dir)? {
        let e = e?;
        let m = match e.metadata() {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if m.is_dir() {
            let d = disk(&e.path())?;
            total.0 += d.0;
            total.1 += d.1;
        } else {
            total.0 += m.len();
            #[cfg(unix)]
            {
                total.1 += m.blocks() * 512;
            }
        }
    }
    Ok(total)
}
struct Monitor {
    dir: PathBuf,
    peak: Arc<Mutex<(u64, u64, u64, u64)>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Monitor {
    fn start(dir: &Path) -> Self {
        let peak = Arc::new(Mutex::new((0, 0, 0, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s, d) = (peak.clone(), stop.clone(), dir.to_owned());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                Self::observe(&d, &p);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            dir: dir.into(),
            peak,
            stop,
            thread: Some(thread),
        }
    }
    fn observe(dir: &Path, p: &Mutex<(u64, u64, u64, u64)>) {
        let d = disk(dir);
        let mut p = p.lock().unwrap();
        match d {
            Ok((l, a)) => {
                p.0 = p.0.max(l);
                p.1 = p.1.max(a);
                p.2 += 1;
            }
            Err(_) => p.3 += 1,
        }
    }
    fn take(&self) -> Value {
        Self::observe(&self.dir, &self.peak);
        let mut p = self.peak.lock().unwrap();
        let v = json!({"sampled_peak_logical":p.0,"sampled_peak_allocated":p.1,"samples":p.2,"sample_errors":p.3});
        *p = (0, 0, 0, 0);
        v
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

// ---- backends
enum Backend {
    E4(Database),
    Sql(Connection),
}
struct Db {
    inner: Backend,
    next_id: u64,
    times: bool,
    dim: usize,
    cid: CollectionId,
    clock: Arc<PhaseClock>,
    sqlite_autocheckpoint: u32,
}
fn vector_bytes(doc: &Value) -> Vec<u8> {
    doc["vector"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|v| (v.as_f64().unwrap() as f32).to_le_bytes())
        .collect()
}
fn vector_from_bytes(bytes: &[u8]) -> Vec<f64> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()) as f64)
        .collect()
}
impl Db {
    /// Each arm gets its own `<path>/tmp` directory, a subdirectory of the
    /// same `path` the recursive disk sampler already walks, so temp files
    /// are measured as part of that engine's total/peak — not left outside
    /// both engines' walked directories in a shared root-level `tmp/`.
    /// `TMPDIR`/`SQLITE_TMPDIR` are pointed at it before that arm's engine
    /// is opened. Ordering differs per engine because `Database::create`
    /// creates `path` itself with a bare (non-recursive, exists-fails)
    /// `fs::create_dir` (see `PageWalStore::open`): `path` must not exist
    /// yet when that call runs, so the env vars are set to a not-yet-
    /// existing directory first (E4 does not read them during creation) and
    /// the directory itself is created immediately once `path` exists.
    /// SQLite has no such constraint, so its directory and temp dir are
    /// created up front, before the connection opens.
    fn create(path: &Path, sql: bool, times: bool, dim: usize) -> R<Self> {
        let mut cid = CollectionId(1);
        let clock = PhaseClock::new();
        let autocheckpoint = sqlite_autocheckpoint_pages();
        let tmp = path.join("tmp");
        let inner = if sql {
            fs::create_dir_all(path)?;
            fs::create_dir_all(&tmp)?;
            std::env::set_var("TMPDIR", &tmp);
            std::env::set_var("SQLITE_TMPDIR", &tmp);
            let c = Connection::open(path.join("data.sqlite"))?;
            c.execute_batch(&format!("PRAGMA page_size=4096;PRAGMA journal_mode=WAL;PRAGMA cache_size=-8192;PRAGMA mmap_size=0;PRAGMA synchronous=FULL;PRAGMA fullfsync=ON;PRAGMA checkpoint_fullfsync=ON;PRAGMA wal_autocheckpoint={autocheckpoint};PRAGMA temp_store=FILE;"))?;
            let extra = if times {
                ",created INTEGER NOT NULL,updated INTEGER NOT NULL"
            } else {
                ""
            };
            c.execute_batch(&format!(
                "CREATE TABLE people(id INTEGER PRIMARY KEY,external_key TEXT NOT NULL UNIQUE,name TEXT NOT NULL,born INTEGER NOT NULL,born_year INTEGER NOT NULL,active INTEGER NOT NULL,income REAL,lon REAL NOT NULL,lat REAL NOT NULL,profile BLOB NOT NULL,vector BLOB NOT NULL{extra});"
            ))?;
            Backend::Sql(c)
        } else {
            std::env::set_var("TMPDIR", &tmp);
            std::env::set_var("SQLITE_TMPDIR", &tmp);
            let mut d = Database::create(path, cfg())?;
            fs::create_dir_all(&tmp)?;
            d.set_clock(clock.clone());
            cid = d.create_collection(
                "people",
                fields(dim),
                CollectionOptions { timestamps: times },
            )?;
            d.commit()?;
            Backend::E4(d)
        };
        Ok(Self {
            inner,
            next_id: 1,
            times,
            dim,
            cid,
            clock,
            sqlite_autocheckpoint: autocheckpoint,
        })
    }
    fn reapply_pragmas(c: &Connection, autocheckpoint: u32) -> R<()> {
        c.execute_batch(&format!("PRAGMA cache_size=-8192;PRAGMA mmap_size=0;PRAGMA synchronous=FULL;PRAGMA fullfsync=ON;PRAGMA checkpoint_fullfsync=ON;PRAGMA wal_autocheckpoint={autocheckpoint};PRAGMA temp_store=FILE;"))?;
        Ok(())
    }
    /// Actual, observed policy (not just what we requested) — reported so
    /// "record actual policies, including after reopen" is checkable rather
    /// than assumed from the PRAGMA statement we issued.
    fn policy_report(&self) -> R<Value> {
        match &self.inner {
            Backend::E4(_) => Ok(json!({
                "engine": "e4",
                "checkpoint_policy": "page-WAL automatic fold near its internal allowance (see src/pagewal.rs); explicit Database::checkpoint() also called every phase",
            })),
            Backend::Sql(c) => {
                let wal_autocheckpoint: i64 =
                    c.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?;
                let synchronous: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
                let journal_mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
                Ok(json!({
                    "engine": "sqlite",
                    "wal_autocheckpoint_pages": wal_autocheckpoint,
                    "synchronous": synchronous,
                    "journal_mode": journal_mode,
                    "explicit_checkpoint_per_phase": "PRAGMA wal_checkpoint(TRUNCATE)",
                }))
            }
        }
    }
    /// Sets this phase's deterministic "now" for both engines' managed
    /// timestamps, once, before that phase's first write.
    fn set_phase(&mut self, phase: usize) {
        self.clock.set(phase);
    }
    fn begin(&mut self) -> R<()> {
        if let Backend::Sql(c) = &self.inner {
            c.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(())
    }
    /// Durable + published; never folds/compacts (that is `checkpoint`).
    fn commit(&mut self) -> R<()> {
        match &mut self.inner {
            Backend::E4(d) => d.commit()?,
            Backend::Sql(c) => c.execute_batch("COMMIT")?,
        }
        Ok(())
    }
    /// Explicit fold/compaction step, distinct from `commit`. Requires no
    /// pending uncommitted transaction (the caller commits first, exactly
    /// once, via `commit`); does not itself commit or reopen anything.
    /// Returns (report, completed) where `completed=false` means the engine
    /// deferred folding (e.g. a live reader for E4, or SQLite reporting a
    /// busy/partial checkpoint).
    fn checkpoint(&mut self) -> R<(Value, bool)> {
        match &mut self.inner {
            Backend::E4(d) => {
                let completed = d.checkpoint()?;
                Ok((json!({"completed": completed}), completed))
            }
            Backend::Sql(c) => {
                let (busy, wal_frames, checkpointed_frames) =
                    c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
                    })?;
                let completed = busy == 0 && checkpointed_frames == wal_frames;
                Ok((
                    json!({"busy":busy,"wal_frames":wal_frames,"checkpointed_frames":checkpointed_frames,"completed":completed}),
                    completed,
                ))
            }
        }
    }
    /// `key` has never been used before; a brand-new identity is created.
    /// `now` is this phase's deterministic timestamp for the SQLite arm;
    /// the E4 arm reads the same value through its shared `PhaseClock`.
    fn create_row(&mut self, key: &str, doc: &Value, now: i64) -> R<()> {
        match &mut self.inner {
            Backend::E4(d) => {
                d.put(self.cid, key, doc)?;
            }
            Backend::Sql(c) => {
                let id = self.next_id;
                self.next_id += 1;
                let vector = vector_bytes(doc);
                let profile = serde_json::to_string(&doc["profile"])?;
                if self.times {
                    c.prepare_cached("INSERT INTO people VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,jsonb(?10),?11,?12,?13)")?
                        .execute(rusqlite::params![
                            id, key,
                            doc["name"].as_str(), doc["born"].as_i64(), doc["born_year"].as_i64(),
                            doc["active"].as_bool(), doc["income"].as_f64(),
                            doc["location"]["coordinates"][0].as_f64(), doc["location"]["coordinates"][1].as_f64(),
                            profile, vector, now, now
                        ])?;
                } else {
                    c.prepare_cached("INSERT INTO people VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,jsonb(?10),?11)")?
                        .execute(rusqlite::params![
                            id, key,
                            doc["name"].as_str(), doc["born"].as_i64(), doc["born_year"].as_i64(),
                            doc["active"].as_bool(), doc["income"].as_f64(),
                            doc["location"]["coordinates"][0].as_f64(), doc["location"]["coordinates"][1].as_f64(),
                            profile, vector
                        ])?;
                }
            }
        }
        Ok(())
    }
    /// `key` already identifies an existing row; its identity (and, for
    /// timestamps, its original `created` value) is unchanged — only
    /// content columns and `updated` (when enabled) are written, on both
    /// engines. This is always a full-document replace, never a
    /// column-subset patch, matching E4's `put()` (this benchmark does not
    /// exercise `Database::update()`'s patch API on either engine).
    fn update_row(&mut self, key: &str, doc: &Value, now: i64) -> R<()> {
        match &mut self.inner {
            Backend::E4(d) => {
                d.put(self.cid, key, doc)?;
            }
            Backend::Sql(c) => {
                let vector = vector_bytes(doc);
                let profile = serde_json::to_string(&doc["profile"])?;
                if self.times {
                    let n = c.prepare_cached("UPDATE people SET name=?1,born=?2,born_year=?3,active=?4,income=?5,lon=?6,lat=?7,profile=jsonb(?8),vector=?9,updated=?10 WHERE external_key=?11")?
                        .execute(rusqlite::params![
                            doc["name"].as_str(), doc["born"].as_i64(), doc["born_year"].as_i64(),
                            doc["active"].as_bool(), doc["income"].as_f64(),
                            doc["location"]["coordinates"][0].as_f64(), doc["location"]["coordinates"][1].as_f64(),
                            profile, vector, now, key
                        ])?;
                    assert_eq!(n, 1);
                } else {
                    let n = c.prepare_cached("UPDATE people SET name=?1,born=?2,born_year=?3,active=?4,income=?5,lon=?6,lat=?7,profile=jsonb(?8),vector=?9 WHERE external_key=?10")?
                        .execute(rusqlite::params![
                            doc["name"].as_str(), doc["born"].as_i64(), doc["born_year"].as_i64(),
                            doc["active"].as_bool(), doc["income"].as_f64(),
                            doc["location"]["coordinates"][0].as_f64(), doc["location"]["coordinates"][1].as_f64(),
                            profile, vector, key
                        ])?;
                    assert_eq!(n, 1);
                }
            }
        }
        Ok(())
    }
    fn delete_row(&mut self, key: &str) -> R<()> {
        match &mut self.inner {
            Backend::E4(d) => assert!(d.delete(self.cid, key)?),
            Backend::Sql(c) => assert_eq!(
                c.prepare_cached("DELETE FROM people WHERE external_key=?1")?
                    .execute(rusqlite::params![key])?,
                1
            ),
        }
        Ok(())
    }
    /// Closes this handle (dropping the E4 writer / SQLite connection,
    /// releasing `writer.lock` or the file descriptor) *before* opening a
    /// fresh one — `self` is consumed, not borrowed, specifically so the old
    /// backend cannot still be alive when the new one opens. An earlier
    /// version wrote `*d = Database::open(path, cfg())?;` through a
    /// `&mut Database`: the right-hand `open()` call is evaluated before the
    /// assignment, so the old handle (and its `writer.lock`) was still held
    /// at that point, which would fail with `WriterLocked` against a real
    /// page-WAL database. No in-memory placeholder is used for either
    /// engine; this always closes and reopens the real on-disk database.
    fn reopen(self, path: &Path) -> R<Self> {
        let Self {
            inner,
            next_id,
            times,
            dim,
            cid,
            clock,
            sqlite_autocheckpoint,
        } = self;
        let was_sql = matches!(inner, Backend::Sql(_));
        drop(inner);
        let inner = if was_sql {
            let fresh = Connection::open(path.join("data.sqlite"))?;
            Self::reapply_pragmas(&fresh, sqlite_autocheckpoint)?;
            Backend::Sql(fresh)
        } else {
            let mut d = Database::open(path, cfg())?;
            d.set_clock(clock.clone());
            Backend::E4(d)
        };
        Ok(Self {
            inner,
            next_id,
            times,
            dim,
            cid,
            clock,
            sqlite_autocheckpoint,
        })
    }
    fn verify(&self, n: u64, phase: usize) -> R<Value> {
        let started = Instant::now();
        let mut count = 0u64;
        let mut crc = 0u32;
        let mut last: Option<u64> = None;
        let mut check = |id: u64, key: String, doc: Value, expected: (u64, String, Value)| -> R<()> {
            if let Some(prev) = last {
                assert!(id > prev, "sequence/id must strictly increase");
            }
            last = Some(id);
            assert_eq!(id, expected.0, "key {key}: id must equal its independently derived expected id, never merely increase");
            assert_eq!(key, expected.1);
            assert_eq!(doc, expected.2, "key {key}");
            crc = crc32c::crc32c_append(crc, &id.to_le_bytes());
            crc = crc32c::crc32c_append(crc, key.as_bytes());
            crc = crc32c::crc32c_append(crc, &serde_json::to_vec(&doc)?);
            count += 1;
            Ok(())
        };
        let mut expected = expected_stream(n, phase, self.dim, self.times);
        match &self.inner {
            Backend::E4(d) => {
                for row in d.scan(self.cid, None)? {
                    let e = row?;
                    let exp = expected.next().ok_or("unexpected extra row")?;
                    check(e.id.sequence, e.key, e.document, exp)?;
                }
            }
            Backend::Sql(c) => {
                let cols = if self.times {
                    "id,external_key,name,born,born_year,active,income,lon,lat,json(profile),vector,created,updated"
                } else {
                    "id,external_key,name,born,born_year,active,income,lon,lat,json(profile),vector"
                };
                let mut s = c.prepare(&format!("SELECT {cols} FROM people ORDER BY id"))?;
                let mut rows = s.query([])?;
                while let Some(r) = rows.next()? {
                    let id: u64 = r.get(0)?;
                    let key: String = r.get(1)?;
                    let vector: Vec<u8> = r.get(10)?;
                    let mut doc = json!({
                        "name": r.get::<_, String>(2)?,
                        "born": r.get::<_, i64>(3)?,
                        "born_year": r.get::<_, i64>(4)?,
                        "active": r.get::<_, bool>(5)?,
                        "income": r.get::<_, Option<f64>>(6)?,
                        "location": {"type":"Point","coordinates":[r.get::<_,f64>(7)?, r.get::<_,f64>(8)?]},
                        "profile": serde_json::from_str::<Value>(&r.get::<_,String>(9)?)?,
                        "vector": vector_from_bytes(&vector),
                    });
                    if self.times {
                        doc["_created_unix"] = json!(r.get::<_, i64>(11)?);
                        doc["_updated_unix"] = json!(r.get::<_, i64>(12)?);
                    }
                    let exp = expected.next().ok_or("unexpected extra row")?;
                    check(id, key, doc, exp)?;
                }
            }
        }
        assert!(expected.next().is_none(), "missing expected rows");
        assert_eq!(count, population(n, phase));
        Ok(json!({"rows":count,"crc32c":crc,"verify_seconds":started.elapsed().as_secs_f64()}))
    }
}

#[derive(Default)]
struct PhaseCounts {
    creates: u64,
    updates: u64,
    deletes: u64,
}
fn run_phase(db: &mut Db, n: u64, phase: usize, dim: usize, batch: u64) -> R<(PhaseCounts, f64)> {
    let t = Instant::now();
    let mut c = PhaseCounts::default();
    let mut ops = 0u64;
    let now = phase_time(phase);
    db.set_phase(phase);
    db.begin()?;
    macro_rules! bump {
        () => {
            ops += 1;
            if ops % batch == 0 {
                db.commit()?;
                db.begin()?;
            }
            if ops % PROGRESS == 0 {
                log(format!("phase {phase} {ops} ops {:.1}s", t.elapsed().as_secs_f64()));
            }
        };
    }
    match phase {
        0 => {
            for slot in 0..n {
                db.create_row(&orig_key(slot), &content(slot, 0, dim), now)?;
                c.creates += 1;
                bump!();
            }
        }
        1 | 2 => {
            let version = phase as u64;
            for slot in (0..n).filter(|&s| is_update(s)) {
                db.update_row(&orig_key(slot), &content(slot, version, dim), now)?;
                c.updates += 1;
                bump!();
            }
        }
        3 => {
            for slot in (0..n).filter(|&s| is_delete(s)) {
                db.delete_row(&orig_key(slot))?;
                c.deletes += 1;
                bump!();
            }
        }
        4 => {
            for slot in (0..n).filter(|&s| is_delete(s)) {
                db.create_row(&reinsert_key(slot), &content(slot, 0, dim), now)?;
                c.creates += 1;
                bump!();
            }
        }
        5 | 6 => {
            let round = if phase == 5 { 1u32 } else { 2u32 };
            let version = if phase == 5 { 3u64 } else { 4u64 };
            let is_target: fn(u64) -> bool = if phase == 5 { is_mixed1 } else { is_mixed2 };
            for slot in 0..n {
                if is_update(slot) {
                    db.update_row(&orig_key(slot), &content(slot, version, dim), now)?;
                    c.updates += 1;
                    bump!();
                } else if is_target(slot) {
                    db.delete_row(&orig_key(slot))?;
                    c.deletes += 1;
                    bump!();
                    db.create_row(&mixed_key(round, slot), &content(slot, 0, dim), now)?;
                    c.creates += 1;
                    bump!();
                }
            }
        }
        _ => unreachable!(),
    }
    db.commit()?;
    Ok((c, t.elapsed().as_secs_f64()))
}

const PHASE_NAMES: [&str; 7] = [
    "load",
    "update_round_1",
    "update_round_2",
    "delete_only",
    "replacement_reinsert",
    "mixed_round_1",
    "mixed_round_2",
];

fn run_arm(root: &Path, sql: bool, n: u64, times: bool, dim: usize, batch: u64) -> R<Value> {
    let name = if sql { "sqlite" } else { "e4" };
    let path = root.join(name);
    log(format!("=== {name} arm: n={n} timestamps={times} dim={dim} batch={batch} ==="));
    let mut db = Db::create(&path, sql, times, dim)?;
    let initial_policy = db.policy_report()?;
    let monitor = Monitor::start(&path);
    let empty = disk(&path)?;
    let mut phases = Vec::new();
    // Engine-only totals: write-phase time and explicit-checkpoint time.
    // Verification (an independent oracle re-deriving and re-comparing
    // every row) is real, necessary benchmark work, but it is not either
    // engine's own cost, so it is reported per phase and separately
    // accumulated, never folded into `cumulative_seconds`/`total_seconds`.
    let mut cumulative_seconds = 0.0;
    let mut cumulative_checkpoint_seconds = 0.0;
    let mut cumulative_verify_seconds = 0.0;
    let mut cumulative_ops = 0u64;
    for phase in 0..=6usize {
        monitor.take();
        let (counts, seconds) = run_phase(&mut db, n, phase, dim, batch)?;
        // The phase's transaction(s) are already committed exactly once
        // inside run_phase; checkpoint() only folds/compacts and must not
        // commit or reopen anything.
        let t = Instant::now();
        let (checkpoint, checkpoint_completed) = db.checkpoint()?;
        let checkpoint_seconds = t.elapsed().as_secs_f64();
        let peak = monitor.take();
        let final_size = disk(&path)?;
        let verify = db.verify(n, phase)?;
        let verify_seconds = verify["verify_seconds"].as_f64().unwrap_or(0.0);
        cumulative_seconds += seconds;
        cumulative_checkpoint_seconds += checkpoint_seconds;
        cumulative_verify_seconds += verify_seconds;
        cumulative_ops += counts.creates + counts.updates + counts.deletes;
        let entry = json!({
            "phase": PHASE_NAMES[phase],
            "phase_index": phase,
            "creates": counts.creates,
            "updates": counts.updates,
            "deletes": counts.deletes,
            "seconds": seconds,
            "checkpoint_seconds": checkpoint_seconds,
            "checkpoint_completed": checkpoint_completed,
            "cumulative_seconds": cumulative_seconds,
            "cumulative_checkpoint_seconds": cumulative_checkpoint_seconds,
            "cumulative_engine_seconds": cumulative_seconds + cumulative_checkpoint_seconds,
            "cumulative_operations": cumulative_ops,
            "checkpoint": checkpoint,
            "peak": peak,
            "final_logical_bytes": final_size.0,
            "final_allocated_bytes": final_size.1,
            "live_population": population(n, phase),
            "verification": verify,
        });
        log(format!(
            "{name} phase {} done: {:.3}s (+{:.3}s checkpoint, completed={checkpoint_completed}) creates={} updates={} deletes={} population={} bytes={}",
            PHASE_NAMES[phase], seconds, checkpoint_seconds, counts.creates, counts.updates, counts.deletes,
            population(n, phase), final_size.0
        ));
        phases.push(entry);
        fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&json!({"engine":name,"phases":phases,"in_progress":true}))?,
        )?;
    }
    // Every phase already ends with its own commit and its own checkpoint
    // above; this final step is one more explicit checkpoint over an
    // otherwise-idle, already-committed database (never a redundant commit —
    // SQLite would refuse COMMIT with no open transaction).
    monitor.take();
    let t = Instant::now();
    let (final_checkpoint, final_checkpoint_completed) = db.checkpoint()?;
    let final_checkpoint_seconds = t.elapsed().as_secs_f64();
    let final_peak = monitor.take();
    let final_size = disk(&path)?;
    let t = Instant::now();
    db = db.reopen(&path)?;
    let reopen_seconds = t.elapsed().as_secs_f64();
    let reopened_policy = db.policy_report()?;
    let reopen_verification = db.verify(n, 6)?;
    let reopen_verify_seconds = reopen_verification["verify_seconds"].as_f64().unwrap_or(0.0);
    let total_seconds =
        cumulative_seconds + cumulative_checkpoint_seconds + final_checkpoint_seconds + reopen_seconds;
    let report = json!({
        "engine": name,
        "rows": n,
        "timestamps": times,
        "vector_dim": dim,
        "batch": batch,
        "cache_bytes": CACHE_BYTES,
        "sync": "FULL; fullfsync/checkpoint_fullfsync requested (inert off macOS)",
        "sqlite_version": rusqlite::version(),
        "initial_policy": initial_policy,
        "reopened_policy": reopened_policy,
        "empty_logical_bytes": empty.0,
        "empty_allocated_bytes": empty.1,
        "phases": phases,
        "cumulative_seconds_ops_only": cumulative_seconds,
        "cumulative_checkpoint_seconds": cumulative_checkpoint_seconds,
        "cumulative_verify_seconds_excluded_from_total": cumulative_verify_seconds,
        "final_checkpoint_seconds": final_checkpoint_seconds,
        "final_checkpoint": final_checkpoint,
        "final_checkpoint_completed": final_checkpoint_completed,
        "final_peak": final_peak,
        "final_logical_bytes": final_size.0,
        "final_allocated_bytes": final_size.1,
        "reopen_seconds": reopen_seconds,
        "reopen_verification": reopen_verification,
        "reopen_verify_seconds_excluded_from_total": reopen_verify_seconds,
        "total_seconds": total_seconds,
        "total_seconds_definition": "sum of per-phase op seconds (which include this benchmark's own cheap, pure content generation, run inline with each write, identically for both engines) + per-phase explicit checkpoint seconds + final checkpoint seconds + reopen seconds; excludes oracle verification time only, reported separately below and never folded in",
        "total_operations": cumulative_ops,
        "peak_semantics": "1ms samples, lower bound, not an enforced cap",
    });
    fs::write(
        root.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&report)?,
    )?;
    log(format!("{name} COMPLETE total_seconds={total_seconds:.3}"));
    Ok(report)
}

fn markdown(root: &Path, n: u64, e4: &Value, sql: &Value) -> R<String> {
    let mut out = String::new();
    writeln!(out, "# V2 foundation benchmark — {n} rows\n")?;
    writeln!(out, "Root: `{}`\n", root.display())?;
    writeln!(
        out,
        "Checkpoint/durability policy — E4: `{}`. SQLite initial: `{}`; after reopen: `{}`.\n",
        e4["initial_policy"]["checkpoint_policy"],
        sql["initial_policy"], sql["reopened_policy"],
    )?;
    writeln!(
        out,
        "| phase | E4 write s | SQLite write s | E4 ckpt s (done) | SQLite ckpt s (done) | E4 c/u/d | SQLite c/u/d | E4 bytes | SQLite bytes | E4 peak logical | SQLite peak logical |"
    )?;
    writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|")?;
    let e4_phases = e4["phases"].as_array().unwrap();
    let sql_phases = sql["phases"].as_array().unwrap();
    for i in 0..e4_phases.len().min(sql_phases.len()) {
        let (a, b) = (&e4_phases[i], &sql_phases[i]);
        writeln!(
            out,
            "| {} | {:.3} | {:.3} | {:.3} ({}) | {:.3} ({}) | {}/{}/{} | {}/{}/{} | {} | {} | {} | {} |",
            a["phase"], a["seconds"].as_f64().unwrap_or(0.0), b["seconds"].as_f64().unwrap_or(0.0),
            a["checkpoint_seconds"].as_f64().unwrap_or(0.0), a["checkpoint_completed"],
            b["checkpoint_seconds"].as_f64().unwrap_or(0.0), b["checkpoint_completed"],
            a["creates"], a["updates"], a["deletes"],
            b["creates"], b["updates"], b["deletes"],
            a["final_logical_bytes"], b["final_logical_bytes"],
            a["peak"]["sampled_peak_logical"], b["peak"]["sampled_peak_logical"],
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Final checkpoint: E4 {:.3}s (completed={}) / SQLite {:.3}s (completed={}). Reopen: E4 {:.3}s / SQLite {:.3}s.",
        e4["final_checkpoint_seconds"].as_f64().unwrap_or(0.0), e4["final_checkpoint_completed"],
        sql["final_checkpoint_seconds"].as_f64().unwrap_or(0.0), sql["final_checkpoint_completed"],
        e4["reopen_seconds"].as_f64().unwrap_or(0.0),
        sql["reopen_seconds"].as_f64().unwrap_or(0.0),
    )?;
    writeln!(
        out,
        "\nEngine-only total (writes + explicit checkpoints + final checkpoint + reopen; \
        excludes oracle verification): E4 {:.3}s ({} ops) vs SQLite {:.3}s ({} ops). \
        Verification time (excluded above): E4 {:.3}s / SQLite {:.3}s cumulative. \
        Final logical bytes: E4 {} / SQLite {}. Final allocated bytes: E4 {} / SQLite {}.",
        e4["total_seconds"].as_f64().unwrap_or(0.0), e4["total_operations"],
        sql["total_seconds"].as_f64().unwrap_or(0.0), sql["total_operations"],
        e4["cumulative_verify_seconds_excluded_from_total"].as_f64().unwrap_or(0.0),
        sql["cumulative_verify_seconds_excluded_from_total"].as_f64().unwrap_or(0.0),
        e4["final_logical_bytes"], sql["final_logical_bytes"],
        e4["final_allocated_bytes"], sql["final_allocated_bytes"],
    )?;
    writeln!(
        out,
        "\nThese numbers are a raw-write/mixed-CRUD comparison of typed collections\
        \nagainst SQLite typed columns + JSONB. They are not a claim about combined\
        \nmultimodel SELECT performance or about all eight laws; peaks are 1ms-sampled\
        \nlower bounds, not an enforced cap. See docs/core/V2_BENCHMARK_PROTOCOL.md."
    )?;
    Ok(out)
}

/// Both arms' `verify()` accumulate a CRC32C over the same (id, key,
/// document)-byte scheme, streamed from the same oracle; if both engines
/// truly match that oracle, their CRCs must also match each other.
fn cross_check_crc(e4: &Value, sql: &Value) {
    let e4_phases = e4["phases"].as_array().unwrap();
    let sql_phases = sql["phases"].as_array().unwrap();
    assert_eq!(e4_phases.len(), sql_phases.len(), "phase count mismatch between arms");
    for (a, b) in e4_phases.iter().zip(sql_phases) {
        assert_eq!(
            a["verification"]["crc32c"], b["verification"]["crc32c"],
            "cross-engine CRC mismatch at phase {}", a["phase"]
        );
    }
    assert_eq!(
        e4["reopen_verification"]["crc32c"], sql["reopen_verification"]["crc32c"],
        "cross-engine CRC mismatch after reopen"
    );
}
fn main() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    if !(2..=4).contains(&args.len()) {
        return Err("usage: v2_foundation_bench ROOT [N] [off|on]".into());
    }
    let root_arg = PathBuf::from(&args[1]);
    if !root_arg.is_absolute()
        || root_arg
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("ROOT must be an absolute, authorized directory with no '..' components".into());
    }
    let n: u64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(1_000_000);
    if n == 0 {
        return Err("rows must be positive".into());
    }
    let times = match args.get(3).map(String::as_str) {
        None | Some("off") => false,
        Some("on") => true,
        _ => return Err("timestamps must be off|on".into()),
    };
    let dim = vector_dim();
    let batch = batch_size();
    let root = root_arg.join(format!("v2-foundation-{n}-{}", now_unix()));
    fs::create_dir_all(&root)?;
    // No shared root-level tmp/TMPDIR here: each arm sets its own
    // `<arm>/tmp` and points TMPDIR/SQLITE_TMPDIR at it inside `Db::create`,
    // so temp files land inside the same directory the per-arm disk sampler
    // walks, not outside both engines' measured trees.
    log(format!("RUN {}", root.display()));

    let e4_report = run_arm(&root, false, n, times, dim, batch)?;
    let sql_report = run_arm(&root, true, n, times, dim, batch)?;
    // Both engines are checked independently against the same oracle above
    // (exact id/key/document equality, not just each engine agreeing with
    // itself), but the two engines' own accumulated CRC32Cs — built from the
    // identical id/key/document byte serialization on both sides — must
    // also agree with each other explicitly, phase by phase and after
    // reopen. This is a real cross-engine invariant, not a restatement of
    // the per-row oracle check.
    cross_check_crc(&e4_report, &sql_report);

    let manifest = json!({
        "rows": n,
        "timestamps": times,
        "vector_dim": dim,
        "batch": batch,
        "cache_bytes": CACHE_BYTES,
        "sqlite_autocheckpoint_pages_requested": sqlite_autocheckpoint_pages(),
        "seed": "deterministic pure functions of slot index; no external RNG",
        "row_selection": "ascending slot order; residue-class groups documented in docs/core/V2_BENCHMARK_PROTOCOL.md",
        "e4": e4_report,
        "sqlite": sql_report,
    });
    fs::write(root.join("results.json"), serde_json::to_vec_pretty(&manifest)?)?;
    let md = markdown(&root, n, &e4_report, &sql_report)?;
    fs::write(root.join("REPORT.md"), md)?;
    log(format!("COMPLETE {}", root.display()));
    Ok(())
}
