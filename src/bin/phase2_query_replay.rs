//! Read-only query comparison over completed, preserved R7 databases.
//! The primary-row oracle runs outside timings and retains only bounded top-k.
use e4_prototype::collections::{
    CandidateDriver, Database, IndexId, OrderValue, Projection, QueryBudget, QueryFilter,
    QueryOrder, QueryRequest, ScalarFilter, ScalarValue, TextMatch, VectorMetric,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{cmp::Ordering, collections::BinaryHeap, fs, path::Path, time::Instant};

type R<T> = Result<T, Box<dyn std::error::Error>>;
const CACHE: usize = 8 << 20;
const SEEDS: [usize; 3] = [17, 73, 211];
const K: usize = 10;
const SQL: &str = "SELECT p.id,p.embedding FROM people_fts f CROSS JOIN people p ON p.id=f.rowid WHERE people_fts MATCH 'flood' AND p.active=1";
type Hits = Vec<(u64, f64)>;

#[derive(Clone, Copy, Debug)]
struct Ranked(u64, f64);
impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1.to_bits() == other.1.to_bits()
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.1.total_cmp(&other.1).then(self.0.cmp(&other.0))
    }
}

fn vector(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| {
            ((((seed as u64 + 1) * (j as u64 + 3) + 17 * j as u64) % 257) as i32 - 128) as f32
                / 128.0
        })
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    assert_eq!(a.len(), b.len());
    let (mut dot, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        assert!(x.is_finite() && y.is_finite());
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        aa += x * x;
        bb += y * y;
    }
    (aa > 0.0 && bb > 0.0).then(|| 1.0 - dot / (aa.sqrt() * bb.sqrt()))
}

fn push(hits: &mut BinaryHeap<Ranked>, id: u64, distance: f64) {
    let candidate = Ranked(id, distance);
    if hits.len() < K {
        hits.push(candidate);
    } else if candidate < *hits.peek().unwrap() {
        hits.pop();
        hits.push(candidate);
    }
}

fn sorted(hits: BinaryHeap<Ranked>) -> Hits {
    hits.into_sorted_vec()
        .into_iter()
        .map(|hit| (hit.0, hit.1))
        .collect()
}

fn matches(active: bool, body: &str) -> bool {
    // The preserved R7 generator is ASCII. Do not call E4's analyzer/index.
    active && body.split_ascii_whitespace().any(|term| term == "flood")
}

fn check(actual: &Hits, expected: &Hits) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.0, expected.0);
        assert!((actual.1 - expected.1).abs() <= 1e-12);
    }
}

fn json_vector(value: &Value, dim: usize) -> Vec<f32> {
    let v = value.as_array().expect("typed vector array");
    assert_eq!(v.len(), dim);
    v.iter().map(|lane| lane.as_f64().unwrap() as f32).collect()
}

fn blob_vector(value: &[u8], dim: usize) -> Vec<f32> {
    assert_eq!(value.len(), dim * 4);
    value
        .chunks_exact(4)
        .map(|lane| f32::from_le_bytes(lane.try_into().unwrap()))
        .collect()
}

