//! Space observations during work, plus a checkpoint logical-length upper bound.
use super::*;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use std::time::Duration;
thread_local! { static PROFILE: std::cell::RefCell<Profile> = std::cell::RefCell::new(Profile::default()); }
#[derive(Default)]
struct Profile {
    commit: Vec<f64>,
    checkpoint_seconds: f64,
    checkpoints: u64,
    auto_checkpoints: u64,
}
fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v[((v.len() - 1) as f64 * q).ceil() as usize]
    }
}
fn profile_start() {
    PROFILE.with(|p| *p.borrow_mut() = Profile::default());
    kernel::write_stats::reset();
}
fn profile_take() -> Value {
    let w = kernel::write_stats::take();
    PROFILE.with(|p| {
        let mut p = p.borrow_mut(); p.commit.sort_by(f64::total_cmp);
        json!({"commit_count":p.commit.len(),"commit_seconds":p.commit.iter().sum::<f64>(),"commit_p50_seconds":percentile(&p.commit,0.5),"commit_p95_seconds":percentile(&p.commit,0.95),"commit_p99_seconds":percentile(&p.commit,0.99),"explicit_checkpoint_seconds":p.checkpoint_seconds,"explicit_checkpoints":p.checkpoints,"automatic_checkpoints":p.auto_checkpoints,"e4_issued_bytes":{"wal":w.wal,"pages":w.final_pages,"sidecars":w.sidecars}})
    })
}
fn commit_policy(db: &mut Db, cadence: u64) -> Result<()> {
    let t = Instant::now();
    match db {
        Db::E4(s) if cadence > 0 => {
            if s.commit_with_checkpoint(cadence, cadence)? {
                PROFILE.with(|p| p.borrow_mut().auto_checkpoints += 1);
            }
        }
        _ => db.commit()?,
    }
    PROFILE.with(|p| p.borrow_mut().commit.push(t.elapsed().as_secs_f64()));
    Ok(())
}
// Full typed materialization, without rebuilding an oracle or serializing it.
// Exact ordered values are checked independently by verify_expected.
fn full_scan(db: &Db, n: u64) -> Result<Value> {
    let t = Instant::now();
    let mut count = 0u64;
    match db {
        Db::E4(store) => {
            let l = Layout::from_descriptor(&store.get(&[0, 240, 0])?.ok_or("missing layout")?)?;
            for item in store.scan(&[0x81])? {
                let (key, bytes) = item?;
                std::hint::black_box((
                    id_of(&key),
                    decode_dense_v3(&l, &bytes, |_| Err("unexpected vector".into()))?,
                ));
                count += 1;
            }
        }
        Db::Sql(c) => {
            let mut st=c.prepare("SELECT id,name,active,income,lon,lat,json(profile),json(extra) FROM person ORDER BY id")?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let id: u64 = r.get(0)?;
                let d = json!({"name":r.get::<_,String>(1)?,"active":r.get::<_,bool>(2)?,"income":r.get::<_,Option<f64>>(3)?,"location":{"type":"Point","coordinates":[r.get::<_,f64>(4)?,r.get::<_,f64>(5)?]},"profile":serde_json::from_str::<Value>(&r.get::<_,String>(6)?)?,"extra":serde_json::from_str::<Value>(&r.get::<_,String>(7)?)?});
                std::hint::black_box((id, d));
                count += 1;
            }
        }
    }
    let seconds = t.elapsed().as_secs_f64();
    assert_eq!(count, n);
    Ok(
        json!({"rows":count,"seconds":seconds,"semantics":"full ordered typed rows including ID; oracle/checksum excluded; warm OS cache; same Value materialization"}),
    )
}
fn point_reads(db: &Db, n: u64, cycle: u64, case: &str) -> Result<Value> {
    let mut latencies = Vec::new();
    let mut crc = 0;
    let l = layout();
    for i in 0..3000u64 {
        let id = 1 + (i * 7919 + 1237) % n;
        let oracle = expected(id, cycle, case);
        let t = Instant::now();
        let d=match db {
            Db::E4(s) => decode_dense_v3(&l,&s.get(&key(id))?.ok_or("missing point row")?, |_|Err("unexpected vector".into()))?,
            Db::Sql(c) => c.prepare_cached("SELECT name,active,income,lon,lat,json(profile),json(extra) FROM person WHERE id=?")?.query_row([id], |r| {
                Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?,r.get::<_,Option<f64>>(2)?,r.get::<_,f64>(3)?,r.get::<_,f64>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?))
            }).map(|(name,active,income,lon,lat,profile,extra)| -> Result<Value> {Ok(json!({"name":name,"active":active,"income":income,"location":{"type":"Point","coordinates":[lon,lat]},"profile":serde_json::from_str::<Value>(&profile)?,"extra":serde_json::from_str::<Value>(&extra)?}))})??,
        };
        latencies.push(t.elapsed().as_secs_f64());
        assert_eq!(d, oracle);
        crc = crc32c::crc32c_append(crc, &serde_json::to_vec(&d)?);
    }
    latencies.sort_by(f64::total_cmp);
    Ok(
        json!({"count":latencies.len(),"crc32c":crc,"seconds":latencies.iter().sum::<f64>(),"p50_seconds":percentile(&latencies,0.5),"p95_seconds":percentile(&latencies,0.95),"p99_seconds":percentile(&latencies,0.99),"semantics":"typed full-row materialization; oracle and checksum excluded; warm OS cache, fixed engine cache"}),
    )
}

