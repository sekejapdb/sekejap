//! P1 entity-only storage ablation. No graph/index claims.
use sekejap_core::*;
use kernel::{
    io::IoMode,
    keys,
    store::{Config, Store, SyncMode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn layout(shape: &str) -> Layout {
    let mut fields = vec![
        ("_key".into(), Kind::Text),
        ("fullname".into(), Kind::Text),
        ("born".into(), Kind::Int),
        ("born_year".into(), Kind::Int),
        ("location".into(), Kind::Geo),
    ];
    if shape != "scalar" {
        fields.push(("profile".into(), Kind::Json));
    }
    Layout { id: 1, fields }
}
fn row(id: u64, shape: &str) -> Value {
    let mut d = json!({"_key":format!("p{id:08}"),"fullname":format!("Person {id:08}"),"born":(1940+id%80)*10000+(1+id%12)*100+1+id%28,"born_year":1940+id%80,"location":{"type":"Point","coordinates":[144.0+(id*37%2000)as f64/1000.0,-38.5+(id*53%2000)as f64/1000.0]}});
    if shape != "scalar" {
        d["profile"] = json!({"languages":["id","en"],"preferences":{"quiet":id%2==0,"score":(id%100)as f64/4.0},"nested":[null,true,{"count":id%17,"unicode":"東京-é"}]});
        if id % 3 == 0 {
            d["source"] = json!("survey");
        }
        if id % 7 == 0 {
            d["extra"] = json!({"unknown":[1,null,"x"],"large":u64::MAX});
        }
        if shape == "large_json" && id % 100 == 0 {
            d["profile"]["note"] = json!("observations-é-".repeat(430));
        }
    }
    d
}
fn key(id: u64, stage: u8) -> Vec<u8> {
    if stage < 2 {
        return keys::node(id);
    }
    let bytes = id.to_be_bytes();
    let start = bytes.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = if stage >= 3 {
        vec![0x80 + (8 - start) as u8]
    } else {
        vec![1, (8 - start) as u8]
    };
    k.extend_from_slice(&bytes[start..]);
    k
}
fn unkey(b: &[u8], stage: u8) -> Result<u64> {
    if stage >= 3 {
        let n = (*b.first().ok_or("ID tag")?)
            .checked_sub(0x80)
            .ok_or("ID tag")? as usize;
        if !(1..=8).contains(&n) || b.len() != n + 1 {
            return Err("compact ID length".into());
        }
        let mut full = [0; 8];
        full[8 - n..].copy_from_slice(&b[1..]);
        return Ok(u64::from_be_bytes(full));
    }
    if b.first() != Some(&1) {
        return Err("entity key tag".into());
    }
    let b = if stage < 2 {
        if b.len() != 9 {
            return Err("fixed ID length".into());
        }
        &b[1..]
    } else {
        let n = *b.get(1).ok_or("compact ID length")? as usize;
        if !(1..=8).contains(&n) || b.len() != n + 2 {
            return Err("compact ID length".into());
        }
        &b[2..]
    };
    let mut all = [0; 8];
    all[8 - b.len()..].copy_from_slice(b);
    Ok(u64::from_be_bytes(all))
}
fn cat(copy: u64, stage: u8) -> Vec<u8> {
    if stage == 0 {
        keys::sqlmeta_key(240, copy)
    } else {
        let mut k = vec![0, 240];
        k.extend_from_slice(&copy.to_be_bytes());
        k
    }
}
fn file_size(p: &Path) -> u64 {
    if p.is_file() {
        p.metadata().unwrap().len()
    } else {
        fs::read_dir(p)
            .unwrap()
            .map(|x| file_size(&x.unwrap().path()))
            .sum()
    }
}
fn each(input: &Path, mut cb: impl FnMut(u64, Value) -> Result<()>) -> Result<()> {
    for line in BufReader::new(File::open(input)?).lines() {
        let d: Value = serde_json::from_str(&line?)?;
        cb(d["id"].as_u64().unwrap(), d["doc"].clone())?;
    }
    Ok(())
}
fn fixtures(input: &Path, n: u64, shape: &str, order: &str) -> Result<()> {
    // Bounded arithmetic permutation: odd multiplier coprime with 10K/40K.
    let mut f = BufWriter::new(File::create(input)?);
    for pos in 0..n {
        let id = if order == "ordered" {
            pos + 1
        } else {
            1 + (pos * 7919 + 1237) % n
        };
        writeln!(f, "{}", json!({"id":id,"doc":row(id,shape)}))?;
    }
    f.flush()?;
    Ok(())
}
fn e4(path: &Path, input: &Path, n: u64, shape: &str, stage: u8) -> Result<Value> {
    let mut l = layout(shape);
    if stage >= 3 {
        l.fields[4].1 = Kind::Point;
    }
    let t = Instant::now();
    let mut s = Store::create(path, config())?;
    let desc = l.descriptor()?;
    for i in 0..3 {
        s.put(&cat(i, stage), &desc)?;
    }
    let (mut count, mut payload) = (0, 0);
    each(input, |id, doc| {
        let e = if stage >= 5 {
            encode_dense_v3(&l, &doc)?
        } else if stage >= 4 {
            encode_dense_v2(&l, &doc)?
        } else if stage >= 3 {
            encode_dense(&l, &doc)?
        } else {
            encode(&l, &doc)?
        };
        payload += e.row.len();
        s.put(&key(id, stage), &e.row)?;
        count += 1;
        if count % 1000 == 0 {
            s.commit()?;
        }
        Ok(())
    })?;
    if count % 1000 != 0 {
        s.commit()?;
    }
    s.checkpoint()?;
    let insert = t.elapsed().as_secs_f64();
    drop(s);
    let size = file_size(path);
    let s = Store::open(path, config())?;
    let desc = s.get(&cat(0, stage))?.ok_or("missing layout")?;
    let l = Layout::from_descriptor(&desc)?;
    let t = Instant::now();
    let (mut seen, mut crc) = (0, 0);
    for r in s.scan(&[if stage >= 3 { 0x81 } else { 1 }])? {
        let (k, b) = r?;
        if (stage < 3 && k[0] != 1) || (stage >= 3 && !(0x81..=0x88).contains(&k[0])) {
            break;
        }
        let id = unkey(&k, stage)?;
        let d = if stage >= 5 {
            decode_dense_v3(&l, &b, |_| Err("no vectors".into()))?
        } else if stage >= 4 {
            decode_dense_v2(&l, &b, |_| Err("no vectors".into()))?
        } else if stage >= 3 {
            decode_dense(&l, &b, |_| Err("no vectors".into()))?
        } else {
            decode(&l, &b, |_| Err("no vectors".into()))?
        };
        crc = crc32c::crc32c_append(crc, &serde_json::to_vec(&d)?);
        std::hint::black_box(id);
        seen += 1;
    }
    let output = t.elapsed().as_secs_f64();
    assert_eq!(seen, n);
    each(input, |id, want| {
        let b = s.get(&key(id, stage))?.ok_or("missing entity")?;
        let got = if stage >= 5 {
            decode_dense_v3(&l, &b, |_| Err("no vector".into()))?
        } else if stage >= 4 {
            decode_dense_v2(&l, &b, |_| Err("no vector".into()))?
        } else if stage >= 3 {
            decode_dense(&l, &b, |_| Err("no vector".into()))?
        } else {
            decode(&l, &b, |_| Err("no vector".into()))?
        };
        if got != want {
            return Err(format!("entity mismatch {id}").into());
        }
        Ok(())
    })?;
    let (rows, pages) = kernel::verify::verify_published_tree(
        &path.join("data"),
        IoMode::Buffered,
        s.published_root(),
        1,
    )?;
    Ok(
        json!({"bytes":size,"insert_seconds":insert,"output_seconds":output,"crc32c":crc,"payload_bytes":payload,"verified_rows":seen,"tree_records":rows,"tree_pages":pages}),
    )
}
fn sqlite(path: &Path, input: &Path, n: u64, shape: &str) -> Result<Value> {
    let t = Instant::now();
    let c = Connection::open(path.join("data.sqlite"))?;
    c.execute_batch("PRAGMA page_size=4096; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA wal_autocheckpoint=0; PRAGMA temp_store=FILE;")?;
    let has = shape != "scalar";
    c.execute_batch(if has{"CREATE TABLE person(id INTEGER PRIMARY KEY, key TEXT, fullname TEXT, born INTEGER, born_year INTEGER, lon REAL, lat REAL, profile BLOB, extras BLOB); BEGIN IMMEDIATE;"}else{"CREATE TABLE person(id INTEGER PRIMARY KEY, key TEXT, fullname TEXT, born INTEGER, born_year INTEGER, lon REAL, lat REAL); BEGIN IMMEDIATE;"})?;
    let l = layout(shape);
    let mut count = 0;
    {
        let mut ins = c.prepare(if has {
            "INSERT INTO person VALUES (?,?,?,?,?,?,?,jsonb(?),jsonb(?))"
        } else {
            "INSERT INTO person VALUES (?,?,?,?,?,?,?)"
        })?;
        each(input, |id, d| {
            let args = params![
                id,
                d["_key"].as_str().unwrap(),
                d["fullname"].as_str().unwrap(),
                d["born"].as_i64().unwrap(),
                d["born_year"].as_i64().unwrap(),
                d["location"]["coordinates"][0].as_f64().unwrap(),
                d["location"]["coordinates"][1].as_f64().unwrap()
            ];
            if has {
                let extra: serde_json::Map<String, Value> = d
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(k, _)| !l.fields.iter().any(|(name, _)| name == *k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                ins.execute(params![
                    id,
                    d["_key"].as_str().unwrap(),
                    d["fullname"].as_str().unwrap(),
                    d["born"].as_i64().unwrap(),
                    d["born_year"].as_i64().unwrap(),
                    d["location"]["coordinates"][0].as_f64().unwrap(),
                    d["location"]["coordinates"][1].as_f64().unwrap(),
                    serde_json::to_string(&d["profile"])?,
                    serde_json::to_string(&extra)?
                ])?;
            } else {
                ins.execute(args)?;
            }
            count += 1;
            if count % 1000 == 0 {
                c.execute_batch("COMMIT")?;
                if count < n {
                    c.execute_batch("BEGIN IMMEDIATE")?;
                }
            }
            Ok(())
        })?;
    }
    if count % 1000 != 0 {
        c.execute_batch("COMMIT")?;
    }
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let insert = t.elapsed().as_secs_f64();
    drop(c);
    let size = file_size(path);
    let c = Connection::open(path.join("data.sqlite"))?;
    c.execute_batch("PRAGMA cache_size=-8192; PRAGMA mmap_size=0;")?;
    let sql = if has {
        "SELECT id,key,fullname,born,born_year,lon,lat,json(profile),json(extras) FROM person ORDER BY id"
    } else {
        "SELECT id,key,fullname,born,born_year,lon,lat FROM person ORDER BY id"
    };
    let read = |r: &rusqlite::Row<'_>| -> Result<(u64, Value)> {
        let id = r.get(0)?;
        let mut d = json!({"_key":r.get::<_,String>(1)?,"fullname":r.get::<_,String>(2)?,"born":r.get::<_,i64>(3)?,"born_year":r.get::<_,i64>(4)?,"location":{"type":"Point","coordinates":[r.get::<_,f64>(5)?,r.get::<_,f64>(6)?]}});
        if has {
            d["profile"] = serde_json::from_str(&r.get::<_, String>(7)?)?;
            let e: Value = serde_json::from_str(&r.get::<_, String>(8)?)?;
            for (k, v) in e.as_object().unwrap() {
                d[k] = v.clone();
            }
        }
        Ok((id, d))
    };
    let t = Instant::now();
    let (mut seen, mut crc) = (0, 0);
    {
        let mut st = c.prepare(sql)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let (_, d) = read(r)?;
            crc = crc32c::crc32c_append(crc, &serde_json::to_vec(&d)?);
            seen += 1;
        }
    }
    let output = t.elapsed().as_secs_f64();
    assert_eq!(seen, n);
    let point_sql = sql.replace("ORDER BY id", "WHERE id=?");
    let mut st = c.prepare(&point_sql)?;
    each(input, |id, want| {
        let mut rs = st.query([id])?;
        let (_, got) = read(rs.next()?.ok_or("missing SQLite entity")?)?;
        if got != want {
            return Err(format!("SQLite mismatch {id}").into());
        }
        Ok(())
    })?;
    let check: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    assert_eq!(check, "ok");
    Ok(
        json!({"bytes":size,"insert_seconds":insert,"output_seconds":output,"crc32c":crc,"verified_rows":seen}),
    )
}
fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let stage: u8 = a.get(1).ok_or("stage 0..5 required")?.parse()?;
    if stage > 5 {
        return Err("stage out of range".into());
    }
    let reps: usize = a.get(2).map(|s| s.parse()).transpose()?.unwrap_or(1);
    let base = PathBuf::from("<scratch>");
    let run = base.join(format!(
        "entry-s{stage}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
    ));
    fs::create_dir(&run)?;
    let tmp = run.join("tmp");
    fs::create_dir(&tmp)?;
    std::env::set_var("TMPDIR", &tmp);
    std::env::set_var("SQLITE_TMPDIR", &tmp);
    println!("RUN {}", run.display());
    let mut results = Vec::new();
    for n in [10_000, 40_000] {
        for shape in ["scalar", "json", "large_json"] {
            for order in ["ordered", "shuffled"] {
                let input = run.join(format!("{n}-{shape}-{order}.jsonl"));
                fixtures(&input, n, shape, order)?;
                for rep in 0..reps {
                    let dir = run.join(format!("n{n}-{shape}-{order}-r{rep}"));
                    fs::create_dir(&dir)?;
                    let ep = dir.join("e4");
                    let sp = dir.join("sqlite");
                    fs::create_dir(&ep)?;
                    fs::create_dir(&sp)?;
                    let (e, s) = if rep % 2 == 0 {
                        (
                            e4(&ep, &input, n, shape, stage)?,
                            sqlite(&sp, &input, n, shape)?,
                        )
                    } else {
                        let s = sqlite(&sp, &input, n, shape)?;
                        (e4(&ep, &input, n, shape, stage)?, s)
                    };
                    assert_eq!(e["crc32c"], s["crc32c"]);
                    let ratio = e["bytes"].as_f64().unwrap() / s["bytes"].as_f64().unwrap();
                    println!(
                        "PASS n={n} {shape} {order} rep={rep} ratio={ratio:.4} e4={} sqlite={}",
                        e["bytes"], s["bytes"]
                    );
                    std::io::stdout().flush()?;
                    results.push(json!({"n":n,"shape":shape,"order":order,"rep":rep,"ratio":ratio,"e4":e,"sqlite":s}));
                    fs::write(
                        run.join("results.json"),
                        serde_json::to_vec_pretty(
                            &json!({"stage":stage,"sibling_balance":cfg!(feature="sqlite-balance"),"compact_cells":cfg!(feature="compact-cells"),"results":results}),
                        )?,
                    )?;
                }
            }
        }
    }
    println!("COMPLETE {}", run.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_ids_are_ordered_and_roundtrip() {
        for stage in [2, 3, 5] {
            let mut prior = vec![];
            for id in [
                0,
                1,
                127,
                128,
                255,
                256,
                65535,
                65536,
                u32::MAX as u64,
                u64::MAX,
            ] {
                let k = key(id, stage);
                assert!(k > prior);
                assert_eq!(unkey(&k, stage).unwrap(), id);
                prior = k;
            }
            assert_eq!(key(40000, 2).len(), 4);
            assert_eq!(key(40000, 5).len(), 3);
        }
    }
}