fn e4(root: &Path, rows: usize, dim: usize, repeats: usize) -> R<Value> {
    if fs::metadata(root.join("wal"))?.len() != 0 {
        return Err("query replay requires a checkpointed E4 source".into());
    }
    let db = Database::open_snapshot(
        root,
        Config {
            budget_bytes: CACHE,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let people = db
        .collection("people")?
        .ok_or("people collection missing")?;
    let indexes = db.list_indexes(people)?;
    let index = |name: &str| -> R<IndexId> {
        Ok(indexes
            .iter()
            .find(|index| index.name == name)
            .ok_or("required index missing")?
            .id)
    };
    let (active, text, embedding) = (
        index("active_idx")?,
        index("body_text")?,
        index("embedding_exact")?,
    );
    let vectors: Vec<_> = SEEDS.iter().map(|seed| vector(*seed % rows, dim)).collect();
    let mut oracle = vec![BinaryHeap::new(); SEEDS.len()];
    let (mut count, mut eligible) = (0, 0);
    let started = Instant::now();
    for row in db.scan(people, None)? {
        let row = row?;
        count += 1;
        if matches(
            row.document["active"].as_bool().unwrap(),
            row.document["body"].as_str().unwrap(),
        ) {
            eligible += 1;
            let stored = json_vector(&row.document["embedding"], dim);
            for (hits, query) in oracle.iter_mut().zip(&vectors) {
                if let Some(distance) = cosine(query, &stored) {
                    push(hits, row.id.sequence, distance);
                }
            }
        }
    }
    assert_eq!(count, rows);
    assert!(eligible >= K, "expected populated completed R7 corpus");
    let oracle: Vec<_> = oracle.into_iter().map(sorted).collect();
    let oracle_seconds = started.elapsed().as_secs_f64();
    let filters = [
        QueryFilter::Scalar {
            index: active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Text {
            index: text,
            query: "flood",
            matching: TextMatch::Any,
        },
    ];
    let mut samples = Vec::new();
    for repetition in 0..repeats {
        for (case, query) in vectors.iter().enumerate() {
            let start = Instant::now();
            let mut prepared = db.prepare_query(QueryRequest {
                collection: people,
                filters: &filters,
                order: QueryOrder::ExactVector {
                    index: embedding,
                    query,
                    metric: VectorMetric::Cosine,
                },
                projection: Projection::Ids,
                total_limit: Some(K),
                driver: CandidateDriver::Filter(1),
            })?;
            let page = prepared.next_page(K, QueryBudget::unlimited(), || false)?;
            let seconds = start.elapsed().as_secs_f64();
            let actual: Hits = page
                .rows
                .iter()
                .map(|row| match row.order {
                    OrderValue::Distance(distance) => (row.id.sequence, distance),
                    _ => panic!("expected distance ordering"),
                })
                .collect();
            check(&actual, &oracle[case]);
            samples.push(json!({"repetition":repetition,"seed":SEEDS[case],"seconds":seconds,"hits":actual,"driver":format!("{:?}",page.driver),"work":format!("{:?}",page.work)}));
        }
    }
    Ok(
        json!({"engine":"e4","people":count,"eligible":eligible,"oracle_seconds":oracle_seconds,"samples":samples}),
    )
}

fn sqlite(root: &Path, rows: usize, dim: usize, repeats: usize) -> R<Value> {
    let path = root.join("database.sqlite").canonicalize()?;
    let wal = root.join("database.sqlite-wal");
    if wal.exists() && fs::metadata(&wal)?.len() != 0 {
        return Err(
            "immutable SQLite replay refuses pending WAL; use a completed checkpointed R7 copy"
                .into(),
        );
    }
    // Percent-encode the absolute filename; immutable mode prevents SHM creation.
    let uri_path: String = path
        .to_str()
        .ok_or("non-UTF8 path")?
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/._-".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let db = Connection::open_with_flags(
        format!("file:{uri_path}?immutable=1"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    db.execute_batch("PRAGMA cache_size=-8192; PRAGMA query_only=ON;")?;
    let plan: Vec<String> = db
        .prepare(&format!("EXPLAIN QUERY PLAN {SQL}"))?
        .query_map([], |row| row.get(3))?
        .collect::<Result<_, _>>()?;
    let fts = plan
        .iter()
        .position(|line| line.contains("SCAN f VIRTUAL TABLE INDEX"))
        .ok_or("FTS-first plan missing")?;
    let primary = plan
        .iter()
        .position(|line| line.contains("SEARCH p USING INTEGER PRIMARY KEY"))
        .ok_or("primary lookup plan missing")?;
    assert!(fts < primary);
    let vectors: Vec<_> = SEEDS.iter().map(|seed| vector(*seed % rows, dim)).collect();
    let mut oracle = vec![BinaryHeap::new(); SEEDS.len()];
    let (mut count, mut eligible) = (0, 0);
    let started = Instant::now();
    {
        let mut statement = db.prepare("SELECT id,active,body,embedding FROM people")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            count += 1;
            if matches(row.get(1)?, &row.get::<_, String>(2)?) {
                eligible += 1;
                let id = row.get(0)?;
                let stored = blob_vector(&row.get::<_, Vec<u8>>(3)?, dim);
                for (hits, query) in oracle.iter_mut().zip(&vectors) {
                    if let Some(distance) = cosine(query, &stored) {
                        push(hits, id, distance);
                    }
                }
            }
        }
    }
    assert_eq!(count, rows);
    assert!(eligible >= K);
    let oracle: Vec<_> = oracle.into_iter().map(sorted).collect();
    let oracle_seconds = started.elapsed().as_secs_f64();
    let mut samples = Vec::new();
    for repetition in 0..repeats {
        for (case, query) in vectors.iter().enumerate() {
            let start = Instant::now();
            let mut statement = db.prepare(SQL)?;
            let mut rows = statement.query([])?;
            let mut actual = BinaryHeap::new();
            while let Some(row) = rows.next()? {
                let stored = blob_vector(&row.get::<_, Vec<u8>>(1)?, dim);
                if let Some(distance) = cosine(query, &stored) {
                    push(&mut actual, row.get(0)?, distance);
                }
            }
            let actual = sorted(actual);
            let seconds = start.elapsed().as_secs_f64();
            check(&actual, &oracle[case]);
            samples.push(
                json!({"repetition":repetition,"seed":SEEDS[case],"seconds":seconds,"hits":actual}),
            );
        }
    }
    Ok(
        json!({"engine":"sqlite","people":count,"eligible":eligible,"oracle_seconds":oracle_seconds,"samples":samples,"query_plan":plan,"sqlite_version":rusqlite::version()}),
    )
}

fn main() -> R<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--version" {
        println!(
            "{}",
            json!({"harness":"phase2-query-replay-v1","engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),"compile_features":{"compact-cells":cfg!(feature="compact-cells"),"sqlite-balance":cfg!(feature="sqlite-balance"),"keyspace-append":cfg!(feature="keyspace-append"),"slotref-split":cfg!(feature="slotref-split")}})
        );
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err("database replay is Linux-only".into());
    }
    if args.len() != 6 {
        return Err("usage: phase2_query_replay e4|sqlite ROOT ROWS DIM REPEATS".into());
    }
    let (rows, dim, repeats): (usize, usize, usize) =
        (args[3].parse()?, args[4].parse()?, args[5].parse()?);
    if !(1000..=1_000_000).contains(&rows)
        || !matches!(dim, 32 | 1536)
        || !(1..=10).contains(&repeats)
    {
        return Err("invalid replay bounds".into());
    }
    let root = Path::new(&args[2]);
    let result = match args[1].as_str() {
        "e4" => e4(root, rows, dim, repeats)?,
        "sqlite" => sqlite(root, rows, dim, repeats)?,
        _ => return Err("engine must be e4 or sqlite".into()),
    };
    println!(
        "{}",
        json!({"format":"phase2-query-replay-v1","query":"text flood AND active=true ORDER BY exact cosine k=10","cache_bytes":CACHE,"dimension":dim,"source_state":"completed R7 post-three-CRUD-round corpus; caller must pin source inventory","timing_scope":"prepare plus complete top-k; independent primary scan before timings warms caches; repetitions share connection/cache; no cold-cache claim","result":result})
    );
    Ok(())
}
