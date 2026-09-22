//! Full collection API comparison. Run one arm at a time on scratch.
use sekejap_core::{
    collections::{Clock, CollectionId, CollectionOptions, Database},
    Kind, Result,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        // `sekejap::Db::open`'s default barrier (SQLite/PostgreSQL parity).
        sync: SyncMode::Normal,
    }
}
struct FixedClock;
fn vector_dim() -> usize {
    static DIM: OnceLock<usize> = OnceLock::new();
    *DIM.get_or_init(|| {
        let n = std::env::var("COLLECTION_VECTOR_DIM")
            .map_or(4, |s| s.parse().expect("vector dimension"));
        assert!((1..=16384).contains(&n));
        n
    })
}
fn change_vectors() -> bool {
    static CHANGE: OnceLock<bool> = OnceLock::new();
    *CHANGE.get_or_init(|| std::env::var("COLLECTION_CHANGE_VECTORS").is_ok_and(|s| s == "1"))
}
impl Clock for FixedClock {
    fn unix_seconds(&self) -> i64 {
        1_788_888_888
    }
}
fn fields() -> Vec<(String, Kind)> {
    vec![
        ("name".into(), Kind::Text),
        ("active".into(), Kind::Bool),
        ("income".into(), Kind::Real),
        ("point".into(), Kind::Point),
        ("profile".into(), Kind::Json),
        ("vector".into(), Kind::Vector(vector_dim())),
    ]
}
fn key(i: u64) -> String {
    format!("person/{i:08}")
}
fn doc(i: u64, cycle: u64, times: bool, case: &str) -> Value {
    let v = if (i % 5 == 0 && case != "reinsert" && case != "load")
        || (i % 10 == 1 && case != "updates" && case != "load")
    {
        cycle
    } else {
        0
    };
    let len = if v % 2 == 1 && i % 100 == 0 { 1024 } else { 24 };
    let mut d = json!({"name":format!("Person {i:08} 東京-é"),"active":(i+v)%3!=0,"income":if i%7==0 {Value::Null}else{json!(25000.25+(i%1000) as f64)},"point":{"type":"Point","coordinates":[(i%300) as f64-150.0,(i%160) as f64-80.0]},"profile":{"revision":v,"notes":"x".repeat(len),"flags":[true,null,"sensor"]},"vector":[0.25,0.5,(i%100) as f64,-1.0],"extra":{"source":"sensor","unsigned":u64::MAX}});
    if vector_dim() != 4 || change_vectors() {
        let base = [0.25, 0.5, (i % 100) as f64, -1.0];
        d["vector"] = Value::Array(
            (0..vector_dim())
                .map(|j| {
                    json!(
                        base[j % 4]
                            + if change_vectors() {
                                v as f64 * 0.125
                            } else {
                                0.0
                            }
                    )
                })
                .collect(),
        );
    }
    if times {
        d["_created_unix"] = json!(1_788_888_888);
        d["_updated_unix"] = json!(1_788_888_888);
    }
    d
}
fn disk(dir: &Path) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let mut total = (0, 0);
    for e in fs::read_dir(dir)? {
        let e = e?;
        let m = match e.metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
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
enum Backend {
    E4(Database),
    Sql(Connection),
}
struct Db {
    inner: Backend,
    next: [u64; 2],
    times: bool,
    case: String,
}
impl Db {
    fn create(path: &Path, sql: bool, times: bool, case: &str) -> Result<Self> {
        let inner = if sql {
            fs::create_dir(path)?;
            let c = Connection::open(path.join("data.sqlite"))?;
            c.execute_batch("PRAGMA page_size=4096;PRAGMA journal_mode=WAL;PRAGMA cache_size=-8192;PRAGMA mmap_size=0;PRAGMA synchronous=FULL;PRAGMA fullfsync=ON;PRAGMA checkpoint_fullfsync=ON;PRAGMA wal_autocheckpoint=1000;
CREATE TABLE collections(cid INTEGER PRIMARY KEY,name TEXT NOT NULL UNIQUE,layout INTEGER NOT NULL,timestamps INTEGER NOT NULL,next_sequence INTEGER NOT NULL);
CREATE TABLE layouts(id INTEGER PRIMARY KEY,fields TEXT NOT NULL);")?;
            c.execute_batch(&format!("CREATE TABLE people(cid INTEGER NOT NULL,seq INTEGER NOT NULL,external_key TEXT NOT NULL,name TEXT,active INTEGER,income REAL,lon REAL,lat REAL,profile BLOB,vector BLOB,extra BLOB{},PRIMARY KEY(cid,seq),UNIQUE(cid,external_key)) WITHOUT ROWID;",if times {",created INTEGER,updated INTEGER"}else{""}))?;
            c.execute_batch("BEGIN")?;
            for i in 1..=2 {
                c.execute(
                    "INSERT INTO layouts VALUES(?1,?2)",
                    params![i, format!("{:?}", fields())],
                )?;
                c.execute(
                    "INSERT INTO collections VALUES(?1,?2,?1,?3,1)",
                    params![i, format!("people{i}"), times],
                )?;
            }
            c.execute_batch("COMMIT;PRAGMA wal_checkpoint(TRUNCATE)")?;
            Backend::Sql(c)
        } else {
            let mut d = Database::create(path, cfg())?;
            d.set_clock(Arc::new(FixedClock));
            for i in 1..=2 {
                assert_eq!(
                    d.create_collection(
                        &format!("people{i}"),
                        fields(),
                        CollectionOptions { timestamps: times }
                    )?
                    .0,
                    i
                );
            }
            d.commit()?;
            Backend::E4(d)
        };
        Ok(Self {
            inner,
            next: [1, 1],
            times,
            case: case.into(),
        })
    }
    fn begin(&mut self) -> Result<()> {
        if let Backend::Sql(c) = &self.inner {
            c.execute_batch("BEGIN")?;
        }
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        match &mut self.inner {
            Backend::E4(d) => d.commit()?,
            Backend::Sql(c) => {
                for i in 1..=2 {
                    c.execute(
                        "UPDATE collections SET next_sequence=?1 WHERE cid=?2",
                        params![self.next[i - 1], i],
                    )?;
                }
                c.execute_batch("COMMIT")?;
            }
        }
        Ok(())
    }
    fn snapshot(&self, path: &Path) -> Result<Self> {
        let inner = match &self.inner {
            Backend::E4(_) => Backend::E4(Database::open_snapshot(path, cfg())?),
            Backend::Sql(_) => {
                let c = Connection::open_with_flags(
                    path.join("data.sqlite"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                c.execute_batch("PRAGMA cache_size=-8192;PRAGMA mmap_size=0;BEGIN")?;
                c.query_row("SELECT seq FROM people LIMIT 1", [], |r| r.get::<_, u64>(0))?;
                Backend::Sql(c)
            }
        };
        Ok(Self {
            inner,
            next: self.next,
            times: self.times,
            case: self.case.clone(),
        })
    }
    fn checkpoint(&mut self) -> Result<Value> {
        if let Backend::Sql(c) = &self.inner {
            let (busy, log, done) = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            Ok(json!({"busy":busy,"wal_frames":log,"checkpointed_frames":done}))
        } else {
            Ok(json!({"published_by_commit":true}))
        }
    }
    fn put(&mut self, cid: u32, i: u64, cycle: u64) -> Result<()> {
        let key = key(i);
        let d = doc(i, cycle, false, &self.case);
        match &mut self.inner {
            Backend::E4(db) => {
                db.put(CollectionId(cid), &key, &d)?;
            }
            Backend::Sql(c) => {
                let old = c
                    .prepare_cached("SELECT seq FROM people WHERE cid=?1 AND external_key=?2")?
                    .query_row(params![cid, key], |r| r.get::<_, u64>(0))
                    .optional()?;
                let seq = old.unwrap_or(self.next[cid as usize - 1]);
                if old.is_none() {
                    self.next[cid as usize - 1] += 1;
                }
                let vector: Vec<u8> = d["vector"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|v| (v.as_f64().unwrap() as f32).to_le_bytes())
                    .collect();
                let query = if self.times {
                    "INSERT INTO people VALUES(?1,?2,?3,?4,?5,?6,?7,?8,jsonb(?9),?10,jsonb(?11),1788888888,1788888888) ON CONFLICT(cid,seq) DO UPDATE SET name=excluded.name,active=excluded.active,income=excluded.income,lon=excluded.lon,lat=excluded.lat,profile=excluded.profile,vector=excluded.vector,extra=excluded.extra,updated=excluded.updated"
                } else {
                    "INSERT INTO people VALUES(?1,?2,?3,?4,?5,?6,?7,?8,jsonb(?9),?10,jsonb(?11)) ON CONFLICT(cid,seq) DO UPDATE SET name=excluded.name,active=excluded.active,income=excluded.income,lon=excluded.lon,lat=excluded.lat,profile=excluded.profile,vector=excluded.vector,extra=excluded.extra"
                };
                c.prepare_cached(query)?.execute(params![
                    cid,
                    seq,
                    key,
                    d["name"].as_str(),
                    d["active"].as_bool(),
                    d["income"].as_f64(),
                    d["point"]["coordinates"][0].as_f64(),
                    d["point"]["coordinates"][1].as_f64(),
                    serde_json::to_string(&d["profile"])?,
                    vector,
                    serde_json::to_string(&d["extra"])?
                ])?;
            }
        }
        Ok(())
    }
    fn delete(&mut self, cid: u32, i: u64) -> Result<()> {
        match &mut self.inner {
            Backend::E4(d) => assert!(d.delete(CollectionId(cid), &key(i))?),
            Backend::Sql(c) => assert_eq!(
                c.prepare_cached("DELETE FROM people WHERE cid=?1 AND external_key=?2")?
                    .execute(params![cid, key(i)])?,
                1
            ),
        }
        Ok(())
    }
    fn alter(&mut self) -> Result<()> {
        let mut f = fields();
        f.push(("future".into(), Kind::Bool));
        match &mut self.inner {
            Backend::E4(d) => {
                for cid in 1..=2 {
                    d.alter_collection(CollectionId(cid), f.clone())?;
                }
            }
            Backend::Sql(c) => {
                for cid in 1..=2 {
                    c.execute(
                        "INSERT INTO layouts VALUES(?1,?2)",
                        params![cid + 2, format!("{f:?}")],
                    )?;
                    c.execute(
                        "UPDATE collections SET layout=?1 WHERE cid=?2",
                        params![cid + 2, cid],
                    )?;
                }
            }
        }
        Ok(())
    }
    fn verify(&self, n: u64, cycle: u64) -> Result<Value> {
        let started = Instant::now();
        let mut count = 0;
        let mut crc = 0u32;
        let mut last = (0u32, 0u64);
        let mut check = |cid: u32, seq: u64, key: String, d: Value| -> Result<()> {
            let i: u64 = key.strip_prefix("person/").ok_or("external key")?.parse()?;
            assert!(i < n);
            assert_eq!(cid, 1 + (i / (n / 2)) as u32);
            assert_eq!(key, crate::key(i));
            assert_eq!(d, doc(i, cycle, self.times, &self.case));
            assert!((cid, seq) > last);
            last = (cid, seq);
            crc = crc32c::crc32c_append(crc, &cid.to_le_bytes());
            crc = crc32c::crc32c_append(crc, &seq.to_le_bytes());
            crc = crc32c::crc32c_append(crc, key.as_bytes());
            crc = crc32c::crc32c_append(crc, &serde_json::to_vec(&d)?);
            count += 1;
            Ok(())
        };
        match &self.inner {
            Backend::E4(db) => {
                for cid in 1..=2 {
                    for row in db.scan(CollectionId(cid), None)? {
                        let r = row?;
                        check(cid, r.id.sequence, r.key, r.document)?;
                    }
                }
            }
            Backend::Sql(c) => {
                assert_eq!(
                    c.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
                    "ok"
                );
                let query=format!("SELECT cid,seq,external_key,name,active,income,lon,lat,json(profile),vector,json(extra){} FROM people ORDER BY cid,seq",if self.times {",created,updated"}else{""});
                let mut s = c.prepare(&query)?;
                let mut rows = s.query([])?;
                while let Some(r) = rows.next()? {
                    let vector: Vec<u8> = r.get(9)?;
                    assert_eq!(vector.len(), vector_dim() * 4);
                    let vector: Vec<f64> = vector
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap()) as f64)
                        .collect();
                    let mut d = json!({"name":r.get::<_,String>(3)?,"active":r.get::<_,bool>(4)?,"income":r.get::<_,Option<f64>>(5)?,"point":{"type":"Point","coordinates":[r.get::<_,f64>(6)?,r.get::<_,f64>(7)?]},"profile":serde_json::from_str::<Value>(&r.get::<_,String>(8)?)?,"vector":vector,"extra":serde_json::from_str::<Value>(&r.get::<_,String>(10)?)?});
                    if self.times {
                        d["_created_unix"] = json!(r.get::<_, i64>(11)?);
                        d["_updated_unix"] = json!(r.get::<_, i64>(12)?);
                    }
                    check(r.get(0)?, r.get(1)?, r.get(2)?, d)?;
                }
            }
        }
        assert_eq!(count, n);
        Ok(json!({"rows":count,"crc32c":crc,"verify_seconds":started.elapsed().as_secs_f64()}))
    }
    fn reopen(&mut self, path: &Path) -> Result<()> {
        match &mut self.inner {
            Backend::E4(db) => {
                db.rollback()?;
            }
            Backend::Sql(c) => {
                let fresh = Connection::open(path.join("data.sqlite"))?;
                fresh.execute_batch("PRAGMA cache_size=-8192;PRAGMA mmap_size=0;PRAGMA synchronous=FULL;PRAGMA fullfsync=ON;PRAGMA checkpoint_fullfsync=ON;PRAGMA wal_autocheckpoint=1000;")?;
                *c = fresh;
            }
        }
        Ok(())
    }
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if !(5..=8).contains(&args.len()) {
        return Err("usage: collections ROOT e4|sqlite ROWS off|on [CYCLES] [load|updates|reinsert|mixed] [none|held|rolling]".into());
    }
    let root = PathBuf::from(&args[1]);
    if !(root.starts_with("<scratch>")
        || root.starts_with("<scratch>"))
        || root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("scratch artifact root required".into());
    }
    let sql = match args[2].as_str() {
        "e4" => false,
        "sqlite" => true,
        _ => return Err("engine".into()),
    };
    let n: u64 = args[3].parse()?;
    if n == 0 || n % 1000 != 0 {
        return Err("rows must be a multiple of 1000".into());
    }
    let times = match args[4].as_str() {
        "on" => true,
        "off" => false,
        _ => return Err("timestamps".into()),
    };
    let cycles = args
        .get(5)
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(3);
    let case = args.get(6).map_or("mixed", String::as_str);
    let reader = args.get(7).map_or("none", String::as_str);
    if !matches!(case, "load" | "updates" | "reinsert" | "mixed")
        || !matches!(reader, "none" | "held" | "rolling")
    {
        return Err("unknown case or reader".into());
    }
    if case == "load" && cycles != 0 {
        return Err("load-only requires zero cycles".into());
    }
    let batch: u64 = std::env::var("COLLECTION_BATCH").unwrap_or_else(|_| "1000".into()).parse()?;
    if batch == 0 { return Err("batch must be positive".into()); }
    fs::create_dir_all(&root)?;
    let path = root.join(format!("{}-{n}-{}", args[2], args[4]));
    if path.exists() {
        return Err("arm already exists".into());
    }
    let mut db = Db::create(&path, sql, times, case)?;
    let monitor = Monitor::start(&path);
    let empty = disk(&path)?;
    let mut phases = Vec::new();
    let mut snapshot: Option<(Db, u64)> = None;
    for cycle in 0..=cycles {
        monitor.take();
        if !sql {
            kernel::write_stats::reset();
        }
        let t = Instant::now();
        let mut ops = 0;
        db.begin()?;
        for i in 0..n {
            let cid = 1 + (i / (n / 2)) as u32;
            if cycle == 0 || (i % 5 == 0 && case != "reinsert") {
                db.put(cid, i, cycle)?;
                ops += 1;
            } else if i % 10 == 1 && case != "updates" {
                db.delete(cid, i)?;
                ops += 1;
                if ops % batch == 0 {
                    db.commit()?;
                    db.begin()?;
                }
                db.put(cid, i, cycle)?;
                ops += 1;
            } else {
                continue;
            }
            if ops % batch == 0 {
                db.commit()?;
                db.begin()?;
            }
        }
        db.commit()?;
        let checkpoint = db.checkpoint()?;
        let seconds = t.elapsed().as_secs_f64();
        let issued = if !sql {
            let b = kernel::write_stats::take();
            json!({"data":b.final_pages,"wal":b.wal,"sidecars":b.sidecars,"total":b.total()})
        } else {
            Value::Null
        };
        let peak = monitor.take();
        let final_size = disk(&path)?;
        let verify = db.verify(n, cycle)?;
        let old = if let Some((old, version)) = &snapshot {
            Some(old.verify(n, *version)?)
        } else {
            None
        };
        let phase = json!({"issued_bytes":issued,"checkpoint":checkpoint,"snapshot_verification":old,"cycle":cycle,"operations":ops,"seconds":seconds,"peak":peak,"final_logical":final_size.0,"final_allocated":final_size.1,"verification":verify});
        println!("{phase}");
        phases.push(phase);
        if cycle < cycles && (reader == "rolling" || (reader == "held" && cycle == 0)) {
            drop(snapshot.take());
            snapshot = Some((db.snapshot(&path)?, cycle));
        }
    }
    monitor.take();
    let release = if snapshot.is_some() {
        let started = Instant::now();
        drop(snapshot.take());
        db.begin()?;
        db.commit()?;
        let checkpoint = db.checkpoint()?;
        Some(
            json!({"seconds":started.elapsed().as_secs_f64(),"checkpoint":checkpoint,"peak":monitor.take(),"final_bytes":disk(&path)?}),
        )
    } else {
        None
    };
    monitor.take();
    let before = db.verify(n, cycles)?;
    let t = Instant::now();
    db.begin()?;
    db.alter()?;
    db.commit()?;
    db.checkpoint()?;
    let alter_seconds = t.elapsed().as_secs_f64();
    let alter_peak = monitor.take();
    let alter_size = disk(&path)?;
    let t = Instant::now();
    db.reopen(&path)?;
    let reopen_seconds = t.elapsed().as_secs_f64();
    let after = db.verify(n, cycles)?;
    assert_eq!(before["crc32c"], after["crc32c"]);
    let report = json!({"case":case,"reader":reader,"reader_release":release,"platform":std::env::consts::OS,"cycles":cycles,"engine":args[2],"rows":n,"timestamps":times,"sqlite_version":rusqlite::version(),"collections":2,"cache_bytes":8<<20,"transaction_operations":1000,"sync":"FULL, platform-native barrier; fullfsync on macOS","vectors":"four exact f32 lanes; E4 separate keyspace, SQLite inline BLOB","sqlite_layout":"WITHOUT ROWID composite primary key and UNIQUE external key; JSONB","peak_semantics":"1ms samples, lower bound, not an enforced cap","empty_logical":empty.0,"empty_allocated":empty.1,"phases":phases,"alter_seconds":alter_seconds,"alter_peak":alter_peak,"alter_final_logical":alter_size.0,"alter_final_allocated":alter_size.1,"reopen_seconds":reopen_seconds,"reopen_verification":after});
    let mut report = report;
    report["transaction_operations"] = json!(batch);
    report["vector_dim"] = json!(vector_dim());
    report["change_vectors"] = json!(change_vectors());
    report["vectors"] = json!(format!(
        "{} exact f32 lanes; E4 separate keyspace, SQLite inline BLOB",
        vector_dim()
    ));
    fs::write(
        root.join(format!("{}-{n}-{}.json", args[2], args[4])),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
