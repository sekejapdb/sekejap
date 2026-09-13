//! Lean, deterministic typed lifecycle oracle. All artifacts stay on scratch.
use e4_prototype::{decode_dense_v3, encode_dense_v3, Kind, Layout, Result};
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    time::Instant,
};

#[path = "lifecycle/space.rs"]
mod space;
#[path = "lifecycle/sustained.rs"]
mod sustained;

// Explicit benchmark-only selection; production policy is persisted by Store.
fn benchmark_limits() -> Result<Option<kernel::limits::ResourceLimits>> {
    let Some(bytes) = std::env::var_os("E4_LIMIT_DATA_BYTES") else { return Ok(None); };
    let l = kernel::limits::ResourceLimits {
        data_bytes: bytes.to_string_lossy().parse()?, wal_bytes: 4 << 20,
        tracked_pages: 65536, readers: 8, record_bytes: 65536, recovery_bytes: 4 << 20,
    }.validate()?;
    Ok(Some(l))
}
fn artifact_root_allowed(root: &Path) -> bool {
    if root.components().any(|c| matches!(c, std::path::Component::ParentDir)) { return false; }
    root.starts_with("<scratch>")
        || root.starts_with("<scratch>")
}
fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn layout() -> Layout {
    Layout {
        id: 1,
        fields: vec![
            ("name".into(), Kind::Text),
            ("active".into(), Kind::Bool),
            ("income".into(), Kind::Real),
            ("location".into(), Kind::Point),
            ("profile".into(), Kind::Json),
        ],
    }
}
fn key(id: u64) -> Vec<u8> {
    let b = id.to_be_bytes();
    let at = b.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = vec![0x80 + (8 - at) as u8];
    k.extend_from_slice(&b[at..]);
    k
}
fn id_of(k: &[u8]) -> u64 {
    assert!(!k.is_empty() && k[0] >= 0x81 && k[0] <= 0x88);
    assert_eq!(k.len(), 1 + (k[0] - 0x80) as usize);
    let mut b = [0; 8];
    b[9 - k.len()..].copy_from_slice(&k[1..]);
    u64::from_be_bytes(b)
}
fn doc(id: u64, cycle: u64) -> Value {
    let version = if id % 5 == 0 || id % 10 == 1 {
        cycle
    } else {
        0
    };
    let length = if version % 2 == 1 && id % 100 == 0 {
        9000
    } else if version % 2 == 1 && id % 10 == 0 {
        512
    } else {
        24
    };
    json!({"name":format!("Person {id:08} 東京-é"), "active":(id+version)%3!=0,
        "income":if id%7==0 { Value::Null } else { json!(25000.25+(id%1000) as f64) },
        "location":{"type":"Point","coordinates":[(id%300) as f64-150.0,(id%160) as f64-80.0]},
        "profile":{"revision":version,"notes":"x".repeat(length),"flags":[true,null,"sensor"]},
        "extra":{"unsigned":u64::MAX,"source":if version%2==0 {"base"} else {"edit"}}})
}
fn sql_config(c: &Connection) -> Result<()> {
    c.execute_batch("PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA wal_autocheckpoint=0;")?;
    Ok(())
}
enum Db {
    E4(Store),
    Sql(Connection),
}
impl Db {
    fn open(dir: &Path, sqlite: bool, create: bool) -> Result<Self> {
        let limits = if sqlite { None } else { benchmark_limits()? };
        if create && limits.is_none() {
            fs::create_dir(dir)?;
        }
        if sqlite {
            let c = Connection::open(dir.join("data.sqlite"))?;
            if create {
                c.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=WAL; CREATE TABLE person(id INTEGER PRIMARY KEY,name TEXT,active INTEGER,income REAL,lon REAL,lat REAL,profile BLOB,extra BLOB);")?;
            }
            sql_config(&c)?;
            Ok(Self::Sql(c))
        } else {
            let mut s = if create {
                if let Some(l) = limits { Store::create_limited(dir, cfg(), l)? }
                else { Store::create(dir, cfg())? }
            } else {
                Store::open(dir, cfg())?
            };
            if create {
                for i in 0..3 {
                    s.put(&[0, 240, i], &layout().descriptor()?)?;
                }
                s.commit()?;
            }
            Ok(Self::E4(s))
        }
    }
    fn snapshot(&self, dir: &Path) -> Result<Self> {
        match self {
            Self::E4(_) => Ok(Self::E4(Store::open_snapshot(
                dir,
                Config {
                    budget_bytes: 64 << 10,
                    ..cfg()
                },
            )?)),
            Self::Sql(_) => {
                let c = Connection::open_with_flags(
                    dir.join("data.sqlite"),
                    OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                c.execute_batch("PRAGMA cache_size=-64; PRAGMA mmap_size=0; BEGIN;")?;
                c.query_row("SELECT count(*) FROM person", [], |r| r.get::<_, u64>(0))?;
                Ok(Self::Sql(c))
            }
        }
    }
    fn begin(&self) -> Result<()> {
        if let Self::Sql(c) = self {
            c.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(())
    }
    fn put(&mut self, id: u64, cycle: u64) -> Result<()> {
        self.put_document(id, &doc(id, cycle))
    }
    fn put_document(&mut self, id: u64, d: &Value) -> Result<()> {
        match self {
            Self::E4(s) => s.put(&key(id), &encode_dense_v3(&layout(), &d)?.row)?,
            Self::Sql(c) => {
                c.prepare_cached(
                    "INSERT OR REPLACE INTO person VALUES(?,?,?,?,?,?,jsonb(?),jsonb(?))",
                )?
                .execute(params![
                    id,
                    d["name"].as_str(),
                    d["active"].as_bool(),
                    d["income"].as_f64(),
                    d["location"]["coordinates"][0].as_f64(),
                    d["location"]["coordinates"][1].as_f64(),
                    serde_json::to_string(&d["profile"])?,
                    serde_json::to_string(&d["extra"])?
                ])?;
            }
        }
        Ok(())
    }
    fn delete(&mut self, id: u64) -> Result<()> {
        match self {
            Self::E4(s) => assert!(s.delete(&key(id))?),
            Self::Sql(c) => assert_eq!(
                c.prepare_cached("DELETE FROM person WHERE id=?")?
                    .execute([id])?,
                1
            ),
        }
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        match self {
            Self::E4(s) => s.commit()?,
            Self::Sql(c) => c.execute_batch("COMMIT")?,
        }
        Ok(())
    }
    fn checkpoint(&mut self, pinned: bool) -> Result<()> {
        match self {
            Self::E4(s) => s.checkpoint()?,
            Self::Sql(c) => {
                let (busy, _, _): (i64, i64, i64) = c.query_row(
                    if pinned {
                        "PRAGMA wal_checkpoint(PASSIVE)"
                    } else {
                        "PRAGMA wal_checkpoint(TRUNCATE)"
                    },
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                if !pinned {
                    assert_eq!(busy, 0, "final SQLite checkpoint busy");
                }
            }
        }
        Ok(())
    }
    fn verify(&self, n: u64, cycle: u64, deleted: bool) -> Result<Value> {
        self.verify_expected(n, deleted, |id| doc(id, cycle))
    }
    fn verify_expected(
        &self,
        n: u64,
        deleted: bool,
        expected: impl Fn(u64) -> Value,
    ) -> Result<Value> {
        let t = Instant::now();
        let mut expected_id = 1;
        let mut count = 0u64;
        let mut crc = 0;
        let mut check = |id: u64, d: Value| -> Result<()> {
            while deleted && expected_id % 10 == 1 {
                expected_id += 1;
            }
            assert_eq!(id, expected_id, "missing/unexpected ID, deleted={deleted}");
            assert_eq!(d, expected(id), "wrong row {id}, deleted={deleted}");
            crc = crc32c::crc32c_append(crc, &serde_json::to_vec(&d)?);
            expected_id += 1;
            count += 1;
            Ok(())
        };
        match self {
            Self::E4(s) => {
                let l = Layout::from_descriptor(&s.get(&[0, 240, 0])?.ok_or("missing layout")?)?;
                for item in s.scan(&[0x81])? {
                    let (k, b) = item?;
                    check(
                        id_of(&k),
                        decode_dense_v3(&l, &b, |_| Err("unexpected vector".into()))?,
                    )?;
                }
            }
            Self::Sql(c) => {
                let mut st=c.prepare("SELECT id,name,active,income,lon,lat,json(profile),json(extra) FROM person ORDER BY id")?;
                let mut rows = st.query([])?;
                while let Some(r) = rows.next()? {
                    check(
                        r.get(0)?,
                        json!({"name":r.get::<_,String>(1)?,"active":r.get::<_,bool>(2)?,"income":r.get::<_,Option<f64>>(3)?,"location":{"type":"Point","coordinates":[r.get::<_,f64>(4)?,r.get::<_,f64>(5)?]},"profile":serde_json::from_str::<Value>(&r.get::<_,String>(6)?)?,"extra":serde_json::from_str::<Value>(&r.get::<_,String>(7)?)?}),
                    )?;
                }
            }
        }
        assert_eq!(count, if deleted { n - n / 10 } else { n });
        Ok(json!({"rows":count,"crc32c":crc,"seconds":t.elapsed().as_secs_f64(),"exact":true}))
    }
    fn structure(&self, dir: &Path, n: u64, deleted: bool) -> Result<Value> {
        match self {
            Self::E4(s) => {
                let (records, pages) = kernel::verify::verify_published_tree(
                    &dir.join("data"),
                    IoMode::Buffered,
                    s.published_root(),
                    1,
                )?;
                assert_eq!(records, if deleted { n - n / 10 + 3 } else { n + 3 });
                Ok(json!({"records":records,"reachable_pages":pages}))
            }
            Self::Sql(c) => {
                let r: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
                assert_eq!(r, "ok");
                Ok(json!({"integrity":r}))
            }
        }
    }
    fn counters(&self) -> Value {
        match self {
            Self::E4(s) => {
                let (eligible, waiting) = s.pool_ref().free_pages_split();
                let st = s.pool_stats();
                let io=s.io_stats().map(|i|json!({"data_read_calls":i.reads.load(Ordering::Relaxed),"data_write_calls":i.writes.load(Ordering::Relaxed),"data_write_bytes":i.write_bytes.load(Ordering::Relaxed)}));
                json!({"tracked_pages":s.pool_ref().tracked_pages(),"free_eligible":eligible,"free_waiting":waiting,"generation":s.generation(),"pool_misses":st.misses,"io":io})
            }
            Self::Sql(c) => {
                json!({"freelist_pages":c.query_row("PRAGMA freelist_count",[],|r|r.get::<_,u64>(0)).unwrap(),"page_count":c.query_row("PRAGMA page_count",[],|r|r.get::<_,u64>(0)).unwrap()})
            }
        }
    }
}
fn footprint(dir: &Path) -> Result<Value> {
    fn files(dir: &Path, base: &Path, out: &mut serde_json::Map<String, Value>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {
                files(&p, base, out)?;
            } else {
                out.insert(
                    p.strip_prefix(base)?.to_string_lossy().into_owned(),
                    json!(p.metadata()?.len()),
                );
            }
        }
        Ok(())
    }
    let mut f = serde_json::Map::new();
    files(dir, dir, &mut f)?;
    let total: u64 = f.values().map(|v| v.as_u64().unwrap()).sum();
    Ok(json!({"total_bytes":total,"files":f}))
}
fn mutate(db: &mut Db, n: u64, cycle: u64, stage: u8) -> Result<u64> {
    let mut pending = 0;
    let mut total = 0;
    db.begin()?;
    for i in 0..n {
        let id = 1 + (i * 7919 + 1237) % n; // bijection for the supported 2^a*5^b sizes
        let selected = match stage {
            0 => true,
            1 => id % 5 == 0 || id % 10 == 1,
            2 => id % 10 == 1,
            _ => unreachable!(),
        };
        if !selected {
            continue;
        }
        if pending == 1000 {
            db.commit()?;
            db.begin()?;
            pending = 0;
        }
        if stage == 1 && id % 10 == 1 {
            db.delete(id)?;
        } else {
            db.put(id, cycle)?;
        }
        pending += 1;
        total += 1;
    }
    db.commit()?;
    Ok(total)
}
fn record(out: &Path, v: &Value) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("events.jsonl"))?;
    writeln!(f, "{}", serde_json::to_string(v)?)?;
    f.sync_all()?;
    println!("{}", serde_json::to_string(v)?);
    std::io::stdout().flush()?;
    Ok(())
}
fn arm(root: &Path, n: u64, sqlite: bool) -> Result<Value> {
    let name = if sqlite { "sqlite" } else { "e4" };
    let dir = root.join(format!("{name}-{n}"));
    let mut db = Db::open(&dir, sqlite, true)?;
    let mut records = Vec::new();
    let t = Instant::now();
    mutate(&mut db, n, 0, 0)?;
    db.checkpoint(false)?;
    let load = t.elapsed().as_secs_f64();
    let v = json!({"arm":name,"n":n,"cycle":0,"stage":"load","seconds":load,"disk":footprint(&dir)?,"verify":db.verify(n,0,false)?,"structure":db.structure(&dir,n,false)?,"counters":db.counters()});
    record(root, &v)?;
    records.push(v);
    let mut snapshot = Some(db.snapshot(&dir)?);
    for cycle in 1..=6 {
        for stage in 1..=2 {
            let before = db.counters();
            let t = Instant::now();
            let operations = mutate(&mut db, n, cycle, stage)?;
            db.checkpoint(snapshot.is_some())?;
            let seconds = t.elapsed().as_secs_f64();
            let counters = db.counters();
            let io_delta = if sqlite {
                Value::Null
            } else {
                let mut delta = serde_json::Map::new();
                for name in ["data_read_calls", "data_write_calls", "data_write_bytes"] {
                    delta.insert(
                        name.into(),
                        json!(
                            counters["io"][name].as_u64().unwrap()
                                - before["io"][name].as_u64().unwrap()
                        ),
                    );
                }
                Value::Object(delta)
            };
            drop(db);
            let t = Instant::now();
            db = Db::open(&dir, sqlite, false)?;
            let reopen = t.elapsed().as_secs_f64();
            let verified = db.verify(n, cycle, stage == 1)?;
            let old = if let Some(r) = &snapshot {
                r.verify(n, 0, false)?
            } else {
                Value::Null
            };
            let structure = db.structure(&dir, n, stage == 1)?;
            let accounting = if let Db::E4(s) = &db {
                if cycle % 2 == 0 {
                    // Even cycles contain only small inline values. This exact
                    // accounting catches leaks hidden by spare snapshot space.
                    let physical = fs::metadata(dir.join("data"))?.len() / 4096;
                    let reachable = structure["reachable_pages"].as_u64().unwrap();
                    let free = s.pool_ref().free_pages_pending() as u64;
                    assert_eq!(
                        physical,
                        2 + reachable + free,
                        "untracked pages at {n} rows, cycle {cycle}, stage {stage}"
                    );
                    json!({"physical_pages":physical,"meta_pages":2,"reachable_tree_pages":reachable,"freelist_pages":free,"unaccounted_pages":0})
                } else {
                    Value::Null
                }
            } else {
                Value::Null
            };
            let v = json!({"arm":name,"n":n,"cycle":cycle,"stage":if stage==1 {"update_delete"}else{"reinsert"},"operations":operations,"seconds":seconds,"reopen_seconds":reopen,"pinned":snapshot.is_some(),"disk":footprint(&dir)?,"counters":counters,"mutation_data_io":io_delta,"accounting":accounting,"verify":verified,"snapshot_verify":old,"structure":structure});
            record(root, &v)?;
            records.push(v);
        }
        if cycle == 2 {
            drop(snapshot.take());
        }
    }
    drop(db);
    Ok(json!({"arm":name,"n":n,"stages":records,"final_disk":footprint(&dir)?}))
}
fn crash_child(dir: &Path, checkpoint: bool, policy: bool, pending: bool) -> Result<()> {
    let mut db = Db::open(dir, false, true)?;
    // Publish large values first, so the interrupted mutation retires old
    // overflow pages as well as changing ordinary leaf records.
    mutate(&mut db, 1000, 1, 0)?;
    db.checkpoint(false)?;
    if policy {
        for id in 1..=1000 {
            db.put(id, 2)?;
        }
        if let Db::E4(s) = &mut db {
            let threshold = if checkpoint { 1 } else { u64::MAX };
            assert_eq!(s.commit_with_checkpoint(threshold, threshold)?, checkpoint);
        }
    } else {
        mutate(&mut db, 1000, 2, 1)?;
        mutate(&mut db, 1000, 2, 2)?;
        if checkpoint {
            db.checkpoint(false)?;
        }
    }
    if pending {
        for id in 1..=1000 { db.put(id, 3)?; }
        if let Db::E4(s) = &db { s.pool_ref().flush_all(kernel::io::Barrier::None)?; }
    }
    println!("READY");
    std::io::stdout().flush()?;
    // Parent kills the process here: no destructor or clean database close.
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Err("child unexpectedly resumed".into())
}
fn crash_probe(root: &Path, checkpoint: bool, policy: bool, pending: bool) -> Result<Value> {
    let dir = root.join(if pending { "crash-pending" } else if checkpoint {
        "crash-checkpoint"
    } else {
        "crash-wal"
    });
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--crash-child")
        .arg(&dir)
        .arg(if checkpoint { "checkpoint" } else { "wal" })
        .arg(if policy { "policy" } else { "ordinary" })
        .arg(if pending { "pending" } else { "acknowledged" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().ok_or("child stdout")?).read_line(&mut line)?;
    if line.trim() != "READY" {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("child not ready: {line}").into());
    }
    child.kill()?;
    let status = child.wait()?;
    assert!(!status.success());
    let mut db = Db::open(&dir, false, false)?;
    let verified = db.verify(1000, 2, false)?;
    db.checkpoint(false)?;
    let v = json!({"resource_limits":benchmark_limits()?.is_some(),"case":if pending {"SIGKILL_during_uncommitted_flushed_pages"}else if checkpoint {"SIGKILL_after_checkpoint"}else{"SIGKILL_after_commit_before_checkpoint"},"policy":policy,"verify":verified,"structure":db.structure(&dir,1000,false)?});
    record(root, &v)?;
    Ok(v)
}
fn fingerprints(dir: &Path) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if !p.is_file() {
            continue;
        }
        let mut f = fs::File::open(&p)?;
        let mut buf = [0u8; 65536];
        let mut crc = 0;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc = crc32c::crc32c_append(crc, &buf[..n]);
        }
        out.insert(
            p.file_name().unwrap().to_string_lossy().into_owned(),
            json!({"bytes":p.metadata()?.len(),"crc32c":crc}),
        );
    }
    Ok(Value::Object(out))
}

