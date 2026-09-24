//! Space observations during work, plus a checkpoint logical-length upper bound.
use super::*;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use std::time::Duration;
#[derive(Default)]
struct Peaks {
    samples: u64,
    logical: u64,
    allocated: u64,
    files: Value,
    errors: u64,
}
fn disk(dir: &Path) -> std::io::Result<(u64, u64, Value)> {
    fn visit(
        dir: &Path,
        base: &Path,
        logical: &mut u64,
        allocated: &mut u64,
        files: &mut serde_json::Map<String, Value>,
    ) -> std::io::Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            let m = match e.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if m.is_dir() {
                match visit(&p, base, logical, allocated, files) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    other => other?,
                }
            } else {
                *logical += m.len();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    *allocated += m.blocks() * 512;
                }
                files.insert(
                    p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    json!(m.len()),
                );
            }
        }
        Ok(())
    }
    let (mut logical, mut allocated, mut files) = (0, 0, serde_json::Map::new());
    visit(dir, dir, &mut logical, &mut allocated, &mut files)?;
    Ok((logical, allocated, Value::Object(files)))
}
struct Monitor {
    dir: PathBuf,
    peaks: Arc<Mutex<Peaks>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Monitor {
    fn start(dir: &Path) -> Self {
        let peaks = Arc::new(Mutex::new(Peaks::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s, d) = (peaks.clone(), stop.clone(), dir.to_owned());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                Self::observe(&d, &p);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            dir: dir.to_owned(),
            peaks,
            stop,
            thread: Some(thread),
        }
    }
    fn observe(dir: &Path, peaks: &Mutex<Peaks>) {
        let value = disk(dir);
        let mut p = peaks.lock().unwrap();
        match value {
            Ok((l, a, f)) => {
                p.samples += 1;
                if l > p.logical {
                    p.logical = l;
                    p.files = f;
                }
                p.allocated = p.allocated.max(a);
            }
            Err(_) => p.errors += 1,
        }
    }
    fn sample(&self) {
        Self::observe(&self.dir, &self.peaks);
    }
    fn take(&self) -> Value {
        self.sample();
        let mut p = self.peaks.lock().unwrap();
        let v = json!({"sampled_peak_bytes":p.logical,"sampled_peak_allocated_bytes":p.allocated,"samples":p.samples,"sample_errors":p.errors,"files_at_sampled_logical_peak":p.files});
        *p = Peaks::default();
        v
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

// Between explicit checkpoints these fixtures never truncate database files.
// SQLite's auto-checkpoint is disabled. Before checkpoint, include the maximum
// destination main length while the existing WAL and old/new E4 freelists
// coexist. This is a conservative logical-file bound, not an allocated-space
// quota, and not a bound for arbitrary SQL/recovery/other processes.
fn checkpoint_bound(db: &Db, dir: &Path) -> Result<u64> {
    let (total, _, files) = disk(dir)?;
    Ok(match db {
        Db::E4(s) => {
            total - files["data"].as_u64().unwrap_or(0)
                + (s.pool_ref().page_count() as u64 * 4096).max(files["data"].as_u64().unwrap_or(0))
                + s.pool_ref().export_free(s.generation() + 1).len() as u64
        }
        Db::Sql(c) => {
            let pages: u64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            total - files["data.sqlite"].as_u64().unwrap_or(0)
                + (pages * 4096).max(files["data.sqlite"].as_u64().unwrap_or(0))
        }
    })
}
// Experimental policy: publish the same already-durable root a second time.
// Both meta slots then reference it, so the older distinct tree can be reused.
// This is NOT the Store default; readers can still pin either older tree.
fn checkpoint_policy(db: &mut Db, dir: &Path, pinned: bool, copies: u64) -> Result<u64> {
    let count = if matches!(db, Db::E4(_)) { copies } else { 1 };
    let mut bound = 0;
    for _ in 0..count {
        bound = bound.max(checkpoint_bound(db, dir)?);
        db.checkpoint(pinned)?;
    }
    Ok(bound)
}
fn expected(id: u64, cycle: u64, case: &str) -> Value {
    let mut d = doc(id, if case == "delete_reinsert" { 0 } else { cycle });
    // Isolate version-management cost from genuine payload growth.
    if case == "updates" {
        d["profile"]["notes"] = json!("x".repeat(24));
    }
    d
}
fn mutate_case(
    db: &mut Db,
    n: u64,
    cycle: u64,
    stage: u8,
    case: &str,
    monitor: &Monitor,
    cadence: u64,
    dir: &Path,
    pinned: bool,
    copies: u64,
) -> Result<(u64, u64)> {
    let mut count = 0;
    let mut bound = 0;
    db.begin()?;
    for i in 0..n {
        let id = 1 + (i * 7919 + 1237) % n;
        let selected = if cycle == 0 {
            true
        } else {
            match case {
                "updates" => stage == 1 && (id % 5 == 0 || id % 10 == 1),
                "delete_reinsert" => id % 10 == 1,
                _ => {
                    if stage == 1 {
                        id % 5 == 0 || id % 10 == 1
                    } else {
                        id % 10 == 1
                    }
                }
            }
        };
        if !selected {
            continue;
        }
        if count > 0 && count % 1000 == 0 {
            db.commit()?;
            monitor.sample();
            if cadence > 0 && count % cadence == 0 {
                bound = bound.max(checkpoint_policy(db, dir, pinned, copies)?);
                monitor.sample();
            }
            db.begin()?;
        }
        if cycle > 0 && stage == 1 && case != "updates" && id % 10 == 1 {
            db.delete(id)?;
        } else {
            db.put_document(id, &expected(id, cycle, case))?;
        }
        count += 1;
    }
    db.commit()?;
    monitor.sample();
    Ok((count, bound))
}

fn arm_space(
    root: &Path,
    n: u64,
    sqlite: bool,
    case: &str,
    cadence: u64,
    copies: u64,
) -> Result<Value> {
    let name = if sqlite { "sqlite" } else { "e4" };
    let home = root.join(format!("{case}-{name}-{n}"));
    fs::create_dir(&home)?;
    let dir = home.join("store");
    let tmp = home.join("tmp");
    fs::create_dir(&tmp)?;
    std::env::set_var("TMPDIR", &tmp);
    std::env::set_var("SQLITE_TMPDIR", &tmp);
    let mut db = Db::open(&dir, sqlite, true)?;
    let monitor = Monitor::start(&home);
    let mut stages = Vec::new();
    let t = Instant::now();
    let (_, intra_bound) = mutate_case(
        &mut db, n, 0, 0, case, &monitor, cadence, &dir, false, copies,
    )?;
    let bound = intra_bound.max(checkpoint_policy(&mut db, &dir, false, copies)?);
    let seconds = t.elapsed().as_secs_f64();
    let peak = monitor.take();
    let initial = disk(&dir)?.0;
    let verify = db.verify_expected(n, false, |id| expected(id, 0, case))?;
    let structure = db.structure(&dir, n, false)?;
    let v = json!({"case":case,"arm":name,"n":n,"stage":"load","cycle":0,"seconds":seconds,"disk":footprint(&dir)?,"peak":peak,"checkpoint_upper_bound_bytes":bound,"verify":verify,"structure":structure,"counters":db.counters()});
    record(root, &v)?;
    stages.push(v);
    let mut snapshot = if case == "mixed_long" {
        Some(db.snapshot(&dir)?)
    } else {
        None
    };
    if case != "load" {
        for cycle in 1..=6 {
            if case == "mixed_short" {
                snapshot = Some(db.snapshot(&dir)?);
            }
            for stage in 1..=if case == "updates" { 1 } else { 2 } {
                // Discard verification-only observations before timed mutation.
                monitor.take();
                let t = Instant::now();
                let (operations, intra_bound) = mutate_case(
                    &mut db,
                    n,
                    cycle,
                    stage,
                    case,
                    &monitor,
                    cadence,
                    &dir,
                    snapshot.is_some(),
                    copies,
                )?;
                let bound = intra_bound.max(checkpoint_policy(
                    &mut db,
                    &dir,
                    snapshot.is_some(),
                    copies,
                )?);
                let seconds = t.elapsed().as_secs_f64();
                // Close/reopen is included in the space observation, not mutation time.
                drop(db);
                db = Db::open(&dir, sqlite, false)?;
                let peak = monitor.take();
                let deleted = stage == 1 && case != "updates";
                let verify = db.verify_expected(n, deleted, |id| expected(id, cycle, case))?;
                let old = match &snapshot {
                    Some(s) => s.verify_expected(n, false, |id| {
                        expected(id, if case == "mixed_long" { 0 } else { cycle - 1 }, case)
                    })?,
                    None => Value::Null,
                };
                let structure = db.structure(&dir, n, deleted)?;
                let accounting = if !sqlite && cycle % 2 == 0 {
                    if let Db::E4(s) = &db {
                        let physical = s.pool_ref().page_count() as u64;
                        let live = structure["reachable_pages"].as_u64().unwrap();
                        let free = s.pool_ref().free_pages_pending() as u64;
                        assert_eq!(physical, 2 + live + free, "unaccounted pages");
                        json!({"physical_pages":physical,"live_tree_pages":live,"free_pages":free,"unaccounted_pages":0})
                    } else {
                        unreachable!()
                    }
                } else {
                    Value::Null
                };
                let v = json!({"case":case,"arm":name,"n":n,"stage":if stage==2 {"reinsert"}else if case=="updates" {"update"}else{"update_delete"},"cycle":cycle,"operations":operations,"seconds":seconds,"pinned":snapshot.is_some(),"disk":footprint(&dir)?,"peak":peak,"checkpoint_upper_bound_bytes":bound,"verify":verify,"snapshot_verify":old,"structure":structure,"accounting":accounting,"counters":db.counters()});
                record(root, &v)?;
                stages.push(v);
            }
            if case == "mixed_short" || cycle == 2 {
                drop(snapshot.take());
            }
        }
    }
    drop(snapshot);
    drop(db);
    let closing_peak = monitor.take();
    let final_disk = footprint(&dir)?;
    let mut out = json!({"case":case,"arm":name,"n":n,"initial_bytes":initial,"stages":stages,"closing_peak":closing_peak,"final_disk":final_disk});
    if case == "mixed_long" {
        // The monitor covers source + destination + sorting/recovery scratch.
        monitor.take();
        let before = fingerprints(&dir)?;
        let t = Instant::now();
        let target = home.join("rebuilt");
        let rebuilt = if sqlite {
            fs::create_dir(&target)?;
            let c = immutable_sqlite(&dir.join("data.sqlite"))?;
            sql_config(&c)?;
            c.execute(
                "VACUUM INTO ?",
                [target.join("data.sqlite").to_string_lossy().as_ref()],
            )?;
            drop(c);
            target
        } else {
            let r = kernel::recover::recover_to(&dir, &target, cfg())?;
            assert_eq!(r.entries_recovered, n + 3);
            assert_eq!(r.known_value_losses, 0);
            assert_eq!(r.unknown_extents, 0);
            r.database
        };
        let seconds = t.elapsed().as_secs_f64();
        let peak = monitor.take();
        let db = if sqlite {
            Db::Sql(immutable_sqlite(&rebuilt.join("data.sqlite"))?)
        } else {
            Db::E4(Store::open_snapshot(&rebuilt, cfg())?)
        };
        let verify = db.verify_expected(n, false, |id| expected(id, 6, case))?;
        let structure = db.structure(&rebuilt, n, false)?;
        drop(db);
        assert_eq!(before, fingerprints(&dir)?, "maintenance changed source");
        out["maintenance"] = json!({"seconds":seconds,"peak":peak,"verify":verify,"structure":structure,"rebuilt_disk":footprint(&rebuilt)?,"source_unchanged":true,"source_fingerprints":before,"published_over_source":false});
    }
    drop(monitor);
    Ok(out)
}
pub(super) fn run(args: &[String]) -> Result<()> {
    let root = &artifact_root(args.first())?;
    let sizes = if let Some(n) = args.get(1) {
        vec![n.parse::<u64>()?]
    } else {
        vec![100_000, 400_000]
    };
    for n in &sizes {
        let mut f = *n;
        while f > 0 && f % 2 == 0 {
            f /= 2;
        }
        while f > 0 && f % 5 == 0 {
            f /= 5;
        }
        if f != 1 || n % 1000 != 0 {
            return Err("unsupported permutation size".into());
        }
    }
    let cases = [
        "load",
        "updates",
        "delete_reinsert",
        "mixed_none",
        "mixed_short",
        "mixed_long",
    ];
    let selected = args.get(2).map(String::as_str);
    if selected.is_some_and(|s| !cases.contains(&s)) {
        return Err("unknown case".into());
    }
    let cadence = args
        .get(3)
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    if cadence % 1000 != 0 {
        return Err("checkpoint cadence must be a multiple of the 1000-operation commit".into());
    }
    let copies = args
        .get(4)
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(1);
    if copies != 1 && copies != 2 {
        return Err("E4 publications must be 1 or 2".into());
    }
    fs::create_dir(root)?;
    let mut result = json!({"sizes":sizes,"cycles":6,"commit_operations":1000,"checkpoint_cadence":"end of mutation phase; load end","sample_interval_ms":1,"peak_semantics":"sampled lower bound; checkpoint upper bound for logical bytes separately; no enforced quota","writer_cache_bytes":8<<20,"snapshot_cache_bytes":64<<10,"sync":"FULL + macOS fullfsync","timestamps":false,"sqlite_version":rusqlite::version(),"arms":[]});
    result["e4_publications_per_checkpoint"] = json!(copies);
    result["checkpoint_operations"] = json!(cadence);
    if cadence > 0 {
        result["checkpoint_cadence"] = json!(format!("every {cadence} operations and phase end"));
    }
    for n in sizes {
        for case in cases {
            if selected.is_some_and(|s| s != case) {
                continue;
            }
            for sqlite in [false, true] {
                let arm = arm_space(root, n, sqlite, case, cadence, copies)?;
                if sqlite {
                    let previous = result["arms"].as_array().unwrap().last().unwrap();
                    for (a, b) in previous["stages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .zip(arm["stages"].as_array().unwrap())
                    {
                        assert_eq!(
                            a["verify"]["crc32c"], b["verify"]["crc32c"],
                            "E4/SQLite disagree"
                        );
                    }
                }
                result["arms"].as_array_mut().unwrap().push(arm);
                fs::write(
                    root.join("results.json"),
                    serde_json::to_vec_pretty(&result)?,
                )?;
            }
        }
    }
    result["complete"] = json!(true);
    fs::write(
        root.join("results.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!("SPACE COMPLETE {}", root.display());
    Ok(())
}
