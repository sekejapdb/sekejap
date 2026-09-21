//! Streaming, three-arm population benchmark. Database artifacts stay on scratch.
use sekejap_core::*;
use kernel::{
    io::IoMode,
    store::{Config, Store, SyncMode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const BATCH: u64 = 1000;
const PROGRESS: u64 = 250_000;
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn layout(stamps: bool) -> Layout {
    let mut fields = vec![
        ("_key".into(), Kind::Text),
        ("fullname".into(), Kind::Text),
        ("born".into(), Kind::Int),
        ("born_year".into(), Kind::Int),
        ("active".into(), Kind::Bool),
        ("income".into(), Kind::Real),
        ("location".into(), Kind::Point),
        ("profile".into(), Kind::Json),
    ];
    if stamps {
        fields.extend([
            ("_created_unix".into(), Kind::Int),
            ("_updated_unix".into(), Kind::Int),
        ]);
    }
    Layout { id: 1, fields }
}
fn person(id: u64) -> Value {
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
        "Zhang",
        "Smith",
        "Khan",
    ];
    let year = 1940 + id % 80;
    let language = ["en", "id", "es", "ja"][(id % 4) as usize];
    let mut d = json!({"_key":format!("p{id:08}"),"fullname":format!("{} {}",FIRST[(id%FIRST.len() as u64)as usize],LAST[((id/20)%LAST.len()as u64)as usize]),"born":year*10000+(1+id%12)*100+1+id%28,"born_year":year,"active":id%5!=0,"income":if id%10==0{Value::Null}else{json!(20000.0+(id*73%180000)as f64+0.25)},"location":{"type":"Point","coordinates":[-179.0+(id*37%358000)as f64/1000.0,-85.0+(id*53%170000)as f64/1000.0]},"profile":{"languages":[language,"en"],"preferences":{"contact":id%2==0,"score":(id%100)as f64/4.0},"tags":["person",null],"household":{"size":1+id%7}}});
    if id % 3 == 0 {
        d["source"] = json!("survey");
    }
    if id % 7 == 0 {
        d["extra"] = json!({"notes":[null,"東京-é"],"unsigned":u64::MAX});
    }
    d
}
fn stamp_fresh(d: &mut Value, second: u64) {
    d["_created_unix"] = json!(second);
    d["_updated_unix"] = json!(second);
}
fn key(id: u64) -> Vec<u8> {
    let b = id.to_be_bytes();
    let at = b.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = vec![0x80 + (8 - at) as u8];
    k.extend_from_slice(&b[at..]);
    k
}
fn unkey(k: &[u8]) -> Result<u64> {
    let n = k
        .first()
        .ok_or("missing ID tag")?
        .checked_sub(0x80)
        .ok_or("invalid ID tag")? as usize;
    if !(1..=8).contains(&n) || k.len() != n + 1 {
        return Err("invalid ID width".into());
    }
    let mut b = [0; 8];
    b[8 - n..].copy_from_slice(&k[1..]);
    Ok(u64::from_be_bytes(b))
}
fn cat(i: u8) -> Vec<u8> {
    vec![0, 240, i]
}
fn size(path: &Path) -> Result<u64> {
    if path.is_file() {
        Ok(path.metadata()?.len())
    } else {
        let mut n = 0;
        for e in fs::read_dir(path)? {
            n += size(&e?.path())?;
        }
        Ok(n)
    }
}
fn file_sizes(path: &Path) -> Result<Value> {
    let mut map = Map::new();
    for entry in fs::read_dir(path)? {
        let e = entry?;
        if e.file_type()?.is_file() {
            map.insert(
                e.file_name().to_string_lossy().into_owned(),
                json!(e.metadata()?.len()),
            );
        }
    }
    Ok(Value::Object(map))
}
fn update_crc(crc: &mut u32, d: &Value) -> Result<()> {
    *crc = crc32c::crc32c_append(*crc, &serde_json::to_vec(d)?);
    Ok(())
}
fn log(msg: String) {
    println!("{msg}");
    std::io::stdout().flush().unwrap();
}
fn fixture(root: &Path, n: u64) -> Result<Value> {
    let t = Instant::now();
    let mut out = BufWriter::with_capacity(1 << 20, File::create(root.join("people.jsonl"))?);
    let mut crc = 0;
    for id in 1..=n {
        let d = person(id);
        update_crc(&mut crc, &d)?;
        serde_json::to_writer(&mut out, &d)?;
        out.write_all(b"\n")?;
        if id % PROGRESS == 0 {
            log(format!(
                "fixture {id}/{n} {:.1}s",
                t.elapsed().as_secs_f64()
            ));
        }
    }
    out.flush()?;
    out.get_ref().sync_all()?;
    Ok(
        json!({"rows":n,"base_crc32c":crc,"bytes":root.join("people.jsonl").metadata()?.len(),"generation_seconds":t.elapsed().as_secs_f64()}),
    )
}
fn each(root: &Path, mut f: impl FnMut(u64, Value) -> Result<()>) -> Result<u64> {
    let mut input = BufReader::with_capacity(1 << 20, File::open(root.join("people.jsonl"))?);
    let mut line = String::new();
    let mut id = 0;
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        id += 1;
        f(id, serde_json::from_str(&line)?)?;
    }
    Ok(id)
}
fn samples(n: u64) -> Vec<u64> {
    let mut ids = vec![1, n, 255, 256, 65535, 65536, 16_777_215, 16_777_216];
    ids.extend((0..64).map(|i| 1 + (i * 104729) % n));
    ids.retain(|i| *i <= n);
    ids.sort_unstable();
    ids.dedup();
    ids
}
fn remove_stamps(d: &mut Value, lo: u64, hi: u64) -> Result<()> {
    let map = d.as_object_mut().ok_or("not an object")?;
    let c = map
        .remove("_created_unix")
        .and_then(|v| v.as_u64())
        .ok_or("created timestamp absent")?;
    let u = map
        .remove("_updated_unix")
        .and_then(|v| v.as_u64())
        .ok_or("updated timestamp absent")?;
    if c != u || c < lo || c > hi {
        return Err("invalid automatic timestamps".into());
    }
    Ok(())
}
fn load_e4(root: &Path, n: u64, want_crc: u32, stamps: bool) -> Result<Value> {
    let arm = if stamps { "e4_timestamps" } else { "e4" };
    let dir = root.join(arm);
    fs::create_dir(&dir)?;
    let l = layout(stamps);
    let started = now();
    let timer = Instant::now();
    let mut s = Store::create(&dir, config())?;
    let descriptor = l.descriptor()?;
    for i in 0..3 {
        s.put(&cat(i), &descriptor)?;
    }
    let (mut payload, mut peak) = (0u64, 0u64);
    let count = each(root, |id, mut d| {
        if stamps {
            stamp_fresh(&mut d, now());
        }
        let e = encode_dense_v3(&l, &d)?;
        payload += e.row.len() as u64;
        s.put(&key(id), &e.row)?;
        if id % BATCH == 0 {
            s.commit()?;
        }
        if id % PROGRESS == 0 {
            peak = peak.max(size(&dir)?);
            log(format!(
                "{arm} load {id}/{n} {:.1}s footprint={}B",
                timer.elapsed().as_secs_f64(),
                peak
            ));
        }
        Ok(())
    })?;
    assert_eq!(count, n);
    if n % BATCH != 0 {
        s.commit()?;
    }
    peak = peak.max(size(&dir)?);
    let before_checkpoint = timer.elapsed().as_secs_f64();
    s.checkpoint()?;
    drop(s);
    let load_seconds = timer.elapsed().as_secs_f64();
    let finished = now();
    let bytes = size(&dir)?;
    let files = file_sizes(&dir)?;
    peak = peak.max(bytes);
    log(format!(
        "{arm} LOADED rows={n} seconds={load_seconds:.3} bytes={bytes}"
    ));
    // Persist load evidence before the separate full read/verification pass.
    let mut result = json!({"arm":arm,"rows":n,"load_seconds":load_seconds,"checkpoint_close_seconds":load_seconds-before_checkpoint,"bytes":bytes,"bytes_per_row":bytes as f64/n as f64,"files":files,"sampled_peak_bytes":peak,"payload_bytes":payload,"started_unix":started,"finished_unix":finished,"verified":false});
    fs::write(
        root.join(format!("{arm}.json")),
        serde_json::to_vec_pretty(&result)?,
    )?;
    let check = Instant::now();
    let s = Store::open(&dir, config())?;
    let l = Layout::from_descriptor(&s.get(&cat(0))?.ok_or("missing catalog")?)?;
    let (mut seen, mut crc) = (0u64, 0u32);
    for item in s.scan(&[0x81])? {
        let (k, b) = item?;
        let id = unkey(&k)?;
        seen += 1;
        if id != seen {
            return Err("IDs out of order or missing".into());
        }
        let mut d = decode_dense_v3(&l, &b, |_| Err("unexpected vector".into()))?;
        if stamps {
            remove_stamps(&mut d, started, finished)?;
        }
        update_crc(&mut crc, &d)?;
        if seen % 1_000_000 == 0 {
            log(format!(
                "{arm} verify {seen}/{n} {:.1}s",
                check.elapsed().as_secs_f64()
            ));
        }
    }
    assert_eq!(seen, n);
    assert_eq!(crc, want_crc);
    for id in samples(n) {
        let mut got = decode_dense_v3(&l, &s.get(&key(id))?.ok_or("sample absent")?, |_| {
            Err("unexpected vector".into())
        })?;
        if stamps {
            remove_stamps(&mut got, started, finished)?;
        }
        assert_eq!(got, person(id));
    }
    let (records, pages) = kernel::verify::verify_published_tree(
        &dir.join("data"),
        IoMode::Buffered,
        s.published_root(),
        1,
    )?;
    assert_eq!(records, n + 3);
    drop(s);
    result["verified"] = json!(true);
    result["base_crc32c"] = json!(crc);
    result["verified_rows"] = json!(seen);
    result["tree_pages"] = json!(pages);
    result["verify_seconds"] = json!(check.elapsed().as_secs_f64());
    fs::write(
        root.join(format!("{arm}.json")),
        serde_json::to_vec_pretty(&result)?,
    )?;
    log(format!("{arm} VERIFIED {seen}"));
    Ok(result)
}
fn sqlite_doc(r: &rusqlite::Row<'_>) -> Result<(u64, Value)> {
    let id: u64 = r.get(0)?;
    let mut d = json!({"_key":r.get::<_,String>(1)?,"fullname":r.get::<_,String>(2)?,"born":r.get::<_,i64>(3)?,"born_year":r.get::<_,i64>(4)?,"active":r.get::<_,bool>(5)?,"income":r.get::<_,Option<f64>>(6)?,"location":{"type":"Point","coordinates":[r.get::<_,f64>(7)?,r.get::<_,f64>(8)?]},"profile":serde_json::from_str::<Value>(&r.get::<_,String>(9)?)?});
    let extras: Value = serde_json::from_str(&r.get::<_, String>(10)?)?;
    for (k, v) in extras.as_object().ok_or("extras not object")? {
        d[k] = v.clone();
    }
    Ok((id, d))
}
fn load_sqlite(root: &Path, n: u64, want_crc: u32) -> Result<Value> {
    let dir = root.join("sqlite");
    fs::create_dir(&dir)?;
    let timer = Instant::now();
    let c = Connection::open(dir.join("data.sqlite"))?;
    c.execute_batch("PRAGMA page_size=4096; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA wal_autocheckpoint=0; PRAGMA temp_store=FILE; CREATE TABLE person(id INTEGER PRIMARY KEY,key TEXT,fullname TEXT,born INTEGER,born_year INTEGER,active INTEGER,income REAL,lon REAL,lat REAL,profile BLOB,extras BLOB); BEGIN IMMEDIATE;")?;
    let mut peak = 0;
    let l = layout(false);
    {
        let mut insert =
            c.prepare("INSERT INTO person VALUES (?,?,?,?,?,?,?,?,?,jsonb(?),jsonb(?))")?;
        let count = each(root, |id, d| {
            let extras: Map<String, Value> = d
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, _)| !l.fields.iter().any(|(name, _)| name == *k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            insert.execute(params![
                id,
                d["_key"].as_str().unwrap(),
                d["fullname"].as_str().unwrap(),
                d["born"].as_i64().unwrap(),
                d["born_year"].as_i64().unwrap(),
                d["active"].as_bool().unwrap(),
                d["income"].as_f64(),
                d["location"]["coordinates"][0].as_f64().unwrap(),
                d["location"]["coordinates"][1].as_f64().unwrap(),
                serde_json::to_string(&d["profile"])?,
                serde_json::to_string(&extras)?
            ])?;
            if id % BATCH == 0 {
                c.execute_batch("COMMIT")?;
                if id < n {
                    c.execute_batch("BEGIN IMMEDIATE")?;
                }
            }
            if id % PROGRESS == 0 {
                peak = peak.max(size(&dir)?);
                log(format!(
                    "sqlite load {id}/{n} {:.1}s footprint={}B",
                    timer.elapsed().as_secs_f64(),
                    peak
                ));
            }
            Ok(())
        })?;
        assert_eq!(count, n);
    }
    if n % BATCH != 0 {
        c.execute_batch("COMMIT")?;
    }
    peak = peak.max(size(&dir)?);
    let before_checkpoint = timer.elapsed().as_secs_f64();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(c);
    let load_seconds = timer.elapsed().as_secs_f64();
    let bytes = size(&dir)?;
    peak = peak.max(bytes);
    log(format!(
        "sqlite LOADED rows={n} seconds={load_seconds:.3} bytes={bytes}"
    ));
    let mut result = json!({"arm":"sqlite","rows":n,"load_seconds":load_seconds,"checkpoint_close_seconds":load_seconds-before_checkpoint,"bytes":bytes,"bytes_per_row":bytes as f64/n as f64,"files":file_sizes(&dir)?,"sampled_peak_bytes":peak,"verified":false});
    fs::write(
        root.join("sqlite.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    let check = Instant::now();
    let c = Connection::open(dir.join("data.sqlite"))?;
    c.execute_batch("PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
    let sql="SELECT id,key,fullname,born,born_year,active,income,lon,lat,json(profile),json(extras) FROM person";
    let (mut seen, mut crc) = (0u64, 0u32);
    {
        let mut st = c.prepare(&format!("{sql} ORDER BY id"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let (id, d) = sqlite_doc(r)?;
            seen += 1;
            assert_eq!(id, seen);
            update_crc(&mut crc, &d)?;
            if seen % 1_000_000 == 0 {
                log(format!(
                    "sqlite verify {seen}/{n} {:.1}s",
                    check.elapsed().as_secs_f64()
                ));
            }
        }
    }
    assert_eq!(seen, n);
    assert_eq!(crc, want_crc);
    {
        let mut st = c.prepare(&format!("{sql} WHERE id=?"))?;
        for id in samples(n) {
            let mut rows = st.query([id])?;
            let (_, got) = sqlite_doc(rows.next()?.ok_or("sample missing")?)?;
            assert_eq!(got, person(id));
        }
    }
    let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    assert_eq!(integrity, "ok");
    let version: String = c.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    result["verified"] = json!(true);
    result["base_crc32c"] = json!(crc);
    result["verified_rows"] = json!(seen);
    result["sqlite_version"] = json!(version);
    result["verify_seconds"] = json!(check.elapsed().as_secs_f64());
    fs::write(
        root.join("sqlite.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    log(format!("sqlite VERIFIED {seen}"));
    Ok(result)
}
fn main() -> Result<()> {
    if !cfg!(all(feature = "sqlite-balance", feature = "compact-cells")) {
        return Err("enable sqlite-balance,compact-cells".into());
    }
    let n: u64 = std::env::args()
        .nth(1)
        .unwrap_or("20000000".into())
        .parse()?;
    if n == 0 {
        return Err("rows must be positive".into());
    }
    let root = PathBuf::from(format!("<scratch>", now()));
    fs::create_dir(&root)?;
    let tmp = root.join("tmp");
    fs::create_dir(&tmp)?;
    std::env::set_var("TMPDIR", &tmp);
    std::env::set_var("SQLITE_TMPDIR", &tmp);
    log(format!("RUN {}", root.display()));
    let f = fixture(&root, n)?;
    let crc = f["base_crc32c"].as_u64().unwrap() as u32;
    let mut manifest = json!({"rows":n,"fixture":f,"insertion_order":"ascending integer ID","commit_rows":BATCH,"cache_bytes":8<<20,"page_bytes":4096,"sync":"FULL + macOS fullfsync","features":["sqlite-balance","compact-cells"],"codec":"dense_v3","sqlite_has_timestamps":false,"arms":[]});
    fs::write(
        root.join("results.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    for arm in 0..3 {
        let result = match arm {
            0 => load_e4(&root, n, crc, false)?,
            1 => load_e4(&root, n, crc, true)?,
            _ => load_sqlite(&root, n, crc)?,
        };
        manifest["arms"].as_array_mut().unwrap().push(result);
        fs::write(
            root.join("results.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
    }
    log(format!("COMPLETE {}", root.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamps_are_typed_and_base_document_roundtrips() {
        for id in [1, 10, 21, 65536, 16_777_216, 20_000_000] {
            let original = person(id);
            let mut d = original.clone();
            stamp_fresh(&mut d, 1_788_900_000);
            let l = layout(true);
            let row = encode_dense_v3(&l, &d).unwrap();
            let mut got = decode_dense_v3(&l, &row.row, |_| Err("no vector".into())).unwrap();
            assert_eq!(got, d);
            remove_stamps(&mut got, 1_788_900_000, 1_788_900_000).unwrap();
            assert_eq!(got, original);
            assert_eq!(unkey(&key(id)).unwrap(), id);
        }
    }
}
