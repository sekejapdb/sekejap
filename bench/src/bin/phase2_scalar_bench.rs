//! Paired scalar/index lifecycle measurements. Run one engine per process on
//! the same Linux storage. No claim about graph/vector/spatial/text indexes.
use sekejap_core::{
    collections::{CollectionOptions, Database, ScalarPredicate},
    pagewal::create_compact_cells,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{path::Path, time::Instant};
type R<T> = Result<T, Box<dyn std::error::Error>>;
fn key(i: usize) -> String {
    format!("person/{i}")
}
fn age(i: usize, round: usize) -> usize {
    18 + (i + round) % 80
}
fn doc(i: usize, round: usize) -> Value {
    json!({"age":age(i,round),"name":format!("Person {i:08}"),"active":i%3!=0})
}
fn sizes(root: &Path) -> (u64, u64) {
    fn visit(root: &Path, totals: &mut (u64, u64)) {
        for e in std::fs::read_dir(root).unwrap() {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            if m.is_dir() {
                visit(&e.path(), totals);
            } else if m.is_file() {
                totals.0 += m.len();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    totals.1 += m.blocks() * 512;
                }
                #[cfg(not(unix))]
                {
                    totals.1 += m.len();
                }
            }
        }
    }
    let mut totals = (0, 0);
    visit(root, &mut totals);
    totals
}
fn hwm() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .unwrap_or("unavailable")
        .to_owned()
}
fn sample(root: &Path, peaks: &mut (u64, u64)) {
    let s = sizes(root);
    peaks.0 = peaks.0.max(s.0);
    peaks.1 = peaks.1.max(s.1);
}
fn expected(n: usize, round: usize, lo: usize, hi: usize) -> Vec<usize> {
    let mut rows: Vec<_> = (0..n)
        .filter(|&i| (lo..=hi).contains(&age(i, round)))
        .collect();
    rows.sort_unstable_by_key(|&i| (age(i, round), round > 0 && i % 10 == 0, i));
    rows
}
fn verify(actual: Vec<usize>, n: usize, round: usize, lo: usize, hi: usize) {
    assert_eq!(actual, expected(n, round, lo, hi));
}
fn main() -> R<()> {
    let a: Vec<_> = std::env::args().collect();
    if !(4..=5).contains(&a.len()) {
        return Err(
            "usage: phase2_scalar_bench e4|sqlite N FRESH_DIRECTORY [resumable|atomic]".into(),
        );
    }
    let engine = &a[1];
    let late_index_mode = match (engine.as_str(), a.get(4).map(String::as_str)) {
        ("e4", None | Some("resumable")) => "resumable",
        ("e4", Some("atomic")) => "atomic",
        ("sqlite", None | Some("atomic")) => "atomic",
        ("sqlite", Some("resumable")) => {
            return Err("SQLite CREATE INDEX has one atomic publication; use atomic".into())
        }
        ("e4" | "sqlite", Some(_)) => {
            return Err("late-index mode must be resumable or atomic".into())
        }
        _ => return Err("unknown engine".into()),
    };
    let n: usize = a[2].parse()?;
    if n == 0 || n > 1_000_000 {
        return Err("N must be 1..1000000".into());
    }
    let root = Path::new(&a[3]);
    if root.exists() {
        return Err("benchmark directory must be fresh".into());
    }
    if engine == "sqlite" {
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        std::env::set_var("SQLITE_TMPDIR", &tmp);
    }
    let late_index_publication_policy = match (engine.as_str(), late_index_mode) {
        ("e4", "resumable") => "commit each bounded 256-row build step",
        ("e4", "atomic") => "one final commit after all bounded 256-row build steps",
        ("sqlite", "atomic") => "one CREATE INDEX transaction",
        _ => unreachable!(),
    };
    let mut peak = (0, 0);
    let mut report = json!({"engine":engine,"rows":n,"cache_bytes":8<<20,"commit_batch":256,"durability":"WAL FULL","disk_scope":"all regular files recursively under the dedicated root","peak_sampling":"after each commit and phase; discrete samples and unlinked SQLite temp files remain lower bounds","columns":"external key, age i64, name text, active bool","index":"age nonunique, plus unique external key","late_index_mode":late_index_mode,"late_index_publication_policy":late_index_publication_policy,"late_index_setup":"fresh post-load database; no prior age index or drop","oracle":"independent generated external keys in exact age then entity-ID order","e4_create_compact_cells":create_compact_cells(),"compile_features":{"compact-cells":cfg!(feature="compact-cells"),"sqlite-balance":cfg!(feature="sqlite-balance"),"keyspace-append":cfg!(feature="keyspace-append"),"slotref-split":cfg!(feature="slotref-split")}});
    if engine == "e4" {
        // Database owns this empty dedicated directory.
        let cfg = || Config {
            budget_bytes: 8 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        };
        let mut db = Database::create(root, cfg())?;
        report["runtime_settings"] = json!({
            "cache_budget_bytes": 8 << 20,
            "io_mode": "buffered",
            "journal_mode": "page-WAL",
            "sync_mode": "FULL"
        });
        let c = db.create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("name".into(), Kind::Text),
                ("active".into(), Kind::Bool),
            ],
            CollectionOptions::default(),
        )?;
        db.commit()?;
        let start = Instant::now();
        for i in 0..n {
            db.put(c, &key(i), &doc(i, 0))?;
            if (i + 1) % 256 == 0 {
                db.commit()?;
                sample(root, &mut peak);
            }
        }
        db.commit()?;
        report["load_s"] = json!(start.elapsed().as_secs_f64());
        db.checkpoint()?;
        report["loaded_bytes"] = json!(sizes(root));
        sample(root, &mut peak);
        let mut late_index_peak = sizes(root);
        let start = Instant::now();
        let idx = db.create_scalar_index(c, "age_idx", "age", false)?;
        let mut build_steps = 0;
        let mut commit_count = 0;
        loop {
            let ready = db.build_index_step(idx, 256)?;
            build_steps += 1;
            if late_index_mode == "resumable" {
                db.commit()?;
                commit_count += 1;
                sample(root, &mut peak);
                sample(root, &mut late_index_peak);
            }
            if ready {
                break;
            }
        }
        if late_index_mode == "atomic" {
            db.commit()?;
            commit_count += 1;
            sample(root, &mut peak);
            sample(root, &mut late_index_peak);
        }
        report["late_index_s"] = json!(start.elapsed().as_secs_f64());
        report["late_index_build_batch_rows"] = json!(256);
        report["late_index_build_steps"] = json!(build_steps);
        report["late_index_commit_count"] = json!(commit_count);
        db.checkpoint()?;
        report["indexed_bytes"] = json!(sizes(root));
        sample(root, &mut peak);
        sample(root, &mut late_index_peak);
        report["late_index_sampled_peak_bytes"] = json!(late_index_peak);
        let mut queries = Vec::new();
        for (lo, hi) in [(38, 38), (47, 49), (90, 92)] {
            let start = Instant::now();
            let ids = db.query_scalar(
                idx,
                ScalarPredicate::Range {
                    lower: Some(json!(lo)),
                    upper: Some(json!(hi)),
                },
                65536,
            )?;
            let elapsed = start.elapsed().as_secs_f64();
            let actual = ids
                .iter()
                .map(|id| {
                    db.get_by_id(*id).unwrap().unwrap().key[7..]
                        .parse()
                        .unwrap()
                })
                .collect();
            verify(actual, n, 0, lo, hi);
            queries.push(json!({"lower":lo,"upper":hi,"seconds":elapsed,"hits":ids.len()}));
        }
        report["queries"] = json!(queries);
        let mut rounds = Vec::new();
        for round in 1..=3 {
            let start = Instant::now();
            for i in 0..n {
                db.update(c, &key(i), &json!({"age":age(i,round)}))?;
                if (i + 1) % 256 == 0 {
                    db.commit()?;
                    sample(root, &mut peak);
                }
            }
            db.commit()?;
            let update = start.elapsed().as_secs_f64();
            let start = Instant::now();
            for (j, i) in (0..n).step_by(10).enumerate() {
                assert!(db.delete(c, &key(i))?);
                if (j + 1) % 256 == 0 {
                    db.commit()?;
                    sample(root, &mut peak);
                }
            }
            db.commit()?;
            let delete = start.elapsed().as_secs_f64();
            let start = Instant::now();
            for (j, i) in (0..n).step_by(10).enumerate() {
                db.put(c, &key(i), &doc(i, round))?;
                if (j + 1) % 256 == 0 {
                    db.commit()?;
                    sample(root, &mut peak);
                }
            }
            db.commit()?;
            let insert = start.elapsed().as_secs_f64();
            let ids = db.query_scalar(
                idx,
                ScalarPredicate::Range {
                    lower: Some(json!(38)),
                    upper: Some(json!(40)),
                },
                65536,
            )?;
            verify(
                ids.iter()
                    .map(|id| {
                        db.get_by_id(*id).unwrap().unwrap().key[7..]
                            .parse()
                            .unwrap()
                    })
                    .collect(),
                n,
                round,
                38,
                40,
            );
            sample(root, &mut peak);
            db.checkpoint()?;
            rounds.push(json!({"round":round,"update_s":update,"delete_s":delete,"reinsert_s":insert,"bytes":sizes(root),"verified":true}));
        }
        report["churn"] = json!(rounds);
        drop(db);
        let start = Instant::now();
        let db = Database::open(root, cfg())?;
        assert_eq!(db.collection("people")?, Some(c));
        report["reopen_s"] = json!(start.elapsed().as_secs_f64());
        report["reopen_scope"] = json!(
            "open with 8 MiB buffered/FULL config and resolve collection people; row count excluded"
        );
        assert_eq!(db.scan(c, None)?.count(), n);
        drop(db);
    } else if engine == "sqlite" {
        let mut db = Connection::open(root.join("data.sqlite"))?;
        db.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE; CREATE TABLE people(id INTEGER PRIMARY KEY AUTOINCREMENT, ext TEXT NOT NULL UNIQUE, age INTEGER, name TEXT, active INTEGER);")?;
        let journal_mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let synchronous: i64 = db.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        let cache_size: i64 = db.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
        let temp_store: i64 = db.query_row("PRAGMA temp_store", [], |r| r.get(0))?;
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(synchronous, 2);
        assert_eq!(cache_size, -8192);
        assert_eq!(temp_store, 1);
        report["runtime_settings"] = json!({
            "cache_bytes": 8 << 20,
            "cache_size_pragma": cache_size,
            "journal_mode": journal_mode,
            "sqlite_version": rusqlite::version(),
            "sqlite_tmpdir": root.join("tmp"),
            "synchronous_pragma": synchronous,
            "temp_store": "FILE",
            "temp_store_pragma": temp_store
        });
        let start = Instant::now();
        for start in (0..n).step_by(256) {
            let tx = db.transaction()?;
            {
                let mut st = tx.prepare_cached(
                    "INSERT INTO people(ext,age,name,active) VALUES(?1,?2,?3,?4)",
                )?;
                for i in start..(start + 256).min(n) {
                    st.execute(params![
                        key(i),
                        age(i, 0),
                        format!("Person {i:08}"),
                        i % 3 != 0
                    ])?;
                }
            }
            tx.commit()?;
            sample(root, &mut peak);
        }
        report["load_s"] = json!(start.elapsed().as_secs_f64());
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        report["loaded_bytes"] = json!(sizes(root));
        sample(root, &mut peak);
        let mut late_index_peak = sizes(root);
        let start = Instant::now();
        db.execute_batch("CREATE INDEX age_idx ON people(age)")?;
        report["late_index_s"] = json!(start.elapsed().as_secs_f64());
        sample(root, &mut peak);
        sample(root, &mut late_index_peak);
        report["late_index_build_batch_rows"] = Value::Null;
        report["late_index_build_steps"] = json!(1);
        report["late_index_commit_count"] = json!(1);
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        report["indexed_bytes"] = json!(sizes(root));
        sample(root, &mut peak);
        sample(root, &mut late_index_peak);
        report["late_index_sampled_peak_bytes"] = json!(late_index_peak);
        let mut queries = Vec::new();
        for (lo, hi) in [(38, 38), (47, 49), (90, 92)] {
            let start = Instant::now();
            let ids: Vec<i64> = db
                .prepare(
                    "SELECT id FROM people WHERE age>=?1 AND age<=?2 ORDER BY age,id LIMIT 65536",
                )?
                .query_map(params![lo, hi], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let elapsed = start.elapsed().as_secs_f64();
            let actual = ids
                .iter()
                .map(|id| {
                    let k: String = db
                        .query_row("SELECT ext FROM people WHERE id=?1", [id], |r| r.get(0))
                        .unwrap();
                    k[7..].parse().unwrap()
                })
                .collect();
            verify(actual, n, 0, lo, hi);
            queries.push(json!({"lower":lo,"upper":hi,"seconds":elapsed,"hits":ids.len()}));
        }
        report["queries"] = json!(queries);
        let mut rounds = Vec::new();
        for round in 1..=3 {
            let start = Instant::now();
            for start in (0..n).step_by(256) {
                let tx = db.transaction()?;
                {
                    let mut st = tx.prepare_cached("UPDATE people SET age=?1 WHERE ext=?2")?;
                    for i in start..(start + 256).min(n) {
                        assert_eq!(st.execute(params![age(i, round), key(i)])?, 1);
                    }
                }
                tx.commit()?;
                sample(root, &mut peak);
            }
            let update = start.elapsed().as_secs_f64();
            let start = Instant::now();
            for start in (0..n).step_by(2560) {
                let tx = db.transaction()?;
                {
                    let mut st = tx.prepare_cached("DELETE FROM people WHERE ext=?1")?;
                    for i in (start..(start + 2560).min(n)).step_by(10) {
                        assert_eq!(st.execute([key(i)])?, 1);
                    }
                }
                tx.commit()?;
                sample(root, &mut peak);
            }
            let delete = start.elapsed().as_secs_f64();
            let start = Instant::now();
            for start in (0..n).step_by(2560) {
                let tx = db.transaction()?;
                {
                    let mut st = tx.prepare_cached(
                        "INSERT INTO people(ext,age,name,active) VALUES(?1,?2,?3,?4)",
                    )?;
                    for i in (start..(start + 2560).min(n)).step_by(10) {
                        st.execute(params![
                            key(i),
                            age(i, round),
                            format!("Person {i:08}"),
                            i % 3 != 0
                        ])?;
                    }
                }
                tx.commit()?;
                sample(root, &mut peak);
            }
            let insert = start.elapsed().as_secs_f64();
            let actual: Vec<usize> = db
                .prepare("SELECT ext FROM people WHERE age BETWEEN 38 AND 40 ORDER BY age,id")?
                .query_map([], |r| {
                    let k: String = r.get(0)?;
                    Ok(k[7..].parse().unwrap())
                })?
                .collect::<rusqlite::Result<_>>()?;
            verify(actual, n, round, 38, 40);
            sample(root, &mut peak);
            db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            rounds.push(json!({"round":round,"update_s":update,"delete_s":delete,"reinsert_s":insert,"bytes":sizes(root),"verified":true}));
        }
        report["churn"] = json!(rounds);
        drop(db);
        let start = Instant::now();
        let db = Connection::open(root.join("data.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE;")?;
        let table: String = db.query_row(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name='people'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(table, "people");
        report["reopen_s"] = json!(start.elapsed().as_secs_f64());
        report["reopen_scope"] = json!("open, reapply WAL/FULL/-8192/temp_store=FILE, and resolve table people in sqlite_schema; row count excluded");
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM people", [], |r| r.get::<_, usize>(0))?,
            n
        );
        drop(db);
    } else {
        return Err("unknown engine".into());
    }
    report["final_bytes"] = json!(sizes(root));
    report["sampled_peak_bytes"] = json!(peak);
    report["process_peak_rss"] = json!(hwm());
    report["verified"] = json!(true);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
