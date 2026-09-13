//! Fixed-work raw-KV scaling harness. Oracle RAM is proportional to changes.
use e4_prototype::pagewal::PageWalStore;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, time::Instant};

type R<T> = Result<T, Box<dyn std::error::Error>>;
const CACHE: usize = 8 << 20;
const SIZE: usize = 256;
enum Db {
    E4(PageWalStore),
    Sql(Connection),
}
impl Db {
    fn open(path: &Path, engine: &str, create: bool) -> R<Self> {
        Ok(match engine {
            "pagewal" => Self::E4(PageWalStore::open(path, create, CACHE)?),
            "sqlite" => {
                if create {
                    fs::create_dir(path)?;
                }
                let c = Connection::open(path.join("data.sqlite"))?;
                c.execute_batch(
                    "PRAGMA page_size=4096; PRAGMA journal_mode=WAL;
                    PRAGMA synchronous=FULL; PRAGMA fullfsync=ON;
                    PRAGMA checkpoint_fullfsync=ON; PRAGMA wal_autocheckpoint=1000;
                    PRAGMA cache_size=-8192; PRAGMA mmap_size=0;",
                )?;
                if create {
                    c.execute_batch(
                        "CREATE TABLE kv(k BLOB PRIMARY KEY,v BLOB NOT NULL) WITHOUT ROWID;",
                    )?;
                }
                Self::Sql(c)
            }
            _ => return Err("unknown engine".into()),
        })
    }
    fn begin(&self) -> R<()> {
        if let Self::Sql(c) = self {
            c.execute_batch("BEGIN")?;
        }
        Ok(())
    }
    fn commit(&mut self) -> R<()> {
        match self {
            Self::E4(s) => s.commit()?,
            Self::Sql(c) => c.execute_batch("COMMIT")?,
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> R<()> {
        match self {
            Self::E4(s) => assert!(s.checkpoint()?),
            Self::Sql(c) => {
                let busy: i64 = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
                assert_eq!(busy, 0);
            }
        }
        Ok(())
    }
    fn put(&mut self, id: u64, value: &[u8]) -> R<()> {
        let key = id.to_be_bytes();
        match self {
            Self::E4(s) => s.put(&key, value)?,
            Self::Sql(c) => {
                c.prepare_cached(
                    "INSERT INTO kv VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                )?
                .execute(params![key.as_slice(), value])?;
            }
        }
        Ok(())
    }
    fn delete(&mut self, id: u64) -> R<()> {
        let key = id.to_be_bytes();
        match self {
            Self::E4(s) => assert!(s.delete(&key)?),
            Self::Sql(c) => assert_eq!(
                c.prepare_cached("DELETE FROM kv WHERE k=?1")?
                    .execute([key.as_slice()])?,
                1
            ),
        }
        Ok(())
    }
    fn get(&self, id: u64) -> R<Option<Vec<u8>>> {
        let key = id.to_be_bytes();
        Ok(match self {
            Self::E4(s) => s.get(&key)?,
            Self::Sql(c) => c
                .query_row("SELECT v FROM kv WHERE k=?1", [key.as_slice()], |r| {
                    r.get(0)
                })
                .optional()?,
        })
    }
    fn scan(&self, mut f: impl FnMut(&[u8], &[u8])) -> R<()> {
        match self {
            Self::E4(s) => s.scan(|k, v| {
                f(k, v);
                true
            })?,
            Self::Sql(c) => {
                let mut q = c.prepare("SELECT k,v FROM kv ORDER BY k")?;
                let mut rows = q.query([])?;
                while let Some(row) = rows.next()? {
                    f(row.get_ref(0)?.as_blob()?, row.get_ref(1)?.as_blob()?);
                }
            }
        }
        Ok(())
    }
    fn stats(&self) -> R<Value> {
        Ok(match self {
            Self::E4(s) => {
                let [data, wal] = s.take_file_io_stats().ok_or("missing FileIo counters")?;
                let item = |v: (u64, u64, u64)| json!({"write_calls":v.0,"issued_write_bytes":v.1,"read_calls":v.2});
                json!({"kind":"buffered FileIo calls; not physical media I/O", "data":item(data),"wal":item(wal),
                    "total_read_calls":data.2+wal.2,"total_write_calls":data.0+wal.0,"issued_write_bytes":data.1+wal.1})
            }
            Self::Sql(c) => {
                let mut out = serde_json::Map::new();
                for (name, op) in [
                    ("cache_hits", rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT),
                    ("cache_misses", rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS),
                    ("cache_writes", rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE),
                ] {
                    let (mut current, mut high) = (0, 0);
                    // Handle is borrowed only during this synchronous call; no other thread uses it.
                    let rc = unsafe {
                        rusqlite::ffi::sqlite3_db_status(c.handle(), op, &mut current, &mut high, 1)
                    };
                    if rc != rusqlite::ffi::SQLITE_OK {
                        return Err("sqlite db_status failed".into());
                    }
                    out.insert(name.into(), json!(current));
                }
                out.insert("kind".into(),json!("SQLite cache events; excludes checkpoint VFS work; not comparable to E4 FileIo totals"));
                Value::Object(out)
            }
        })
    }
}
fn payload(id: u64, version: u64) -> [u8; SIZE] {
    let mut b = [b'a' + ((id + version) % 26) as u8; SIZE];
    b[..8].copy_from_slice(&id.to_le_bytes());
    b[8..16].copy_from_slice(&version.to_le_bytes());
    b
}
fn disk(path: &Path) -> R<Value> {
    use std::os::unix::fs::MetadataExt;
    let (mut logical, mut allocated) = (0, 0);
    let mut files = BTreeMap::new();
    for e in fs::read_dir(path)? {
        let e = e?;
        let m = e.metadata()?;
        assert!(m.is_file());
        logical += m.len();
        allocated += m.blocks() * 512;
        files.insert(
            e.file_name().to_string_lossy().into_owned(),
            json!({"logical":m.len(),"allocated":m.blocks()*512}),
        );
    }
    Ok(json!({"logical":logical,"allocated":allocated,"files":files}))
}
fn ids(n: u64, changes: u64, locality: &str, op: &str) -> Vec<u64> {
    // 619 is coprime to the supported 1000 and 100 change counts. The permutation
    // scatters execution order without keeping a database-sized shuffled array.
    (0..changes)
        .map(|j| {
            let i = if locality == "scattered" {
                (j * 619 + 37) % changes
            } else {
                j
            };
            match (locality, op) {
                ("local", "insert") => 2 * (n + i),
                ("local", "update") => 2 * i,
                ("local", "delete") => 2 * (changes + i),
                ("scattered", "insert") => 2 * (i * n / changes) + 1,
                ("scattered", "update") => 2 * (i * n / changes),
                ("scattered", "delete") => 2 * (i * n / changes + 1),
                _ => unreachable!(),
            }
        })
        .collect()
}
fn verify(db: &Db, n: u64, model: &BTreeMap<u64, Option<u64>>) -> R<Value> {
    let (mut count, mut crc, mut previous) = (0u64, 0, None);
    db.scan(|k, v| {
        let id = u64::from_be_bytes(k.try_into().expect("8-byte key"));
        assert!(previous.is_none_or(|p| id > p));
        previous = Some(id);
        let version = match model.get(&id) {
            Some(v) => v.expect("deleted row survived"),
            None => {
                assert!(id % 2 == 0 && id / 2 < n, "unexpected key {id}");
                0
            }
        };
        assert_eq!(v, payload(id, version), "wrong value for {id}");
        crc = crc32c::crc32c_append(crc, k);
        crc = crc32c::crc32c_append(crc, v);
        count += 1;
    })?;
    let extra = model
        .iter()
        .filter(|(id, v)| v.is_some() && (**id % 2 != 0 || **id / 2 >= n))
        .count() as u64;
    let deleted = model.values().filter(|v| v.is_none()).count() as u64;
    assert_eq!(count, n + extra - deleted);
    Ok(
        json!({"rows":count,"crc32c":crc,"oracle_entries":model.len(),"exact_values_order_and_membership":true}),
    )
}
fn main() -> R<()> {
    let a: Vec<_> = std::env::args().collect();
    if a.len() != 6 {
        return Err("OUTPUT pagewal|sqlite ROWS local|scattered CHANGES(100|1000)".into());
    }
    let root = Path::new(&a[1]);
    let engine = &a[2];
    let n: u64 = a[3].parse()?;
    let locality = &a[4];
    let changes: u64 = a[5].parse()?;
    assert!(matches!(changes, 100 | 1000) && n >= changes * 10 && n % changes == 0);
    assert!(matches!(locality.as_str(), "local" | "scattered"));
    fs::create_dir(root)?;
    let path = root.join("db");
    let mut db = Db::open(&path, engine, true)?;
    db.stats()?;
    let start = Instant::now();
    for first in (0..n).step_by(1000) {
        db.begin()?;
        for slot in first..(first + 1000).min(n) {
            db.put(2 * slot, &payload(2 * slot, 0))?;
        }
        db.commit()?;
    }
    db.checkpoint()?;
    let load =
        json!({"seconds":start.elapsed().as_secs_f64(),"io":db.stats()?,"disk":disk(&path)?});
    println!("load {load}");
    drop(db);
    let mut model = BTreeMap::new();
    let mut phases = Vec::new();
    for op in ["insert", "update", "delete"] {
        // Precompute only O(changes) inputs. Reopen each phase: empty engine
        // cache, ordinary OS cache retained. Reopen time is separately priced.
        let keys = ids(n, changes, locality, op);
        let version = if op == "insert" { 1 } else { 2 };
        let values: Vec<_> = keys.iter().map(|id| payload(*id, version)).collect();
        let opened = Instant::now();
        let mut db = Db::open(&path, engine, false)?;
        let reopen_seconds = opened.elapsed().as_secs_f64();
        db.stats()?;
        let start = Instant::now();
        db.begin()?;
        for (id, v) in keys.iter().zip(&values) {
            if op == "delete" {
                db.delete(*id)?;
            } else {
                db.put(*id, v)?;
            }
        }
        let writes_seconds = start.elapsed().as_secs_f64();
        let t = Instant::now();
        db.commit()?;
        let commit_seconds = t.elapsed().as_secs_f64();
        let t = Instant::now();
        db.checkpoint()?;
        let checkpoint_seconds = t.elapsed().as_secs_f64();
        let seconds = start.elapsed().as_secs_f64();
        let io = db.stats()?;
        for id in keys {
            let expected = if op == "delete" { None } else { Some(version) };
            assert_eq!(db.get(id)?, expected.map(|v| payload(id, v).to_vec()));
            model.insert(id, expected);
        }
        let phase = json!({"operation":op,"changes":changes,"seconds":seconds,"writes_seconds":writes_seconds,
            "commit_seconds":commit_seconds,"checkpoint_seconds":checkpoint_seconds,
            "reopen_seconds":reopen_seconds,"io":io,"disk":disk(&path)?,"changed_point_checks":changes});
        println!("phase {phase}");
        phases.push(phase);
        drop(db);
    }
    let t = Instant::now();
    let db = Db::open(&path, engine, false)?;
    let final_reopen_seconds = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let oracle = verify(&db, n, &model)?;
    let verification_seconds = t.elapsed().as_secs_f64();
    let report = json!({"version":"fixed-work-v1","engine":engine,"rows":n,"changes_per_phase":changes,
        "locality":locality,"payload_bytes":SIZE,"key_bytes":8,"cache_bytes":CACHE,"load_batch":1000,
        "mutation_batch":changes,"sync":"native FULL; SQLite fullfsync and checkpoint_fullfsync ON",
        "layer":"raw KV; no typed collection or secondary indexes","cache_state":"fresh engine cache each mutation phase; OS caches not flushed",
        "size_sampling":"phase boundaries only; NOT peak or cap evidence","sqlite_version":rusqlite::version(),
        "load":load,"phases":phases,"final_reopen_seconds":final_reopen_seconds,"verification_seconds":verification_seconds,
        "verification":oracle,"final_disk":disk(&path)?});
    fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
