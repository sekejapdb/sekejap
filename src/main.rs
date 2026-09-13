use e4_prototype::*;
use kernel::{
    io::IoMode,
    keys,
    store::{Config, Store, SyncMode},
};
use rusqlite::{params, Connection, Row};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CACHE: usize = 8 << 20;
const BATCH: u64 = 1000;
const VFIELD: usize = 6;
fn cfg() -> Config {
    Config {
        budget_bytes: CACHE,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
fn layout(dim: usize) -> Layout {
    Layout {
        id: 1,
        fields: vec![
            ("_key".into(), Kind::Text),
            ("name".into(), Kind::Text),
            ("born".into(), Kind::Int),
            ("profile".into(), Kind::Json),
            ("location".into(), Kind::Geo),
            ("nickname".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(dim)),
        ],
    }
}
fn cat_key(copy: u64) -> Vec<u8> {
    keys::sqlmeta_key(240, copy)
}
fn install(s: &mut Store, l: &Layout) -> Result<()> {
    let d = l.descriptor()?;
    for c in 0..3 {
        s.put(&cat_key(c), &d)?;
    }
    Ok(())
}
fn read_layout(s: &Store) -> Result<Layout> {
    let mut good = None;
    for c in 0..3 {
        if let Ok(Some(b)) = s.get(&cat_key(c)) {
            if let Ok(l) = Layout::from_descriptor(&b) {
                if good.as_ref().is_some_and(|g| g != &l) {
                    return Err("conflicting layout copies".into());
                }
                good = Some(l);
            }
        }
    }
    good.ok_or_else(|| "all layout copies unavailable".into())
}
fn ext(k: &str) -> Vec<u8> {
    let mut v = vec![keys::TAG_EXT];
    v.extend_from_slice(k.as_bytes());
    v
}
fn birth(b: i64) -> u64 {
    (b as u64) ^ 0x8000_0000_0000_0000
}
fn size(p: &Path) -> u64 {
    if p.is_file() {
        return p.metadata().unwrap().len();
    }
    fs::read_dir(p)
        .unwrap()
        .map(|e| size(&e.unwrap().path()))
        .sum()
}
fn fixture(path: &Path, n: u64, dim: usize) -> Result<u64> {
    let mut f = BufWriter::new(File::create(path)?);
    let mut hash = 0;
    for id in 1..=n {
        let mut rng = id.wrapping_mul(6364136223846793005).wrapping_add(17);
        let v: Vec<f64> = (0..dim)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (((rng >> 32) as u32 as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32) as f64
            })
            .collect();
        let mut profile = json!({"languages":if id%3==0 {vec!["id","en"]}else{vec!["en"]},"preferences":{"quiet":id%2==0,"tags":["garden",format!("group-{}",id%11)]},"score":(id%100) as f64/4.0,"nested":[null,true,{"unicode":"Māya 東京","count":id%17}]});
        if id % 100 == 0 {
            profile["long_note"] = Value::String("observations-é-".repeat(430));
        }
        let mut doc = json!({"_key":format!("p{id:08}"),"name":format!("Person {id:08}"),"born":(1940+id%80)*10000+(1+id%12)*100+1+id%28,"profile":profile,"location":{"type":"Point","coordinates":[144.0+(id*37%2000) as f64/1000.0,-38.5+(id*53%2000) as f64/1000.0]},"embedding":v});
        match id % 3 {
            0 => doc["nickname"] = Value::Null,
            1 => doc["nickname"] = json!(format!("N{id}")),
            _ => {}
        }
        if id % 4 != 0 {
            doc["source"] = json!(if id % 2 == 0 { "survey" } else { "sensor" });
        }
        if id % 7 == 0 {
            doc["extra"] = json!({"nullable":null,"array":[1,"x",false],"large_integer":u64::MAX});
        }
        let line = serde_json::to_vec(&json!({"id":id,"doc":doc,"edges":edges(id,n)}))?;
        hash = crc32c::crc32c_append(hash, &line);
        f.write_all(&line)?;
        f.write_all(b"\n")?;
    }
    f.flush()?;
    Ok(hash as u64)
}
fn edges(id: u64, n: u64) -> [u64; 2] {
    [1 + id % n, 1 + (id + 96) % n]
}
fn each(path: &Path, mut f: impl FnMut(u64, Value, [u64; 2]) -> Result<()>) -> Result<()> {
    for line in BufReader::new(File::open(path)?).lines() {
        let v: Value = serde_json::from_str(&line?)?;
        f(
            v["id"].as_u64().unwrap(),
            v["doc"].clone(),
            serde_json::from_value(v["edges"].clone())?,
        )?;
    }
    Ok(())
}
fn get_e4(s: &Store, l: &Layout, id: u64) -> Result<Value> {
    let b = s.get(&keys::node(id))?.ok_or("missing entity")?;
    decode(l, &b, |f| {
        s.get(&keys::vec_key(f as u64, id))?
            .ok_or_else(|| "missing vector".into())
    })
}
fn put_e4(s: &mut Store, l: &Layout, id: u64, doc: &Value, edges: [u64; 2]) -> Result<usize> {
    let e = encode(l, doc)?;
    let bytes = e.row.len();
    s.put(&keys::node(id), &e.row)?;
    for (f, b) in e.vectors {
        s.put(&keys::vec_key(f as u64, id), &b)?;
    }
    s.put(&ext(doc["_key"].as_str().unwrap()), &id.to_be_bytes())?;
    s.put(
        &keys::prop(2, birth(doc["born"].as_i64().unwrap()), id),
        &[],
    )?;
    for dst in edges {
        s.put(&keys::edge(0, id, 1, dst), &[])?;
        s.put(&keys::redge(0, dst, 1, id), &[])?;
    }
    Ok(bytes)
}
fn sql_open(path: &Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.execute_batch("PRAGMA page_size=4096; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA wal_autocheckpoint=0; PRAGMA temp_store=FILE;")?;
    Ok(c)
}
const SELECT:&str="SELECT id, key, name, born, json(profile), lon, lat, nickname, nickname_present, embedding, json(extras) FROM person";
fn sql_doc(r: &Row<'_>, dim: usize) -> Result<(u64, Value)> {
    let id: u64 = r.get(0)?;
    let mut v = json!({"_key":r.get::<_,String>(1)?,"name":r.get::<_,String>(2)?,"born":r.get::<_,i64>(3)?,"profile":serde_json::from_str::<Value>(&r.get::<_,String>(4)?)?,"location":{"type":"Point","coordinates":[r.get::<_,f64>(5)?,r.get::<_,f64>(6)?]},"embedding":vector_json(&r.get::<_,Vec<u8>>(9)?,dim)?});
    if r.get::<_, bool>(8)? {
        v["nickname"] = r
            .get::<_, Option<String>>(7)?
            .map(Value::String)
            .unwrap_or(Value::Null);
    }
    let extras: Value = serde_json::from_str(&r.get::<_, String>(10)?)?;
    for (k, x) in extras.as_object().unwrap() {
        v[k] = x.clone();
    }
    Ok((id, v))
}
fn get_sql(c: &Connection, dim: usize, id: u64) -> Result<Value> {
    let mut st = c.prepare_cached(&format!("{SELECT} WHERE id=?"))?;
    let mut rows = st.query([id])?;
    Ok(sql_doc(rows.next()?.ok_or("missing sqlite row")?, dim)?.1)
}
fn load_e4(path: &Path, input: &Path, n: u64, dim: usize) -> Result<Value> {
    let t = Instant::now();
    let mut s = Store::create(path, cfg())?;
    let l = layout(dim);
    install(&mut s, &l)?;
    let mut peak = 0;
    let mut payload = 0u64;
    each(input, |id, doc, es| {
        payload += put_e4(&mut s, &l, id, &doc, es)? as u64;
        if id % BATCH == 0 {
            s.commit()?;
            peak = peak.max(size(path));
        }
        Ok(())
    })?;
    if n % BATCH != 0 {
        s.commit()?;
    }
    s.checkpoint()?;
    let elapsed = t.elapsed().as_secs_f64();
    peak = peak.max(size(path));
    drop(s);
    Ok(
        json!({"insert_seconds":elapsed,"bytes":size(path),"peak_sampled_bytes":peak,"entity_payload_bytes":payload}),
    )
}
fn load_sql(path: &Path, input: &Path, n: u64, dim: usize) -> Result<Value> {
    let t = Instant::now();
    let c = sql_open(&path.join("data.sqlite"))?;
    c.execute_batch("CREATE TABLE person(id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, name TEXT NOT NULL, born INTEGER NOT NULL, profile BLOB NOT NULL, lon REAL NOT NULL, lat REAL NOT NULL, nickname TEXT, nickname_present INTEGER NOT NULL, embedding BLOB NOT NULL, extras BLOB NOT NULL); CREATE INDEX person_born ON person(born); CREATE TABLE edge(src INTEGER,dst INTEGER,PRIMARY KEY(src,dst)) WITHOUT ROWID; CREATE INDEX edge_reverse ON edge(dst,src); BEGIN IMMEDIATE;")?;
    let mut peak = 0;
    let l = layout(dim);
    {
        let mut ins =
            c.prepare("INSERT INTO person VALUES (?,?,?,?,jsonb(?),?,?,?,?,?,jsonb(?))")?;
        let mut edge = c.prepare("INSERT INTO edge VALUES (?,?)")?;
        each(input, |id, doc, es| {
            let v: Vec<u8> = doc["embedding"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|v| (v.as_f64().unwrap() as f32).to_le_bytes())
                .collect();
            let extra: serde_json::Map<String, Value> = doc
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, _)| !l.fields.iter().any(|(name, _)| name == *k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            ins.execute(params![
                id,
                doc["_key"].as_str().unwrap(),
                doc["name"].as_str().unwrap(),
                doc["born"].as_i64().unwrap(),
                serde_json::to_string(&doc["profile"])?,
                doc["location"]["coordinates"][0].as_f64().unwrap(),
                doc["location"]["coordinates"][1].as_f64().unwrap(),
                doc.get("nickname").and_then(Value::as_str),
                doc.get("nickname").is_some(),
                v,
                serde_json::to_string(&extra)?
            ])?;
            for dst in es {
                edge.execute(params![id, dst])?;
            }
            if id % BATCH == 0 {
                c.execute_batch("COMMIT;")?;
                peak = peak.max(size(path));
                if id < n {
                    c.execute_batch("BEGIN IMMEDIATE;")?;
                }
            }
            Ok(())
        })?;
    }
    if n % BATCH != 0 {
        c.execute_batch("COMMIT;")?;
    }
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let elapsed = t.elapsed().as_secs_f64();
    peak = peak.max(size(path));
    drop(c);
    Ok(json!({"insert_seconds":elapsed,"bytes":size(path),"peak_sampled_bytes":peak}))
}
#[derive(Clone, Copy)]
struct Query {
    lo: i64,
    hi: i64,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
    quiet: bool,
}
fn query(i: usize) -> Query {
    Query {
        lo: 19500101 + (i as i64) * 30000,
        hi: 19991231 + (i as i64) * 10000,
        x0: 144.15 + i as f64 * 0.03,
        x1: 145.8,
        y0: -38.3,
        y1: -36.7 - i as f64 * 0.02,
        quiet: i % 2 == 0,
    }
}
fn matches(v: &Value, q: Query) -> bool {
    let b = v["born"].as_i64().unwrap();
    let x = v["location"]["coordinates"][0].as_f64().unwrap();
    let y = v["location"]["coordinates"][1].as_f64().unwrap();
    b >= q.lo
        && b <= q.hi
        && x >= q.x0
        && x <= q.x1
        && y >= q.y0
        && y <= q.y1
        && v["profile"]["preferences"]["quiet"].as_bool() == Some(q.quiet)
}
fn score(b: &[u8]) -> f64 {
    b.chunks_exact(4)
        .enumerate()
        .map(|(i, b)| {
            let x = f32::from_le_bytes(b.try_into().unwrap()) as f64;
            let q = ((i * 17 % 29) as f64 / 29.0 - 0.5) as f32 as f64;
            (x - q) * (x - q)
        })
        .sum()
}
fn rank(top: &mut Vec<(u64, f64)>, id: u64, s: f64) {
    top.push((id, s));
    top.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    top.truncate(10);
}
fn vector(v: &Value) -> Vec<u8> {
    v["embedding"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|x| (x.as_f64().unwrap() as f32).to_le_bytes())
        .collect()
}
fn graph_candidates(n: u64) -> BTreeSet<u64> {
    let mut ids = BTreeSet::new();
    for i in 0..64 {
        let src = 1 + (i * 619) % n;
        for a in edges(src, n) {
            for b in edges(a, n) {
                ids.insert(b);
            }
        }
    }
    ids
}
fn oracle(input: &Path, n: u64) -> Result<Vec<Vec<(u64, f64)>>> {
    let graph = graph_candidates(n);
    let mut tops = vec![Vec::new(); 9];
    each(input, |id, v, _| {
        for (i, top) in tops.iter_mut().enumerate() {
            if (i < 8 || graph.contains(&id)) && matches(&v, query(i % 8)) {
                rank(top, id, score(&vector(&v)));
            }
        }
        Ok(())
    })?;
    Ok(tops)
}
fn e4_hybrid(
    s: &Store,
    l: &Layout,
    q: Query,
    candidates: Option<&BTreeSet<u64>>,
    projection: bool,
) -> Result<Vec<(u64, f64)>> {
    let mut top = Vec::new();
    let mut visit = |id| -> Result<()> {
        let b = s.get(&keys::node(id))?.ok_or("hybrid node missing")?;
        let matched = if projection {
            let p = project(
                l,
                &b,
                &[
                    ("born", &[]),
                    ("profile", &["preferences", "quiet"]),
                    ("location", &[]),
                ],
            )?;
            let born = p[0]
                .as_ref()
                .and_then(Value::as_i64)
                .ok_or("projected born")?;
            let quiet = p[1]
                .as_ref()
                .and_then(Value::as_bool)
                .ok_or("projected quiet")?;
            let geo = p[2].as_ref().ok_or("projected location")?;
            let x = geo["coordinates"][0].as_f64().ok_or("projected x")?;
            let y = geo["coordinates"][1].as_f64().ok_or("projected y")?;
            born >= q.lo
                && born <= q.hi
                && quiet == q.quiet
                && x >= q.x0
                && x <= q.x1
                && y >= q.y0
                && y <= q.y1
        } else {
            matches(&decode_inline(l, &b)?.0, q)
        };
        if matched {
            let b = s
                .get(&keys::vec_key(VFIELD as u64, id))?
                .ok_or("hybrid vector missing")?;
            rank(&mut top, id, score(&b));
        }
        Ok(())
    };
    if let Some(ids) = candidates {
        for &id in ids {
            visit(id)?;
        }
    } else {
        let prefix = keys::prop_prefix(2);
        for r in s.scan(&keys::prop(2, birth(q.lo), 0))? {
            let (k, _) = r?;
            if !k.starts_with(&prefix) {
                break;
            }
            let b = u64::from_be_bytes(k[9..17].try_into()?);
            if b > birth(q.hi) {
                break;
            }
            visit(u64::from_be_bytes(k[17..25].try_into()?))?;
        }
    }
    Ok(top)
}
fn e4_neighbors(s: &Store, id: u64) -> Result<Vec<u64>> {
    let p = keys::edge_prefix(0, id);
    let mut out = Vec::new();
    for r in s.scan(&p)? {
        let (k, _) = r?;
        if !k.starts_with(&p) {
            break;
        }
        out.push(u64::from_be_bytes(k[k.len() - 8..].try_into()?));
    }
    Ok(out)
}
fn e4_graph(s: &Store, n: u64) -> Result<BTreeSet<u64>> {
    let mut out = BTreeSet::new();
    for i in 0..64 {
        for a in e4_neighbors(s, 1 + (i * 619) % n)? {
            out.extend(e4_neighbors(s, a)?);
        }
    }
    Ok(out)
}
fn sql_graph(c: &Connection, n: u64) -> Result<BTreeSet<u64>> {
    let mut out = BTreeSet::new();
    let mut st = c.prepare_cached(
        "SELECT e2.dst FROM edge e1 JOIN edge e2 ON e2.src=e1.dst WHERE e1.src=?",
    )?;
    for i in 0..64 {
        for id in st.query_map([1 + (i * 619) % n], |r| r.get::<_, u64>(0))? {
            out.insert(id?);
        }
    }
    Ok(out)
}
fn sql_hybrid(
    c: &Connection,
    q: Query,
    ids: Option<&BTreeSet<u64>>,
    dim: usize,
) -> Result<Vec<(u64, f64)>> {
    let mut top = Vec::new();
    if let Some(ids) = ids {
        for &id in ids {
            let v = get_sql(c, dim, id)?;
            if matches(&v, q) {
                rank(&mut top, id, score(&vector(&v)));
            }
        }
    } else {
        let mut st=c.prepare_cached("SELECT id,embedding FROM person INDEXED BY person_born WHERE born BETWEEN ?1 AND ?2 AND lon BETWEEN ?3 AND ?4 AND lat BETWEEN ?5 AND ?6 AND json_extract(profile,'$.preferences.quiet')=?7")?;
        let mut rows = st.query(params![q.lo, q.hi, q.x0, q.x1, q.y0, q.y1, q.quiet])?;
        while let Some(r) = rows.next()? {
            let b: Vec<u8> = r.get(1)?;
            rank(&mut top, r.get(0)?, score(&b));
        }
    }
    Ok(top)
}
fn checksum(v: &Value, sum: &mut u32, bytes: &mut u64) -> Result<()> {
    let b = serde_json::to_vec(v)?;
    *sum = crc32c::crc32c_append(*sum, &b);
    *bytes += b.len() as u64;
    std::hint::black_box(b);
    Ok(())
}
fn verify_e4(s: &Store, l: &Layout, input: &Path, n: u64) -> Result<()> {
    each(input, |id, doc, es| {
        let got = get_e4(s, l, id)?;
        if got != doc {
            return Err(format!("e4 roundtrip id {id}: expected {doc}, got {got}").into());
        }
        let mut expected = es.to_vec();
        expected.sort_unstable();
        if e4_neighbors(s, id)? != expected {
            return Err("e4 edges mismatch".into());
        }
        if s.get(&ext(doc["_key"].as_str().unwrap()))? != Some(id.to_be_bytes().to_vec()) {
            return Err("e4 key index mismatch".into());
        }
        if s.get(&keys::prop(2, birth(doc["born"].as_i64().unwrap()), id))?
            .is_none()
        {
            return Err("e4 born index mismatch".into());
        }
        for dst in es {
            if s.get(&keys::redge(0, dst, 1, id))?.is_none() {
                return Err("e4 reverse edge missing".into());
            }
        }
        Ok(())
    })?;
    let mut nodes = 0;
    for r in s.scan(&[keys::TAG_NODE])? {
        let (k, _) = r?;
        if k[0] != keys::TAG_NODE {
            break;
        }
        nodes += 1;
    }
    if nodes != n {
        return Err("e4 node count".into());
    }
    Ok(())
}
fn verify_sql(c: &Connection, dim: usize, input: &Path, n: u64) -> Result<()> {
    each(input, |id, doc, es| {
        let got = get_sql(c, dim, id)?;
        if got != doc {
            return Err(format!("sqlite roundtrip id {id}: expected {doc}, got {got}").into());
        }
        let mut st = c.prepare_cached("SELECT dst FROM edge WHERE src=? ORDER BY dst")?;
        let actual = st
            .query_map([id], |r| r.get::<_, u64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut expected = es.to_vec();
        expected.sort_unstable();
        if actual != expected {
            return Err("sqlite edges mismatch".into());
        }
        Ok(())
    })?;
    let count: u64 = c.query_row("SELECT count(*) FROM person", [], |r| r.get(0))?;
    if count != n {
        return Err("sqlite count".into());
    }
    let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(integrity.into());
    }
    Ok(())
}
fn measure_e4(
    path: &Path,
    input: &Path,
    n: u64,
    expected: &[Vec<(u64, f64)>],
    projection: bool,
) -> Result<Value> {
    let t = Instant::now();
    let s = Store::open(path, cfg())?;
    let l = read_layout(&s)?;
    let reopen = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut sum = 0;
    let mut bytes = 0;
    for r in s.scan(&[keys::TAG_NODE])? {
        let (k, b) = r?;
        if k[0] != keys::TAG_NODE {
            break;
        }
        let id = u64::from_be_bytes(k[1..].try_into()?);
        let v = decode(&l, &b, |f| {
            s.get(&keys::vec_key(f as u64, id))?
                .ok_or_else(|| "missing vector".into())
        })?;
        checksum(&v, &mut sum, &mut bytes)?;
    }
    let full = t.elapsed().as_secs_f64();
    let t = Instant::now();
    for i in 0..500 {
        let v = get_e4(&s, &l, 1 + (i * 7919) % n)?;
        std::hint::black_box(serde_json::to_vec(&v)?);
    }
    let get = t.elapsed().as_secs_f64();
    let mut hybrid = Vec::new();
    for (i, want) in expected.iter().enumerate() {
        let t = Instant::now();
        let ids = if i == 8 { Some(e4_graph(&s, n)?) } else { None };
        let got = e4_hybrid(&s, &l, query(i % 8), ids.as_ref(), projection)?;
        hybrid.push(t.elapsed().as_secs_f64());
        if &got != want {
            return Err(format!("e4 hybrid mismatch query {i}").into());
        }
    }
    verify_e4(&s, &l, input, n)?;
    let mut tags: BTreeMap<u8, (u64, u64, u64)> = BTreeMap::new();
    for r in s.scan(&[])? {
        let (k, v) = r?;
        let e = tags.entry(k[0]).or_default();
        e.0 += 1;
        e.1 += k.len() as u64;
        e.2 += v.len() as u64;
    }
    let breakdown: Value = tags
        .into_iter()
        .map(|(tag, (rows, k, v))| {
            (
                format!("0x{tag:02x}"),
                json!({"records":rows,"key_bytes":k,"value_bytes":v}),
            )
        })
        .collect();
    Ok(
        json!({"reopen_seconds":reopen,"full_output_seconds":full,"point_500_seconds":get,"hybrid_seconds":hybrid,"full_output_crc32c":sum,"output_bytes":bytes,"verified_rows":n,"keyspaces":breakdown}),
    )
}
fn measure_sql(
    path: &Path,
    input: &Path,
    n: u64,
    dim: usize,
    expected: &[Vec<(u64, f64)>],
) -> Result<Value> {
    let t = Instant::now();
    let c = sql_open(&path.join("data.sqlite"))?;
    let reopen = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut sum = 0;
    let mut bytes = 0;
    {
        let mut st = c.prepare(&format!("{SELECT} ORDER BY id"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            checksum(&sql_doc(r, dim)?.1, &mut sum, &mut bytes)?;
        }
    }
    let full = t.elapsed().as_secs_f64();
    let t = Instant::now();
    for i in 0..500 {
        let v = get_sql(&c, dim, 1 + (i * 7919) % n)?;
        std::hint::black_box(serde_json::to_vec(&v)?);
    }
    let get = t.elapsed().as_secs_f64();
    let mut hybrid = Vec::new();
    for (i, want) in expected.iter().enumerate() {
        let t = Instant::now();
        let ids = if i == 8 {
            Some(sql_graph(&c, n)?)
        } else {
            None
        };
        let got = sql_hybrid(&c, query(i % 8), ids.as_ref(), dim)?;
        hybrid.push(t.elapsed().as_secs_f64());
        if &got != want {
            return Err(format!("sqlite hybrid mismatch query {i}: {got:?} != {want:?}").into());
        }
    }
    verify_sql(&c, dim, input, n)?;
    let version: String = c.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    let json_bytes: u64 = c.query_row(
        "SELECT sum(length(profile)+length(extras)) FROM person",
        [],
        |r| r.get(0),
    )?;
    Ok(
        json!({"reopen_seconds":reopen,"full_output_seconds":full,"point_500_seconds":get,"hybrid_seconds":hybrid,"full_output_crc32c":sum,"output_bytes":bytes,"verified_rows":n,"sqlite_version":version,"jsonb_bytes":json_bytes}),
    )
}
fn merge(mut a: Value, b: Value) -> Value {
    a.as_object_mut()
        .unwrap()
        .extend(b.as_object().unwrap().clone());
    a
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let base = PathBuf::from(
        args.get(1)
            .map(String::as_str)
            .unwrap_or("<scratch>"),
    );
    if !base.starts_with("<scratch>") {
        return Err("benchmark data must live under <scratch>".into());
    }
    let smoke = args.iter().any(|x| x == "--smoke");
    let run = base.join(format!(
        "p0-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
    ));
    let projection = args.iter().any(|x| x == "--project");
    fs::create_dir_all(&base)?;
    fs::create_dir(&run)?;
    let tmp = run.join("tmp");
    fs::create_dir(&tmp)?;
    std::env::set_var("TMPDIR", &tmp);
    std::env::set_var("SQLITE_TMPDIR", &tmp);
    println!("RUN {}", run.display());
    std::io::stdout().flush()?;
    let mut results = Vec::new();
    let counts = if smoke {
        vec![100]
    } else {
        vec![10_000, 40_000]
    };
    let dims = if smoke { vec![3] } else { vec![3, 128] };
    let reps = if smoke { 1 } else { 3 };
    for n in counts {
        for &dim in &dims {
            let input = run.join(format!("fixture-{n}-{dim}.jsonl"));
            let input_crc = fixture(&input, n, dim)?;
            let expected = oracle(&input, n)?;
            fs::write(
                run.join(format!("oracle-{n}-{dim}.json")),
                serde_json::to_vec_pretty(&expected)?,
            )?;
            for rep in 0..reps {
                let dir = run.join(format!("n{n}-d{dim}-r{rep}"));
                fs::create_dir(&dir)?;
                let ep = dir.join("e4");
                let sp = dir.join("sqlite");
                fs::create_dir(&ep)?;
                fs::create_dir(&sp)?;
                println!(
                    "START n={n} dim={dim} rep={rep} first={}",
                    if rep % 2 == 0 { "e4" } else { "sqlite" }
                );
                std::io::stdout().flush()?;
                let (mut e, mut s);
                if rep % 2 == 0 {
                    e = load_e4(&ep, &input, n, dim)?;
                    s = load_sql(&sp, &input, n, dim)?;
                } else {
                    s = load_sql(&sp, &input, n, dim)?;
                    e = load_e4(&ep, &input, n, dim)?;
                }
                if rep % 2 == 0 {
                    e = merge(e, measure_e4(&ep, &input, n, &expected, projection)?);
                    s = merge(s, measure_sql(&sp, &input, n, dim, &expected)?);
                } else {
                    s = merge(s, measure_sql(&sp, &input, n, dim, &expected)?);
                    e = merge(e, measure_e4(&ep, &input, n, &expected, projection)?);
                }
                if e["full_output_crc32c"] != s["full_output_crc32c"]
                    || e["output_bytes"] != s["output_bytes"]
                {
                    return Err("full rendered output differs".into());
                }
                println!("PASS n={n} dim={dim} rep={rep} e4_bytes={} sqlite_bytes={} e4_insert={} sqlite_insert={}",e["bytes"],s["bytes"],e["insert_seconds"],s["insert_seconds"]);
                results.push(json!({"rows":n,"dimensions":dim,"repetition":rep,"fixture_crc32c":input_crc,"e4":e,"sqlite":s}));
                fs::write(
                    run.join("results.json"),
                    serde_json::to_vec_pretty(
                        &json!({"prototype":"P0","projected_hybrid":projection,"cache_bytes":CACHE,"batch_rows":BATCH,"results":results}),
                    )?,
                )?;
            }
        }
    }
    println!("COMPLETE {}", run.join("results.json").display());
    Ok(())
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};
    fn directory() -> tempfile::TempDir {
        let p = std::env::temp_dir();
        assert!(
            p.starts_with("<scratch>"),
            "Set TMPDIR under <scratch> for storage tests"
        );
        tempfile::tempdir_in(p).unwrap()
    }
    #[test]
    fn descriptor_leaf_damage_recovers_without_reading_damaged_copy() {
        let d = directory();
        let l = layout(3);
        let mut s = Store::create(d.path(), cfg()).unwrap();
        install(&mut s, &l).unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        drop(s);
        let data = fs::read(d.path().join("data")).unwrap();
        let locations: Vec<usize> = data
            .windows(8)
            .enumerate()
            .filter_map(|(i, b)| (b == b"E4P0LAY\0").then_some(i))
            .collect();
        assert_eq!(locations.len(), 3);
        assert_eq!(
            locations
                .iter()
                .map(|i| i / 4096)
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        for (i, loc) in locations.iter().enumerate() {
            let mut f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(d.path().join("data"))
                .unwrap();
            f.seek(SeekFrom::Start((loc + 20) as u64)).unwrap();
            let mut byte = [0];
            f.read_exact(&mut byte).unwrap();
            byte[0] ^= 1;
            f.seek(SeekFrom::Start((loc + 20) as u64)).unwrap();
            f.write_all(&byte).unwrap();
            f.sync_all().unwrap();
            drop(f);
            let s = Store::open(d.path(), cfg()).unwrap();
            if i < 2 {
                assert_eq!(read_layout(&s).unwrap(), l);
            } else {
                assert!(read_layout(&s).is_err());
            }
            drop(s);
        }
    }
    #[test]
    fn large_json_and_large_external_vector_survive_reopen() {
        let d = directory();
        let l = layout(1536);
        let mut s = Store::create(d.path(), cfg()).unwrap();
        install(&mut s, &l).unwrap();
        let doc = json!({"_key":"large","name":"large","born":19490622,"profile":{"long":"nested-data-雪".repeat(10000)},"location":{"type":"Point","coordinates":[144.0,-38.0]},"embedding":vec![0.25;1536],"arbitrary":{"a":[null,true,3]}});
        put_e4(&mut s, &l, 1, &doc, [2, 3]).unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        drop(s);
        let s = Store::open(d.path(), cfg()).unwrap();
        let got = read_layout(&s).unwrap();
        assert_eq!(get_e4(&s, &got, 1).unwrap(), doc);
    }
}