#[derive(Default)]
struct Peaks {
    samples: u64,
    logical: u64,
    allocated: u64,
    files: Value,
    errors: u64,
    rss: Option<u64>,
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
        let rss = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)).and_then(|n| n.parse::<u64>().ok()).map(|n| n*1024)
        });
        let mut p = peaks.lock().unwrap();
        if let Some(rss) = rss { p.rss = Some(p.rss.unwrap_or(0).max(rss)); }
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
        let v = json!({"sampled_peak_rss_bytes":p.rss,"sampled_peak_bytes":p.logical,"sampled_peak_allocated_bytes":p.allocated,"samples":p.samples,"sample_errors":p.errors,"files_at_sampled_logical_peak":p.files});
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
        let t = Instant::now();
        db.checkpoint(pinned)?;
        PROFILE.with(|p| {
            let mut p = p.borrow_mut();
            p.checkpoints += 1;
            p.checkpoint_seconds += t.elapsed().as_secs_f64();
        });
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
    _dir: &Path,
    _pinned: bool,
    _copies: u64,
) -> Result<(u64, u64)> {
    let mut count = 0;
    let bound = 0;
    db.begin()?;
    let ordered_load = cycle == 0 && std::env::var("E4_BENCH_ORDERED_LOAD").as_deref() == Ok("1");
    for i in 0..n {
        let id = if ordered_load { i + 1 } else { 1 + (i * 7919 + 1237) % n };
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
            commit_policy(db, cadence)?;
            monitor.sample();
            db.begin()?;
        }
        if cycle > 0 && stage == 1 && case != "updates" && id % 10 == 1 {
            db.delete(id)?;
        } else {
            db.put_document(id, &expected(id, cycle, case))?;
        }
        count += 1;
    }
    commit_policy(db, cadence)?;
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
    cycles: u64,
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
    if let Db::Sql(c) = &db {
        c.execute_batch("PRAGMA wal_autocheckpoint=1000;")?;
    }
    let monitor = Monitor::start(&home);
    let mut stages = Vec::new();
    profile_start();
    let t = Instant::now();
    let (_, intra_bound) = mutate_case(
        &mut db, n, 0, 0, case, &monitor, cadence, &dir, false, copies,
    )?;
    let bound = intra_bound.max(checkpoint_policy(&mut db, &dir, false, copies)?);
    let seconds = t.elapsed().as_secs_f64();
    let profile = profile_take();
    let peak = monitor.take();
    let initial = disk(&dir)?.0;
    let verify = db.verify_expected(n, false, |id| expected(id, 0, case))?;
    let structure = db.structure(&dir, n, false)?;
    let v = json!({"case":case,"arm":name,"n":n,"stage":"load","cycle":0,"profile":profile,"seconds":seconds,"disk":footprint(&dir)?,"peak":peak,"checkpoint_upper_bound_bytes":bound,"verify":verify,"structure":structure,"counters":db.counters()});
    record(root, &v)?;
    stages.push(v);
    let mut snapshot = if case == "mixed_long" {
        Some(db.snapshot(&dir)?)
    } else {
        None
    };
    if case != "load" {
        for cycle in 1..=cycles {
            if case.starts_with("mixed_short") {
                snapshot = Some(db.snapshot(&dir)?);
            }
            for stage in 1..=if case == "updates" { 1 } else { 2 } {
                // Discard verification-only observations before timed mutation.
                monitor.take();
                profile_start();
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
                let profile = profile_take();
                // Close/reopen is included in the space observation, not mutation time.
                // Keep the writer alive through sustained work. Reopen is checked at the end.
                let counters = db.counters();
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
                let v = json!({"case":case,"arm":name,"n":n,"stage":if stage==2 {"reinsert"}else if case=="updates" {"update"}else{"update_delete"},"cycle":cycle,"operations":operations,"seconds":seconds,"profile":profile,"pinned":snapshot.is_some(),"disk":footprint(&dir)?,"peak":peak,"checkpoint_upper_bound_bytes":bound,"verify":verify,"snapshot_verify":old,"structure":structure,"accounting":accounting,"counters":counters});
                record(root, &v)?;
                stages.push(v);
            }
            if case.starts_with("mixed_short") || cycle == 2 {
                drop(snapshot.take());
            }
            if case == "mixed_short_gap" {
                // Price an explicit no-reader maintenance opportunity in BOTH
                // engines before registering the next cycle's snapshot.
                monitor.take();
                profile_start();
                let t = Instant::now();
                let bound = checkpoint_policy(&mut db, &dir, false, copies)?;
                let seconds = t.elapsed().as_secs_f64();
                let profile = profile_take();
                let peak = monitor.take();
                let verify = db.verify_expected(n, false, |id| expected(id, cycle, case))?;
                let structure = db.structure(&dir, n, false)?;
                let v = json!({"case":case,"arm":name,"n":n,"stage":"reader_release_checkpoint","cycle":cycle,"operations":0,"seconds":seconds,"profile":profile,"pinned":false,"disk":footprint(&dir)?,"peak":peak,"checkpoint_upper_bound_bytes":bound,"verify":verify,"snapshot_verify":null,"structure":structure,"accounting":null,"counters":db.counters()});
                record(root, &v)?;
                stages.push(v);
            }
        }
    }
    let reads = point_reads(&db, n, if case == "load" { 0 } else { cycles }, case)?;
    let scan = full_scan(&db, n)?;
    drop(snapshot);
    drop(db);
    let reopen_started = Instant::now();
    let reopened = Db::open(&dir, sqlite, false)?;
    let reopen_seconds = reopen_started.elapsed().as_secs_f64();
    let reopened_verify = reopened.verify_expected(n, false, |id| {
        expected(id, if case == "load" { 0 } else { cycles }, case)
    })?;
    drop(reopened);
    let closing_peak = monitor.take();
    let final_disk = footprint(&dir)?;
    let out = json!({"case":case,"arm":name,"n":n,"initial_bytes":initial,"stages":stages,"closing_peak":closing_peak,"final_disk":final_disk,"reads":reads,"scan":scan,"reopen_seconds":reopen_seconds,"reopened_verify":reopened_verify});
    drop(monitor);
    Ok(out)
}
fn process_memory_limits() -> Value {
    let path = std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("0::").map(str::to_owned))
    });
    let Some(path) = path else { return Value::Null; };
    let dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let read = |name| std::fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_owned());
    let address_space = std::fs::read_to_string("/proc/self/limits").ok().and_then(|s| {
        s.lines().find(|l| l.starts_with("Max address space"))
            .map(|l| l.split_whitespace().skip(3).take(2).map(str::to_owned).collect::<Vec<_>>())
    });
    json!({"cgroup":path,"memory_max":read("memory.max"),"swap_max":read("memory.swap.max"),
        "address_space_soft_hard":address_space})
}
/// Post-reboot audit; benchmark executable is retained separately.
pub(super) fn verify_saved(args: &[String]) -> Result<()> {
    let root = Path::new(args.first().ok_or("missing matrix directory")?);
    if !artifact_root_allowed(root) { return Err("data must stay in the authorized workspace".into()); }
    let mut pairs = Vec::new();
    let mut paths = fs::read_dir(root)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    for pair in paths {
        if !pair.is_dir() { continue; }
        let name = pair.file_name().unwrap().to_str().ok_or("invalid run name")?;
        if !root.join(format!("{name}.completed")).is_file() { continue; }
        let saved: Value = serde_json::from_slice(&fs::read(pair.join("results.json"))?)?;
        assert_eq!(saved["complete"], true);
        let mut arms = Vec::new();
        for arm in saved["arms"].as_array().ok_or("missing arms")? {
            let engine = arm["arm"].as_str().ok_or("missing engine")?;
            let case = arm["case"].as_str().ok_or("missing case")?;
            assert!(matches!(case, "load" | "updates" | "mixed_none" | "mixed_long"));
            let n = arm["n"].as_u64().ok_or("missing rows")?;
            let cycle = if case == "load" { 0 } else { saved["cycles"].as_u64().ok_or("missing cycles")? };
            let dir = pair.join(format!("{case}-{engine}-{n}")).join("store");
            let before = fingerprints(&dir)?;
            let db = match engine {
                "e4" => Db::E4(Store::open_snapshot(&dir, cfg())?),
                "sqlite" => {
                    let c = Connection::open_with_flags(dir.join("data.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    c.execute_batch("PRAGMA cache_size=-8192; PRAGMA mmap_size=0; BEGIN;")?;
                    Db::Sql(c)
                }
                _ => return Err("unknown engine".into()),
            };
            let verified = db.verify_expected(n, false, |id| expected(id, cycle, case))?;
            assert_eq!(verified["crc32c"], arm["reopened_verify"]["crc32c"]);
            let structure = db.structure(&dir, n, false)?;
            drop(db);
            let after = fingerprints(&dir)?;
            for file in ["data", "wal", "free", "data.sqlite", "data.sqlite-wal"] {
                // SQLite may create an empty WAL on a read-only WAL-mode open.
                // Missing and empty contain the same bytes; never waive a
                // change to a nonempty WAL or to either engine's data file.
                if file == "data.sqlite-wal"
                    && before.get(file).map_or(true, |v| v["bytes"] == 0)
                    && after.get(file).map_or(true, |v| v["bytes"] == 0) {
                    continue;
                }
                assert_eq!(before.get(file), after.get(file), "source changed: {name}/{file}");
            }
            arms.push(json!({"engine":engine,"verify":verified,"structure":structure,"source_unchanged":true}));
            eprintln!("verified {name}/{engine}");
        }
        assert_eq!(arms.len(), 2);
        pairs.push(json!({"run":name,"arms":arms}));
    }
    println!("{}", serde_json::to_string_pretty(&json!({"complete":true,"process_memory_limits":process_memory_limits(),"pairs":pairs}))?);
    Ok(())
}
pub(super) fn run(args: &[String]) -> Result<()> {
    let root = Path::new(args.first().ok_or("missing run directory")?);
    if !artifact_root_allowed(root) {
        return Err("data must stay on scratch".into());
    }
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
        "mixed_short_gap",
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

    let copies = args
        .get(4)
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(1);
    if copies != 1 && copies != 2 {
        return Err("E4 publications must be 1 or 2".into());
    }
    let cycles = args
        .get(5)
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(20);
    if cycles == 0 || cycles % 2 != 0 {
        return Err("cycles must be positive and even".into());
    }
    let sqlite_first = args.get(6).is_some_and(|s| s == "sqlite-first");
    fs::create_dir(root)?;
    let mut result = json!({"sizes":sizes,"cycles":cycles,"commit_operations":1000,"checkpoint_cadence":"end of mutation phase; load end","sample_interval_ms":1,"peak_semantics":"sampled logical, allocated and RSS peaks; the sampler is not a cap; E4 admission limits are recorded separately","writer_cache_bytes":8<<20,"snapshot_cache_bytes":64<<10,"sync":if cfg!(target_os="macos") {"FULL + F_FULLFSYNC"} else {"FULL + fsync"},"timestamps":false,"sqlite_version":rusqlite::version(),"arms":[]});
    result["process_memory_limits"] = process_memory_limits();
    result["load_order"] = json!(if std::env::var("E4_BENCH_ORDERED_LOAD").as_deref() == Ok("1") {
        "ascending IDs (both engines)"
    } else { "fixed permutation (both engines)" });
    result["e4_resource_limits"] = json!(benchmark_limits()?.map(|l| json!({
        "data_bytes": l.data_bytes, "wal_bytes": l.wal_bytes, "tracked_pages": l.tracked_pages,
        "readers": l.readers, "record_bytes": l.record_bytes, "recovery_bytes": l.recovery_bytes,
        "managed_logical_allowance": l.total_bytes().unwrap(), "commit_mode":"publish each commit"
    })));
    result["e4_publications_per_checkpoint"] = json!(copies);
    result["e4_wal_or_allocated_page_checkpoint_bytes"] = json!(cadence);
    if cadence > 0 {
        result["checkpoint_cadence"] = if benchmark_limits()?.is_some() {
            json!("E4 constrained publication every commit; SQLite native auto=1000 pages; both phase end")
        } else { json!(format!("E4 WAL or allocated page bytes >= {cadence} bytes after commit; SQLite native auto=1000 pages; both phase end")) };
    }
    result["sqlite_native_autocheckpoint_pages"] = json!(1000);
    result["sqlite_first"] = json!(sqlite_first);
    result["short_reader_schedules"] = json!({"mixed_short":"rolling snapshots, no checkpoint in the release gap", "mixed_short_gap":"both engines checkpoint with no reader after each cycle; maintenance time included"});
    for n in sizes {
        for case in cases {
            if selected.is_some_and(|s| s != case) {
                continue;
            }
            for (arm_index, sqlite) in [sqlite_first, !sqlite_first].into_iter().enumerate() {
                let arm = arm_space(root, n, sqlite, case, cadence, copies, cycles)?;
                if arm_index == 1 {
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
    println!("SUSTAINED COMPLETE {}", root.display());
    Ok(())
}