fn immutable_sqlite(path: &Path) -> Result<Connection> {
    // Only for these closed, checkpointed fixtures: avoid creating WAL/SHM
    // files in the source during the maintenance measurement.
    Ok(Connection::open_with_flags(
        format!("file:{}?immutable=1", path.display()),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

// A maintenance measurement using existing source-preserving recovery and
// SQLite VACUUM INTO. It does not install a replacement over either source.
fn repack_probe(root: &Path, n: u64) -> Result<()> {
    if !artifact_root_allowed(&root) {
        return Err("data must stay on scratch".into());
    }
    std::env::set_var("TMPDIR", root.join("tmp"));
    std::env::set_var("SQLITE_TMPDIR", root.join("tmp"));
    let source = root.join(format!("e4-{n}"));
    let before = fingerprints(&source)?;
    let t = Instant::now();
    let r = kernel::recover::recover_to(&source, &root.join(format!("e4-rebuilt-{n}")), cfg())?;
    let seconds = t.elapsed().as_secs_f64();
    assert_eq!(r.entries_recovered, n + 3);
    assert_eq!(r.known_value_losses, 0);
    assert_eq!(r.unknown_extents, 0);
    assert_eq!(r.raw_candidate_records, 0);
    let db = Db::E4(Store::open_snapshot(&r.database, cfg())?);
    let verify = db.verify(n, 6, false)?;
    let structure = db.structure(&r.database, n, false)?;
    drop(db);
    assert_eq!(before, fingerprints(&source)?, "rebuild changed E4 source");
    let e4 = json!({"arm":"e4","seconds":seconds,"source_fingerprints":before,"source_unchanged":true,"source_disk":footprint(&source)?,"rebuilt_disk":footprint(&r.database)?,"verify":verify,"structure":structure,"known_value_losses":r.known_value_losses,"unknown_extents":r.unknown_extents});

    let source = root.join(format!("sqlite-{n}"));
    let before = fingerprints(&source)?;
    let target = root.join(format!("sqlite-rebuilt-{n}"));
    fs::create_dir(&target)?;
    let c = immutable_sqlite(&source.join("data.sqlite"))?;
    sql_config(&c)?;
    let t = Instant::now();
    c.execute(
        "VACUUM INTO ?",
        [target.join("data.sqlite").to_string_lossy().as_ref()],
    )?;
    let seconds = t.elapsed().as_secs_f64();
    drop(c);
    let c = immutable_sqlite(&target.join("data.sqlite"))?;
    sql_config(&c)?;
    let db = Db::Sql(c);
    let verify = db.verify(n, 6, false)?;
    let structure = db.structure(&target, n, false)?;
    drop(db);
    assert_eq!(
        before,
        fingerprints(&source)?,
        "rebuild changed SQLite source"
    );
    let sqlite = json!({"arm":"sqlite","seconds":seconds,"source_fingerprints":before,"source_unchanged":true,"source_disk":footprint(&source)?,"rebuilt_disk":footprint(&target)?,"verify":verify,"structure":structure});
    let result = json!({"n":n,"e4":e4,"sqlite":sqlite,"published_over_source":false});
    fs::write(
        root.join(format!("repack-{n}.json")),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn main() -> Result<()> {
    if !cfg!(all(feature = "sqlite-balance", feature = "compact-cells")) {
        return Err("enable sqlite-balance,compact-cells".into());
    }
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--verify-saved") {
        return sustained::verify_saved(&args[2..]);
    }
    if args.get(1).map(String::as_str) == Some("--sustained") {
        return sustained::run(&args[2..]);
    }
    if args.get(1).map(String::as_str) == Some("--space-check") {
        return space::run(&args[2..]);
    }
    if args.get(1).map(String::as_str) == Some("--crash-child") {
        return crash_child(
            Path::new(&args[2]),
            args[3] == "checkpoint",
            args.get(4).is_some_and(|a| a == "policy"),
            args.get(5).is_some_and(|a| a == "pending"),
        );
    }
    if args.get(1).map(String::as_str) == Some("--repack-check") {
        return repack_probe(
            Path::new(args.get(2).ok_or("missing run directory")?),
            args.get(3).ok_or("missing rows")?.parse()?,
        );
    }
    if matches!(
        args.get(1).map(String::as_str),
        Some("--crash-check" | "--policy-crash-check" | "--resource-crash-check")
    ) {
        let policy = args[1] == "--policy-crash-check";
        let root = Path::new(args.get(2).ok_or("missing crash run directory")?);
        if !artifact_root_allowed(&root) {
            return Err("data must stay on scratch".into());
        }
        fs::create_dir(root)?;
        fs::create_dir(root.join("tmp"))?;
        std::env::set_var("TMPDIR", root.join("tmp"));
        let mut crashes = vec![crash_probe(root,false,policy,false)?,crash_probe(root,true,policy,false)?];
        if args[1] == "--resource-crash-check" { crashes.push(crash_probe(root,false,false,true)?); }
        let results = json!({"overflow_retirement":true,"crashes":crashes,"complete":true});
        fs::write(
            root.join("results.json"),
            serde_json::to_vec_pretty(&results)?,
        )?;
        return Ok(());
    }
    let root = PathBuf::from(
        args.get(1)
            .ok_or("usage: lifecycle <scratch> [rows]")?,
    );
    if !artifact_root_allowed(&root) {
        return Err("data must stay on scratch".into());
    }
    fs::create_dir(&root)?;
    fs::create_dir(root.join("tmp"))?;
    std::env::set_var("TMPDIR", root.join("tmp"));
    std::env::set_var("SQLITE_TMPDIR", root.join("tmp"));
    let sizes = if let Some(n) = args.get(2) {
        vec![n.parse::<u64>()?]
    } else {
        vec![100_000, 400_000]
    };
    for &n in &sizes {
        let mut factor = n;
        while factor > 0 && factor % 2 == 0 {
            factor /= 2;
        }
        while factor > 0 && factor % 5 == 0 {
            factor /= 5;
        }
        if factor != 1 || n % 1000 != 0 {
            return Err("rows must be divisible by 1000 and have only factors 2 and 5".into());
        }
    }
    let mut results = json!({"sizes":sizes,"cycles":6,"writer_cache_bytes":8<<20,"snapshot_cache_bytes":64<<10,"commit_operations":1000,"timestamps":false,"sync":"FULL + macOS fullfsync","insertion_order":"fixed affine permutation, stride 7919, offset 1237","sqlite_version":rusqlite::version(),"arms":[],"crashes":[]});
    for n in sizes {
        for sqlite in [false, true] {
            results["arms"]
                .as_array_mut()
                .unwrap()
                .push(arm(&root, n, sqlite)?);
            fs::write(
                root.join("results.json"),
                serde_json::to_vec_pretty(&results)?,
            )?;
        }
    }
    for checkpoint in [false, true] {
        results["crashes"]
            .as_array_mut()
            .unwrap()
            .push(crash_probe(&root, checkpoint, false, false)?);
    }
    results["complete"] = json!(true);
    fs::write(
        root.join("results.json"),
        serde_json::to_vec_pretty(&results)?,
    )?;
    println!("COMPLETE {}", root.display());
    Ok(())
}
