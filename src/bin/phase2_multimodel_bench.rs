//! Matched Phase-2 scale workload. Run one engine per fresh Linux process.
//! Correctness work is outside named timed stages; sampled disk peaks are lower bounds.
use crc32c::crc32c_append;
use e4_prototype::{
    collections::{
        BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database, Direction, EdgeKey,
        EntityId, Error as CollectionError, GraphContextId, IndexId, IndexState, NeighborRequest,
        PointFilter, Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter,
        ScalarPredicate, ScalarValue, SpatialCandidates, TextCandidates, TextMatch,
        VectorCandidates, VectorMetric,
    },
    pagewal::{create_compact_cells, IoCounters, PageWalStore},
    spatial_math::Bounds,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    fs,
    path::Path,
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;
const CACHE: usize = 8 << 20;
const BATCH: usize = 256;
const VOCAB: [&str; 16] = [
    "flood", "river", "road", "forest", "water", "storm", "bridge", "coast", "rain", "city", "map",
    "search", "public", "safety", "north", "south",
];

#[derive(Clone, Copy)]
enum ReaderMode {
    None,
    Batch,
    Short,
    Held,
}

#[derive(Clone, Copy, Default)]
struct CompactCommittedState {
    entity_loaded: usize,
    graph_edges_loaded: usize,
    updated: [usize; 3],
    deleted: [usize; 3],
    reinserted: [usize; 3],
    restored: [usize; 3],
}

struct ExpectedIndex {
    id: IndexId,
    ready: bool,
    after: u64,
}

struct E4Progress {
    stage: &'static str,
    stage_progress: Value,
    completed: serde_json::Map<String, Value>,
    commits: usize,
    committed: CompactCommittedState,
    indexes: BTreeMap<String, ExpectedIndex>,
}

impl E4Progress {
    fn new(_rows: usize) -> Self {
        Self {
            stage: "create_schema",
            stage_progress: json!({}),
            completed: serde_json::Map::new(),
            commits: 0,
            committed: CompactCommittedState::default(),
            indexes: BTreeMap::new(),
        }
    }

    fn committed(&mut self, stage: &'static str, detail: Value) {
        self.stage = stage;
        self.stage_progress = detail;
        self.commits += 1;
    }

    fn complete(&mut self, stage: &'static str, value: &Value) {
        self.stage = stage;
        self.completed.insert(stage.into(), value.clone());
    }

    fn complete_crud_stage(&mut self, cycle: usize, stage: &str, value: Value) {
        self.completed
            .insert(format!("crud_cycle_{cycle}_{stage}"), value);
    }

    fn index_committed(&mut self, name: &str, id: IndexId, ready: bool, after: u64) {
        self.indexes
            .insert(name.into(), ExpectedIndex { id, ready, after });
    }

    fn evidence(&self) -> Value {
        json!({
            "failed_stage": self.stage,
            "last_confirmed_commit": self.commits,
            "stage_progress": self.stage_progress,
            "completed_stages": self.completed,
            "committed_boundaries": {
                "entities": self.committed.entity_loaded,
                "relationships": self.committed.graph_edges_loaded,
                "crud_updated": self.committed.updated,
                "crud_deleted": self.committed.deleted,
                "crud_reinserted": self.committed.reinserted,
                "crud_edges_restored": self.committed.restored
            },
            "committed_indexes": self.indexes.iter().map(|(name,index)| json!({
                "name":name,"id":index.id.0,"state":if index.ready {"Ready"} else {"Building"},
                "after":if index.ready {Value::Null} else {json!(index.after)}
            })).collect::<Vec<_>>()
        })
    }
}

impl ReaderMode {
    fn parse(value: &str) -> R<Self> {
        Ok(match value {
            "none" => Self::None,
            "batch" => Self::Batch,
            "short" => Self::Short,
            "held" => Self::Held,
            _ => return Err("reader mode must be none|batch|short|held".into()),
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Batch => "batch",
            Self::Short => "short",
            Self::Held => "held",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::None => "no concurrent reader",
            Self::Batch => {
                "one snapshot held across the first 256-row update commit of each CRUD round, then released"
            }
            Self::Short => "one snapshot held across each complete CRUD round",
            Self::Held => "one snapshot held across all three CRUD rounds",
        }
    }
}

fn cfg() -> Config {
    Config {
        budget_bytes: CACHE,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn person_key(i: usize) -> String {
    format!("person/{i}")
}
fn org_key(i: usize) -> String {
    format!("org/{i}")
}
fn age(i: usize) -> i64 {
    18 + (i % 70) as i64
}
fn active(i: usize) -> bool {
    i % 3 != 0
}
fn point(i: usize) -> (f64, f64) {
    (
        144.0 + (i % 1000) as f64 / 10_000.0,
        -38.0 + ((i / 1000) % 1000) as f64 / 10_000.0,
    )
}
fn vector(i: usize, dimension: usize) -> Vec<f32> {
    (0..dimension)
        .map(|j| {
            ((((i as u64 + 1) * (j as u64 + 3) + 17 * j as u64) % 257) as i32 - 128) as f32 / 128.0
        })
        .collect()
}
fn body(i: usize) -> String {
    let multiplicity = i % 4;
    let mut terms = (0..6)
        .map(|offset| VOCAB[(i + offset) % VOCAB.len()])
        .collect::<Vec<_>>();
    terms.extend(std::iter::repeat_n(VOCAB[i % VOCAB.len()], multiplicity));
    terms.join(" ")
}
fn profile() -> Value {
    json!({"codes":[1,2,3],"nested":{"enabled":true}})
}
fn document(i: usize, dimension: usize) -> Value {
    let (longitude, latitude) = point(i);
    json!({
        "age":age(i),"name":format!("Person {i:08}"),"active":active(i),"body":body(i),
        "embedding":vector(i,dimension),
        "position":{"type":"Point","coordinates":[longitude,latitude]},"profile":profile()
    })
}
fn updated_document(i: usize, n: usize, dimension: usize, cycle: usize) -> Value {
    let mut value = document(i, dimension);
    let (x, y) = point(i);
    let object = value.as_object_mut().unwrap();
    object.insert("age".into(), json!(age(i) + cycle as i64 + 1));
    object.insert("body".into(), json!(format!("{} cycle{cycle}", body(i))));
    object.insert(
        "embedding".into(),
        json!(vector((i + cycle + 1) % n, dimension)),
    );
    object.insert(
        "position".into(),
        json!({"type":"Point","coordinates":[x+0.00001,y]}),
    );
    value
}

fn digest(n: usize, dimension: usize) -> String {
    let mut crc = 0;
    let mut add = |bytes: &[u8]| crc = crc32c_append(crc, bytes);
    add(b"phase2-synthetic-v1\0people:age,name,active,body,embedding,position,profile\0");
    for i in 0..100 {
        add(org_key(i).as_bytes());
        add(format!("Organization {i:03}").as_bytes());
    }
    for i in 0..n {
        add(person_key(i).as_bytes());
        add(&age(i).to_le_bytes());
        add(&[u8::from(active(i))]);
        add(format!("Person {i:08}").as_bytes());
        add(body(i).as_bytes());
        for lane in vector(i, dimension) {
            add(&lane.to_bits().to_le_bytes());
        }
        let (x, y) = point(i);
        add(&x.to_bits().to_le_bytes());
        add(&y.to_bits().to_le_bytes());
        add(br#"{"codes":[1,2,3],"nested":{"enabled":true}}"#);
        for destination in [(i + 1) % n, (i + 7) % n] {
            add(format!("knows:{i}:{destination}").as_bytes());
        }
        add(format!("member:{i}:{}", i % 100).as_bytes());
    }
    format!("crc32c:{crc:08x}")
}

fn sizes(root: &Path) -> (u64, u64) {
    fn visit(path: &Path, totals: &mut (u64, u64)) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                visit(&entry.path(), totals);
            } else if metadata.is_file() {
                totals.0 += metadata.len();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    totals.1 += metadata.blocks() * 512;
                }
                #[cfg(not(unix))]
                {
                    totals.1 += metadata.len();
                }
            }
        }
    }
    let mut totals = (0, 0);
    visit(root, &mut totals);
    totals
}
fn sample(root: &Path, peak: &mut (u64, u64)) {
    let current = sizes(root);
    peak.0 = peak.0.max(current.0);
    peak.1 = peak.1.max(current.1);
}
fn hwm() -> String {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .unwrap_or("unavailable")
        .to_owned()
}
fn expected_final_relationships(n: usize) -> usize {
    let knows = (0..n)
        .map(|i| {
            if i % 10 == 0 {
                2
            } else {
                [(i + 1) % n, (i + 7) % n]
                    .into_iter()
                    .filter(|d| d % 10 != 0)
                    .count()
            }
        })
        .sum::<usize>();
    n + knows
}
fn raw_graph_relationships(root: &Path) -> R<usize> {
    let store = PageWalStore::open_snapshot(root, CACHE)?;
    let mut count = 0;
    for row in store.range(&[0x71])? {
        let (key, _) = row?;
        if !key.starts_with(&[0x71]) {
            break;
        }
        count += 1;
    }
    Ok(count)
}
fn entity(sequence: usize) -> EntityId {
    EntityId {
        collection: CollectionId(1),
        sequence: sequence as u64 + 1,
    }
}
fn vector_blob(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect()
}
fn blob_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_bits(u32::from_le_bytes(chunk.try_into().unwrap())))
        .collect()
}
fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    let (mut dot, mut an, mut bn) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        dot += f64::from(x) * f64::from(y);
        an += f64::from(x) * f64::from(x);
        bn += f64::from(y) * f64::from(y);
    }
    (an > 0.0 && bn > 0.0).then(|| 1.0 - dot / (an.sqrt() * bn.sqrt()))
}

#[derive(Clone, Copy, Debug)]
struct Ranked {
    distance: f64,
    id: u64,
}
impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.distance.to_bits() == other.distance.to_bits() && self.id == other.id
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
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.id.cmp(&other.id))
    }
}
fn independent_vector(n: usize, dimension: usize, query: &[f32], k: usize) -> Vec<Ranked> {
    independent_vector_where(n, dimension, query, k, |_| true)
}

fn independent_vector_where(
    n: usize,
    dimension: usize,
    query: &[f32],
    k: usize,
    mut include: impl FnMut(usize) -> bool,
) -> Vec<Ranked> {
    let mut out = Vec::with_capacity(k + 1);
    for i in 0..n {
        if !include(i) {
            continue;
        }
        if let Some(distance) = cosine(query, &vector(i, dimension)) {
            out.push(Ranked {
                distance,
                id: i as u64 + 1,
            });
            out.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
            if out.len() > k {
                out.pop();
            }
        }
    }
    out
}

fn independent_text(n: usize, query: &str, k: usize) -> Vec<(u64, f64)> {
    #[derive(Clone, Copy)]
    struct Hit(u64, f64);
    impl PartialEq for Hit {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0 && self.1.to_bits() == other.1.to_bits()
        }
    }
    impl Eq for Hit {}
    impl PartialOrd for Hit {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Hit {
        fn cmp(&self, other: &Self) -> Ordering {
            other.1.total_cmp(&self.1).then(self.0.cmp(&other.0))
        }
    }
    let mut corpus_tokens = 0usize;
    let mut df = 0usize;
    for i in 0..n {
        let text = body(i);
        let mut found = false;
        for term in text.split_whitespace() {
            corpus_tokens += 1;
            found |= term == query;
        }
        df += usize::from(found);
    }
    let total = n as f64;
    let average = corpus_tokens as f64 / total;
    let idf = (1.0 + (total - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
    let mut heap = BinaryHeap::new();
    for i in 0..n {
        let text = body(i);
        let len = text.split_whitespace().count();
        let tf = text
            .split_whitespace()
            .filter(|term| *term == query)
            .count() as f64;
        if tf > 0.0 {
            let denominator = tf + 1.2 * (0.25 + 0.75 * len as f64 / average);
            heap.push(Hit(i as u64 + 1, idf * (tf * 2.2) / denominator));
            if heap.len() > k {
                heap.pop();
            }
        }
    }
    let mut results = heap
        .into_iter()
        .map(|hit| (hit.0, hit.1))
        .collect::<Vec<_>>();
    results.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    results
}

fn independent_combined(n: usize, dimension: usize, query: &[f32]) -> Vec<Ranked> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![0usize];
    for _ in 0..2 {
        let mut next = Vec::new();
        for source in frontier {
            for destination in [(source + 1) % n, (source + 7) % n] {
                if destination != 0 && seen.insert(destination) {
                    next.push(destination);
                }
            }
        }
        frontier = next;
    }
    let mut hits = seen
        .into_iter()
        .filter(|&i| active(i))
        .filter(|&i| {
            let (x, y) = point(i);
            (144.0..=144.01).contains(&x) && (-38.0..=-37.99).contains(&y)
        })
        .filter_map(|i| {
            cosine(query, &vector(i, dimension)).map(|distance| Ranked {
                distance,
                id: i as u64 + 1,
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
    hits.truncate(10);
    hits
}

fn independent_text_active_vector(n: usize, dimension: usize, query: &[f32]) -> Vec<Ranked> {
    independent_vector_where(n, dimension, query, 10, |i| {
        active(i) && body(i).split_whitespace().any(|term| term == "flood")
    })
}

fn independent_members_active_spatial_vector(
    n: usize,
    dimension: usize,
    query: &[f32],
) -> Vec<Ranked> {
    independent_vector_where(n, dimension, query, 10, |i| {
        let (x, y) = point(i);
        i % 100 == 0 && active(i) && (144.0..=144.01).contains(&x) && (-38.0..=-37.99).contains(&y)
    })
}

/// Traversal-group size and seed spread. Seed j is person (j*7919)%n, so the
/// hundred seeds land all over the key space instead of in one hot page range.
const GRAPH_SEEDS: usize = 100;
const GRAPH_BFS_DEPTH: usize = 3;

fn graph_seed(j: usize, n: usize) -> usize {
    (j * 7919) % n
}

/// Outgoing `knows` destinations of a seed, straight from the generator formula.
fn independent_knows_out(n: usize, seed: usize) -> BTreeSet<u64> {
    [(seed + 1) % n, (seed + 7) % n]
        .into_iter()
        .map(|i| i as u64 + 1)
        .collect()
}

/// Incoming `knows` sources of a seed: the two people whose formula points at it.
fn independent_knows_in(n: usize, seed: usize) -> BTreeSet<u64> {
    [(seed + n - 1) % n, (seed + n - 7) % n]
        .into_iter()
        .map(|i| i as u64 + 1)
        .collect()
}

/// People wired to organization `org` (1-based) by the `member_of` formula.
fn independent_org_member_count(n: usize, org: u64) -> usize {
    (0..n).filter(|i| i % 100 == org as usize - 1).count()
}

/// Distinct entities within `max_depth` `knows` hops of a seed, both directions,
/// counted the way the engine counts `visited`: the seed included.
fn independent_bfs_visited(n: usize, seed: usize, max_depth: usize) -> usize {
    let mut seen = BTreeSet::from([seed]);
    let mut frontier = BTreeSet::from([seed]);
    for _ in 0..max_depth {
        let mut next = BTreeSet::new();
        for source in frontier {
            for adjacent in [
                (source + 1) % n,
                (source + 7) % n,
                (source + n - 1) % n,
                (source + n - 7) % n,
            ] {
                if !seen.contains(&adjacent) {
                    next.insert(adjacent);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        seen.extend(next.iter().copied());
        frontier = next;
    }
    seen.len()
}

/// One traversal item: total wall time over every seed plus the per-seed median,
/// because a single traversal is too short to read off a single-shot timer.
fn graph_timing_json(mut micros: Vec<f64>, rows: usize, detail: Value) -> Value {
    let total = micros.iter().sum::<f64>() / 1e6;
    micros.sort_by(|a, b| a.total_cmp(b));
    let median = micros[micros.len() / 2];
    let mut value = detail;
    value["seconds"] = json!(total);
    value["median_micros"] = json!(median);
    value["seeds"] = json!(micros.len());
    value["rows"] = json!(rows);
    value
}

fn final_vector_oracle(
    n: usize,
    dimension: usize,
    query: &[f32],
    ids: &[u64],
    mut include: impl FnMut(usize) -> bool,
) -> Vec<Ranked> {
    let mut out = Vec::with_capacity(11);
    for i in 0..n {
        if !include(i) {
            continue;
        }
        if let Some(distance) = cosine(query, &vector((i + 3) % n, dimension)) {
            out.push(Ranked {
                distance,
                id: ids[i],
            });
            out.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
            if out.len() > 10 {
                out.pop();
            }
        }
    }
    out
}
fn final_spatial_oracle(n: usize, ids: &[u64]) -> Vec<u64> {
    let mut out = (0..n)
        .filter(|&i| {
            let (x, y) = point(i);
            let x = x + 0.00001;
            (144.0..=144.01).contains(&x) && (-38.0..=-37.99).contains(&y)
        })
        .map(|i| ids[i])
        .collect::<Vec<_>>();
    out.sort();
    out
}
fn final_text_oracle(n: usize, ids: &[u64]) -> Vec<(u64, f64)> {
    let mut tokens = 0usize;
    let mut df = 0usize;
    for i in 0..n {
        let text = format!("{} cycle2", body(i));
        tokens += text.split_whitespace().count();
        df += usize::from(text.split_whitespace().any(|term| term == "flood"));
    }
    let total = n as f64;
    let average = tokens as f64 / total;
    let idf = (1.0 + (total - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
    let mut out = Vec::with_capacity(11);
    for i in 0..n {
        let text = format!("{} cycle2", body(i));
        let tf = text
            .split_whitespace()
            .filter(|term| *term == "flood")
            .count() as f64;
        if tf > 0.0 {
            let len = text.split_whitespace().count() as f64;
            out.push((
                ids[i],
                idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * len / average)),
            ));
            out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            if out.len() > 10 {
                out.pop();
            }
        }
    }
    out
}

fn finish_build(
    db: &mut Database,
    id: IndexId,
    name: &str,
    policy: &str,
    root: &Path,
    peak: &mut (u64, u64),
    progress: &mut E4Progress,
) -> R<(usize, usize)> {
    if policy == "atomic" {
        // Atomic publication is the READY flip, not one transaction: the engine
        // driver groups bounded chunks and commits them under BUILDING.
        let steps = db.build_index_to_ready(id, BATCH)?;
        progress.index_committed(name, id, true, 0);
        progress.committed(
            "late_build",
            json!({"index":name,"steps":steps,"publication_commits":1,"ready":true}),
        );
        sample(root, peak);
        return Ok((steps, 1));
    }
    let mut steps = 0;
    let mut commits = 0;
    loop {
        let ready = db.build_index_step(id, BATCH)?;
        steps += 1;
        db.commit()?;
        if policy == "resumable" {
            commits += 1;
            progress.index_committed(
                name,
                id,
                ready,
                if ready { 0 } else { (steps * BATCH) as u64 },
            );
            progress.committed(
                "late_build",
                json!({"index":name,"steps":steps,"publication_commits":commits,"ready":ready}),
            );
            sample(root, peak);
        }
        if ready {
            break;
        }
    }
    Ok((steps, commits))
}

fn time_json(start: Instant, extra: Value) -> Value {
    let mut value = extra;
    value["seconds"] = json!(start.elapsed().as_secs_f64());
    value
}

fn io_json(c: &IoCounters) -> Value {
    json!({
        "frames": c.wal_frames_appended,
        "commit_frames": c.commit_frames,
        "dirty_pages_flushed": c.dirty_pages_flushed,
        "data_pages_written_at_checkpoint": c.data_pages_written_at_checkpoint,
        "pages_written": c.pages_written(),
        "wal_fsyncs": c.wal_fsyncs(),
        "wal_fsyncs_commit": c.wal_fsyncs_commit,
        "wal_fsyncs_checkpoint": c.wal_fsyncs_checkpoint,
        "wal_fsyncs_open": c.wal_fsyncs_open,
        "data_fsyncs": c.data_fsyncs_checkpoint,
        "metadata_fsyncs": c.metadata_fsyncs,
        "fsyncs": c.fsyncs(),
        "checkpoint_count": c.checkpoint_count,
        "wal_bytes_written": c.wal_bytes_written,
        "data_bytes_written": c.data_bytes_written,
        "bytes_written": c.bytes_written()
    })
}

fn e4_io_delta(db: &Database, prev: &mut IoCounters) -> R<Value> {
    let now = db.io_counters()?;
    let d = now.saturating_sub(*prev);
    *prev = now;
    Ok(io_json(&d))
}

#[derive(Clone, Copy, Default)]
struct SqliteIoAcc {
    commits: u64,
    explicit_checkpoints: u64,
    checkpointed_pages: i64,
}

fn sqlite_db_status(conn: &Connection, op: i32) -> R<i64> {
    let (mut current, mut high) = (0i32, 0i32);
    let rc = unsafe { rusqlite::ffi::sqlite3_db_status(conn.handle(), op, &mut current, &mut high, 0) };
    if rc != rusqlite::ffi::SQLITE_OK {
        return Err("sqlite db_status failed".into());
    }
    Ok(i64::from(current))
}

fn sqlite_io_delta(conn: &Connection, acc: &SqliteIoAcc, prev: &mut (i64, i64, i64, i64, SqliteIoAcc)) -> R<Value> {
    let writes = sqlite_db_status(conn, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE)?;
    let hits = sqlite_db_status(conn, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT)?;
    let misses = sqlite_db_status(conn, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS)?;
    let spill = sqlite_db_status(conn, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_SPILL)?;
    let d_writes = (writes - prev.0).max(0);
    let d_hits = (hits - prev.1).max(0);
    let d_misses = (misses - prev.2).max(0);
    let d_spill = (spill - prev.3).max(0);
    let d_commits = acc.commits.saturating_sub(prev.4.commits);
    let d_ckpt = acc.explicit_checkpoints.saturating_sub(prev.4.explicit_checkpoints);
    let d_ckpt_pages = (acc.checkpointed_pages - prev.4.checkpointed_pages).max(0);
    *prev = (writes, hits, misses, spill, *acc);
    Ok(json!({
        "frames": d_writes,
        "pages_written": d_writes,
        "cache_writes": d_writes,
        "cache_hits": d_hits,
        "cache_misses": d_misses,
        "cache_spill": d_spill,
        "commits": d_commits,
        "explicit_checkpoints": d_ckpt,
        "checkpointed_pages": d_ckpt_pages,
        "derived_wal_fsyncs": d_commits,
        "derived_checkpoint_fsyncs": d_ckpt * 2,
        "fsyncs": d_commits + d_ckpt * 2,
        "bytes_written": d_writes * 4096,
        "fsync_basis": "derived: 1 WAL fsync per COMMIT (synchronous=FULL, fullfsync=ON) + 2 fsyncs per explicit TRUNCATE checkpoint (db+wal); auto-checkpoint fsyncs are not observed (no VFS hook)"
    }))
}

fn sqlite_truncate(conn: &Connection, acc: &mut SqliteIoAcc) -> R<()> {
    let (_busy, _log, checkpointed): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    acc.explicit_checkpoints += 1;
    acc.checkpointed_pages += checkpointed;
    Ok(())
}

fn run_e4(
    n: usize,
    dimension: usize,
    root: &Path,
    policy: &str,
    readers: ReaderMode,
    progress: &mut E4Progress,
) -> R<Value> {
    let mut peak = (0, 0);
    let mut db = Database::create(root, cfg())?;
    let people = db.create_collection(
        "people",
        vec![
            ("age".into(), Kind::Int),
            ("name".into(), Kind::Text),
            ("active".into(), Kind::Bool),
            ("body".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(dimension)),
            ("position".into(), Kind::Point),
            ("profile".into(), Kind::Json),
        ],
        CollectionOptions::default(),
    )?;
    let organizations = db.create_collection(
        "organizations",
        vec![("name".into(), Kind::Text)],
        CollectionOptions::default(),
    )?;
    db.enable_graph()?;
    let knows = db.create_edge_type("knows")?;
    let member = db.create_edge_type("member_of")?;
    db.commit()?;
    progress.committed(
        "create_schema",
        json!({"people_collection":people.0,"organizations_collection":organizations.0,"knows":knows.0,"member_of":member.0}),
    );
    let mut io = serde_json::Map::new();
    let mut io_prev = db.io_counters()?;

    progress.stage = "entity_load";
    let start = Instant::now();
    for i in 0..100 {
        db.put(
            organizations,
            &org_key(i),
            &json!({"name":format!("Organization {i:03}")}),
        )?;
    }
    for i in 0..n {
        assert_eq!(
            db.put(people, &person_key(i), &document(i, dimension))?,
            entity(i)
        );
        if (i + 1) % BATCH == 0 {
            db.commit()?;
            let committed_people = i + 1;
            progress.committed.entity_loaded = committed_people;
            progress.committed(
                "entity_load",
                json!({"people":committed_people,"organizations":100}),
            );
            sample(root, &mut peak);
        }
    }
    db.commit()?;
    progress.committed.entity_loaded = n;
    progress.committed("entity_load", json!({"people":n,"organizations":100}));
    let entity_load = time_json(start, json!({"commits":n.div_ceil(BATCH)+1}));
    progress.complete("entity_load", &entity_load);
    io.insert("entity_load".into(), e4_io_delta(&db, &mut io_prev)?);

    progress.stage = "graph_load";
    let start = Instant::now();
    let mut edge_count = 0usize;
    for i in 0..n {
        for destination in [(i + 1) % n, (i + 7) % n] {
            db.put_edge(
                GraphContextId::BASE,
                entity(i),
                knows,
                entity(destination),
                &json!({}),
            )?;
            edge_count += 1;
        }
        db.put_edge(
            GraphContextId::BASE,
            entity(i),
            member,
            EntityId {
                collection: organizations,
                sequence: (i % 100) as u64 + 1,
            },
            &json!({}),
        )?;
        edge_count += 1;
        if edge_count % BATCH == 0 {
            db.commit()?;
            progress.committed.graph_edges_loaded = edge_count;
            progress.committed("graph_load", json!({"relationships":edge_count}));
            sample(root, &mut peak);
        }
    }
    db.commit()?;
    progress.committed.graph_edges_loaded = edge_count;
    progress.committed("graph_load", json!({"relationships":edge_count}));
    let graph_load = time_json(
        start,
        json!({"relationships":edge_count,"reverse_maintained":true}),
    );
    progress.complete("graph_load", &graph_load);
    db.checkpoint()?;
    io.insert("graph_load".into(), e4_io_delta(&db, &mut io_prev)?);
    let loaded = sizes(root);
    progress
        .completed
        .insert("loaded_bytes".into(), json!(loaded));

    let mut builds = serde_json::Map::new();
    progress.stage = "late_build";
    progress.stage_progress = json!({"index":"age_idx","steps":0,"publication_commits":0});
    let start = Instant::now();
    let age_index = db.create_scalar_index(people, "age_idx", "age", false)?;
    let (active_index, steps, commits) = if policy == "atomic" {
        let steps = db.build_index_to_ready(age_index, BATCH)?;
        let active = db.create_scalar_index(people, "active_idx", "active", false)?;
        let steps = steps + db.build_index_to_ready(active, BATCH)?;
        progress.index_committed("age_idx", age_index, true, 0);
        progress.index_committed("active_idx", active, true, 0);
        progress.committed(
            "late_build",
            json!({"index":"scalar","steps":steps,"publication_commits":1,"ready":true}),
        );
        sample(root, &mut peak);
        (active, steps, 1)
    } else {
        let (steps, commits) = finish_build(
            &mut db, age_index, "age_idx", policy, root, &mut peak, progress,
        )?;
        let active = db.create_scalar_index(people, "active_idx", "active", false)?;
        let (active_steps, active_commits) = finish_build(
            &mut db,
            active,
            "active_idx",
            policy,
            root,
            &mut peak,
            progress,
        )?;
        (active, steps + active_steps, commits + active_commits)
    };
    let scalar_build = time_json(
        start,
        json!({"indexes":2,"steps":steps,"publication_commits":commits,"policy":policy}),
    );
    builds.insert("scalar".into(), scalar_build.clone());
    progress.complete("build_scalar", &scalar_build);
    io.insert("build_scalar".into(), e4_io_delta(&db, &mut io_prev)?);
    progress.stage = "late_build";
    progress.stage_progress = json!({"index":"embedding_exact","steps":0,"publication_commits":0});
    let start = Instant::now();
    let vector_index = db.create_exact_vector_index(people, "embedding_exact", "embedding")?;
    let (steps, commits) = finish_build(
        &mut db,
        vector_index,
        "embedding_exact",
        policy,
        root,
        &mut peak,
        progress,
    )?;
    let vector_build = time_json(
        start,
        json!({"steps":steps,"publication_commits":commits,"policy":policy}),
    );
    builds.insert("vector".into(), vector_build.clone());
    progress.complete("build_vector", &vector_build);
    io.insert("build_vector".into(), e4_io_delta(&db, &mut io_prev)?);
    progress.stage = "late_build";
    progress.stage_progress = json!({"index":"position_point","steps":0,"publication_commits":0});
    let start = Instant::now();
    let spatial_index = db.create_point_index(people, "position_point", "position")?;
    let (steps, commits) = finish_build(
        &mut db,
        spatial_index,
        "position_point",
        policy,
        root,
        &mut peak,
        progress,
    )?;
    let spatial_build = time_json(
        start,
        json!({"steps":steps,"publication_commits":commits,"policy":policy}),
    );
    builds.insert("spatial".into(), spatial_build.clone());
    progress.complete("build_spatial", &spatial_build);
    io.insert("build_spatial".into(), e4_io_delta(&db, &mut io_prev)?);
    progress.stage = "late_build";
    progress.stage_progress = json!({"index":"body_text","steps":0,"publication_commits":0});
    let start = Instant::now();
    let text_index = db.create_text_index(people, "body_text", "body")?;
    let (steps, commits) = finish_build(
        &mut db,
        text_index,
        "body_text",
        policy,
        root,
        &mut peak,
        progress,
    )?;
    let text_build = time_json(
        start,
        json!({"steps":steps,"publication_commits":commits,"policy":policy}),
    );
    builds.insert("text".into(), text_build.clone());
    progress.complete("build_text", &text_build);
    io.insert("build_text".into(), e4_io_delta(&db, &mut io_prev)?);
    db.checkpoint()?;
    sample(root, &mut peak);
    io.insert("checkpoint_after_builds".into(), e4_io_delta(&db, &mut io_prev)?);

    progress.stage = "pre_crud_queries";
    let query_vector = vector(17 % n, dimension);
    let oracle = independent_vector(n, dimension, &query_vector, 10);
    let mut queries = serde_json::Map::new();
    let scalar_filters = [
        QueryFilter::Scalar {
            index: age_index,
            predicate: ScalarFilter::Eq(ScalarValue::I64(30)),
        },
        QueryFilter::Scalar {
            index: active_index,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
    ];
    let start = Instant::now();
    let mut scalar_query = db.prepare_query(QueryRequest {
        collection: people,
        filters: &scalar_filters,
        order: QueryOrder::Scalar {
            index: age_index,
            direction: e4_prototype::collections::SortDirection::Ascending,
        },
        projection: Projection::Ids,
        total_limit: Some(100),
        driver: CandidateDriver::Auto,
    })?;
    let scalar_page = scalar_query.next_page(100, QueryBudget::unlimited(), || false)?;
    queries.insert(
        "scalar_active_age".into(),
        time_json(start, json!({"hits":scalar_page.rows.len()})),
    );
    let expected_scalar = (0..n)
        .filter(|&i| active(i) && age(i) == 30)
        .map(|i| i as u64 + 1)
        .take(100)
        .collect::<Vec<_>>();
    assert_eq!(
        scalar_page
            .rows
            .iter()
            .map(|row| row.id.sequence)
            .collect::<Vec<_>>(),
        expected_scalar
    );
    let start = Instant::now();
    let vector_hits = db.query_exact_vector(
        vector_index,
        &query_vector,
        VectorMetric::Cosine,
        10,
        VectorCandidates::All,
        n + 1,
        || false,
    )?;
    queries.insert(
        "vector_cosine_k10".into(),
        time_json(start, json!({"hits":vector_hits.len()})),
    );
    assert_eq!(
        vector_hits
            .iter()
            .map(|h| h.id.sequence)
            .collect::<Vec<_>>(),
        oracle.iter().map(|h| h.id).collect::<Vec<_>>()
    );
    for (hit, expected) in vector_hits.iter().zip(&oracle) {
        assert!((hit.distance - expected.distance).abs() < 1e-12);
    }

    let bounds = Bounds::new(144.0, 144.01, -38.0, -37.99)?;
    let start = Instant::now();
    let spatial = db.query_point_bbox(
        spatial_index,
        bounds,
        65536,
        SpatialCandidates::All,
        n + 1,
        || false,
    )?;
    queries.insert(
        "spatial_bbox".into(),
        time_json(
            start,
            json!({"hits":spatial.len(),"driver":"RTree candidate-first CROSS JOIN + exact f64 refinement"}),
        ),
    );
    let expected_spatial = (0..n)
        .filter(|&i| {
            let (x, y) = point(i);
            (144.0..=144.01).contains(&x) && (-38.0..=-37.99).contains(&y)
        })
        .map(entity)
        .collect::<Vec<_>>();
    assert_eq!(spatial, expected_spatial);

    let start = Instant::now();
    let text = db.query_text(
        text_index,
        "flood",
        TextMatch::Any,
        10,
        TextCandidates::All,
        n.saturating_mul(4) + 1,
        || false,
    )?;
    queries.insert(
        "text_positive_bm25_k10".into(),
        time_json(
            start,
            json!({"hits":text.len(),"semantics":"phase2 positive BM25"}),
        ),
    );
    let text_oracle = independent_text(n, "flood", 10);
    assert_eq!(
        text.iter().map(|hit| hit.id.sequence).collect::<Vec<_>>(),
        text_oracle.iter().map(|hit| hit.0).collect::<Vec<_>>()
    );
    for (actual, expected) in text.iter().zip(&text_oracle) {
        assert!((actual.score - expected.1).abs() < 1e-12);
    }

    let filters = [
        QueryFilter::Graph(BfsRequest {
            seed: entity(0),
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            min_depth: 1,
            max_depth: 2,
            include_seed: false,
            max_visited: 16,
            max_edges: 32,
            result_limit: 16,
        }),
        QueryFilter::Scalar {
            index: active_index,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Point {
            index: spatial_index,
            predicate: PointFilter::Bbox(bounds),
        },
    ];
    let start = Instant::now();
    let mut prepared = db.prepare_query(QueryRequest {
        collection: people,
        filters: &filters,
        order: QueryOrder::ExactVector {
            index: vector_index,
            query: &query_vector,
            metric: VectorMetric::Cosine,
        },
        projection: Projection::Ids,
        total_limit: Some(10),
        driver: CandidateDriver::Auto,
    })?;
    let page = prepared.next_page(10, QueryBudget::unlimited(), || false)?;
    queries.insert("combined_graph_active_bbox_vector".into(), time_json(start, json!({"hits":page.rows.len(),"driver":format!("{:?}",page.driver),"work":format!("{:?}",page.work)})));
    let combined_oracle = independent_combined(n, dimension, &query_vector);
    assert_eq!(
        page.rows
            .iter()
            .map(|row| row.id.sequence)
            .collect::<Vec<_>>(),
        combined_oracle.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );

    let text_vector_filters = [
        QueryFilter::Scalar {
            index: active_index,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Text {
            index: text_index,
            query: "flood",
            matching: TextMatch::Any,
        },
    ];
    let start = Instant::now();
    let mut text_vector = db.prepare_query(QueryRequest {
        collection: people,
        filters: &text_vector_filters,
        order: QueryOrder::ExactVector {
            index: vector_index,
            query: &query_vector,
            metric: VectorMetric::Cosine,
        },
        projection: Projection::Ids,
        total_limit: Some(10),
        driver: CandidateDriver::Filter(1),
    })?;
    let text_vector_page = text_vector.next_page(10, QueryBudget::unlimited(), || false)?;
    queries.insert("text_active_vector".into(),time_json(start,json!({"hits":text_vector_page.rows.len(),"driver":format!("{:?}",text_vector_page.driver),"work":format!("{:?}",text_vector_page.work)})));
    let text_vector_oracle = independent_text_active_vector(n, dimension, &query_vector);
    assert_eq!(
        text_vector_page
            .rows
            .iter()
            .map(|row| row.id.sequence)
            .collect::<Vec<_>>(),
        text_vector_oracle
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>()
    );

    let member_limit = n.div_ceil(100).saturating_add(1).min(65536);
    let member_filters = [
        QueryFilter::Graph(BfsRequest {
            seed: EntityId {
                collection: organizations,
                sequence: 1,
            },
            direction: Direction::Incoming,
            context: GraphContextId::BASE,
            edge_type: Some(member),
            min_depth: 1,
            max_depth: 1,
            include_seed: false,
            max_visited: member_limit + 1,
            max_edges: member_limit + 1,
            result_limit: member_limit,
        }),
        QueryFilter::Scalar {
            index: active_index,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Point {
            index: spatial_index,
            predicate: PointFilter::Bbox(bounds),
        },
    ];
    let start = Instant::now();
    let mut member_vector = db.prepare_query(QueryRequest {
        collection: people,
        filters: &member_filters,
        order: QueryOrder::ExactVector {
            index: vector_index,
            query: &query_vector,
            metric: VectorMetric::Cosine,
        },
        projection: Projection::Ids,
        total_limit: Some(10),
        driver: CandidateDriver::Auto,
    })?;
    let member_page = member_vector.next_page(10, QueryBudget::unlimited(), || false)?;
    queries.insert("members_active_spatial_vector".into(),time_json(start,json!({"hits":member_page.rows.len(),"driver":format!("{:?}",member_page.driver),"work":format!("{:?}",member_page.work)})));
    let member_oracle = independent_members_active_spatial_vector(n, dimension, &query_vector);
    assert_eq!(
        member_page
            .rows
            .iter()
            .map(|row| row.id.sequence)
            .collect::<Vec<_>>(),
        member_oracle.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    // Dedicated graph traversal group. The graph is this database's speciality and
    // nothing here timed a plain traversal: every earlier graph number was an edge
    // load, a CRUD cascade, or a combined query measured once. One traversal is far
    // too short for a single-shot timer, so each item runs over the same hundred
    // spread seeds, each seed timed on its own, after one untimed warm-up seed.
    let graph_seeds = (0..GRAPH_SEEDS).map(|j| graph_seed(j, n)).collect::<Vec<_>>();
    let warm_seed = graph_seed(0, n);
    let out_request = |seed: usize| NeighborRequest {
        entity: entity(seed),
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        limit: 8,
    };
    let in_request = |seed: usize| NeighborRequest {
        entity: entity(seed),
        direction: Direction::Incoming,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        limit: 8,
    };
    let bfs_request = |seed: usize| BfsRequest {
        seed: entity(seed),
        direction: Direction::Both,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        min_depth: 1,
        max_depth: GRAPH_BFS_DEPTH,
        include_seed: false,
        max_visited: 1000,
        max_edges: 100_000,
        result_limit: 1000,
    };
    let org_request = |seed: usize| BfsRequest {
        seed: EntityId {
            collection: organizations,
            sequence: (seed % 100) as u64 + 1,
        },
        direction: Direction::Incoming,
        context: GraphContextId::BASE,
        edge_type: Some(member),
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: member_limit + 1,
        max_edges: member_limit + 1,
        result_limit: member_limit,
    };

    db.neighbors(out_request(warm_seed))?;
    let mut out_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut out_rows = 0usize;
    for &seed in &graph_seeds {
        let start = Instant::now();
        let found = db
            .neighbors(out_request(seed))?
            .into_iter()
            .map(|edge| edge.key.destination.sequence)
            .collect::<BTreeSet<_>>();
        out_micros.push(start.elapsed().as_secs_f64() * 1e6);
        assert_eq!(found, independent_knows_out(n, seed));
        out_rows += found.len();
    }
    queries.insert(
        "graph_out_1hop".into(),
        graph_timing_json(
            out_micros,
            out_rows,
            json!({"work":"outgoing knows neighbours, complete-or-error limit 8"}),
        ),
    );

    db.neighbors(in_request(warm_seed))?;
    let mut in_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut in_rows = 0usize;
    for &seed in &graph_seeds {
        let start = Instant::now();
        let found = db
            .neighbors(in_request(seed))?
            .into_iter()
            .map(|edge| edge.key.source.sequence)
            .collect::<BTreeSet<_>>();
        in_micros.push(start.elapsed().as_secs_f64() * 1e6);
        assert_eq!(found, independent_knows_in(n, seed));
        in_rows += found.len();
    }
    queries.insert(
        "graph_in_1hop".into(),
        graph_timing_json(
            in_micros,
            in_rows,
            json!({"work":"incoming knows neighbours through the reverse edge index"}),
        ),
    );

    db.traverse_bfs(bfs_request(warm_seed))?;
    let mut bfs_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut bfs_rows = 0usize;
    for &seed in &graph_seeds {
        let start = Instant::now();
        let visited = db.traverse_bfs(bfs_request(seed))?.visited;
        bfs_micros.push(start.elapsed().as_secs_f64() * 1e6);
        assert_eq!(visited, independent_bfs_visited(n, seed, GRAPH_BFS_DEPTH));
        bfs_rows += visited;
    }
    queries.insert(
        "graph_bfs_3hop".into(),
        graph_timing_json(
            bfs_micros,
            bfs_rows,
            json!({"work":"distinct-entity BFS over knows, both directions, depth 1..=3, visited counted with the seed"}),
        ),
    );

    db.traverse_bfs(org_request(warm_seed))?;
    let mut org_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut org_rows = 0usize;
    for &seed in &graph_seeds {
        let start = Instant::now();
        let members = db.traverse_bfs(org_request(seed))?.nodes.len();
        org_micros.push(start.elapsed().as_secs_f64() * 1e6);
        assert_eq!(
            members,
            independent_org_member_count(n, (seed % 100) as u64 + 1)
        );
        org_rows += members;
    }
    queries.insert(
        "graph_members_of_org".into(),
        graph_timing_json(
            org_micros,
            org_rows,
            json!({"work":"every person with a member_of edge to organization (seed%100)+1, through the reverse edge index"}),
        ),
    );

    progress.complete("pre_crud_queries", &Value::Object(queries.clone()));
    io.insert("pre_crud_queries".into(), e4_io_delta(&db, &mut io_prev)?);

    let held = matches!(readers, ReaderMode::Held)
        .then(|| Database::open_snapshot(root, cfg()))
        .transpose()?;
    let mut current_ids = (0..n).map(entity).collect::<Vec<_>>();
    let mut crud = Vec::new();
    for cycle in 0..3usize {
        progress.stage = "crud_update";
        progress.stage_progress = json!({"cycle":cycle,"rows":0});
        let short = matches!(readers, ReaderMode::Short)
            .then(|| Database::open_snapshot(root, cfg()))
            .transpose()?;
        let mut batch = matches!(readers, ReaderMode::Batch)
            .then(|| Database::open_snapshot(root, cfg()))
            .transpose()?;
        let expected_before = if cycle == 0 {
            document(1, dimension)
        } else {
            updated_document(1, n, dimension, cycle - 1)
        };
        for snapshot in [short.as_ref(), batch.as_ref()].into_iter().flatten() {
            assert_eq!(
                snapshot.get(people, &person_key(1))?.unwrap().document,
                expected_before
            );
        }
        let start = Instant::now();
        for i in 0..n {
            db.update(
                people,
                &person_key(i),
                &updated_document(i, n, dimension, cycle),
            )?;
            db.put_edge(
                GraphContextId::BASE,
                current_ids[i],
                knows,
                current_ids[(i + 1) % n],
                &json!({"round":cycle,"slot":1}),
            )?;
            db.put_edge(
                GraphContextId::BASE,
                current_ids[i],
                knows,
                current_ids[(i + 7) % n],
                &json!({"round":cycle,"slot":7}),
            )?;
            db.put_edge(
                GraphContextId::BASE,
                current_ids[i],
                member,
                EntityId {
                    collection: organizations,
                    sequence: (i % 100) as u64 + 1,
                },
                &json!({"round":cycle}),
            )?;
            if (i + 1) % BATCH == 0 {
                db.commit()?;
                progress.committed.updated[cycle] = i + 1;
                progress.committed("crud_update", json!({"cycle":cycle,"rows":i+1}));
                sample(root, &mut peak);
                if let Some(snapshot) = batch.take() {
                    assert_eq!(
                        snapshot.get(people, &person_key(1))?.unwrap().document,
                        expected_before
                    );
                    drop(snapshot);
                }
            }
        }
        db.commit()?;
        progress.committed.updated[cycle] = n;
        progress.committed("crud_update", json!({"cycle":cycle,"rows":n}));
        sample(root, &mut peak);
        if let Some(snapshot) = batch.take() {
            assert_eq!(
                snapshot.get(people, &person_key(1))?.unwrap().document,
                expected_before
            );
            drop(snapshot);
        }
        let update_s = start.elapsed().as_secs_f64();
        progress.complete_crud_stage(cycle, "update", json!({"seconds":update_s,"updated":n}));
        io.insert(format!("crud_cycle_{cycle}_update"), e4_io_delta(&db, &mut io_prev)?);
        progress.stage = "crud_delete";
        progress.stage_progress = json!({"cycle":cycle,"people":0});
        let start = Instant::now();
        let mut deleted = 0usize;
        for i in (0..n).step_by(10) {
            assert!(db.delete(people, &person_key(i))?);
            deleted += 1;
            if deleted % BATCH == 0 {
                db.commit()?;
                progress.committed.deleted[cycle] = deleted;
                progress.committed("crud_delete", json!({"cycle":cycle,"people":deleted}));
                sample(root, &mut peak);
            }
        }
        db.commit()?;
        progress.committed.deleted[cycle] = deleted;
        progress.committed("crud_delete", json!({"cycle":cycle,"people":deleted}));
        sample(root, &mut peak);
        let delete_s = start.elapsed().as_secs_f64();
        progress.complete_crud_stage(
            cycle,
            "delete",
            json!({"seconds":delete_s,"deleted":deleted}),
        );
        io.insert(format!("crud_cycle_{cycle}_delete"), e4_io_delta(&db, &mut io_prev)?);
        progress.stage = "crud_reinsert";
        progress.stage_progress = json!({"cycle":cycle,"people":0});
        let start = Instant::now();
        let mut inserted = 0usize;
        for i in (0..n).step_by(10) {
            current_ids[i] = db.put(
                people,
                &person_key(i),
                &updated_document(i, n, dimension, cycle),
            )?;
            inserted += 1;
            if inserted % BATCH == 0 {
                db.commit()?;
                progress.committed.reinserted[cycle] = inserted;
                progress.committed("crud_reinsert", json!({"cycle":cycle,"people":inserted}));
                sample(root, &mut peak);
            }
        }
        db.commit()?;
        progress.committed.reinserted[cycle] = inserted;
        progress.committed("crud_reinsert", json!({"cycle":cycle,"people":inserted}));
        progress.stage = "crud_restore_edges";
        progress.stage_progress = json!({"cycle":cycle,"people":0,"relationships":0});
        // Commit by PEOPLE restored, exactly like the SQLite arm's chunks(BATCH)
        // over deleted_indices and E4's own reinsert loop above. Counting edges
        // here (+3 per person) committed every 86 people instead of 256, so the
        // E4 arm did ~2x the commits and fsyncs of the SQLite arm in this stage.
        let mut restored_sources = 0usize;
        for i in (0..n).step_by(10) {
            db.put_edge(
                GraphContextId::BASE,
                current_ids[i],
                knows,
                current_ids[(i + 1) % n],
                &json!({"round":cycle,"slot":1}),
            )?;
            db.put_edge(
                GraphContextId::BASE,
                current_ids[i],
                knows,
                current_ids[(i + 7) % n],
                &json!({"round":cycle,"slot":7}),
            )?;
            db.put_edge(
                GraphContextId::BASE,
                current_ids[i],
                member,
                EntityId {
                    collection: organizations,
                    sequence: (i % 100) as u64 + 1,
                },
                &json!({"round":cycle}),
            )?;
            restored_sources += 1;
            if restored_sources % BATCH == 0 {
                db.commit()?;
                progress.committed.restored[cycle] = restored_sources;
                progress.committed(
                    "crud_restore_edges",
                    json!({"cycle":cycle,"people":restored_sources,"relationships":restored_sources*3}),
                );
                sample(root, &mut peak);
            }
        }
        db.commit()?;
        progress.committed.restored[cycle] = restored_sources;
        progress.committed(
            "crud_restore_edges",
            json!({"cycle":cycle,"people":restored_sources,"relationships":restored_sources*3}),
        );
        sample(root, &mut peak);
        let insert_s = start.elapsed().as_secs_f64();
        progress.complete_crud_stage(
            cycle,
            "reinsert_edges",
            json!({"seconds":insert_s,"reinserted":inserted,"relationships_restored":restored_sources*3}),
        );
        io.insert(format!("crud_cycle_{cycle}_reinsert"), e4_io_delta(&db, &mut io_prev)?);
        assert_eq!(
            db.get(people, &person_key(1))?.unwrap().document,
            updated_document(1, n, dimension, cycle)
        );
        assert_eq!(
            db.get(people, &person_key(0))?.unwrap().document,
            updated_document(0, n, dimension, cycle)
        );
        let destinations = db
            .neighbors(NeighborRequest {
                entity: current_ids[1],
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(knows),
                limit: 8,
            })?
            .into_iter()
            .map(|edge| edge.key.destination)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            destinations,
            [current_ids[2], current_ids[8]].into_iter().collect()
        );
        if let Some(snapshot) = short.as_ref() {
            assert_eq!(
                snapshot.get(people, &person_key(1))?.unwrap().document,
                expected_before
            );
        }
        drop(short);
        crud.push(json!({"cycle":cycle,"profile":"scale","update_seconds":update_s,"delete_seconds":delete_s,"reinsert_edges_seconds":insert_s,"updated":n,"deleted_reinserted":deleted,"relationships_reasserted_during_update":3*n,"deleted_fraction":deleted as f64/n as f64}));
        progress.complete("crud", &Value::Array(crud.clone()));
    }
    progress.stage = "post_crud_oracle";
    let mut verified_rows = 0usize;
    for row in db.scan(people, None)? {
        let row = row?;
        let i: usize = row.key.strip_prefix("person/").unwrap().parse()?;
        assert_eq!(row.id, current_ids[i]);
        assert_eq!(row.document, updated_document(i, n, dimension, 2));
        verified_rows += 1;
    }
    assert_eq!(verified_rows, n);
    assert_eq!(
        raw_graph_relationships(root)?,
        expected_final_relationships(n)
    );
    let id_sequences = current_ids.iter().map(|id| id.sequence).collect::<Vec<_>>();
    let mut post_crud_queries = serde_json::Map::new();
    let start = Instant::now();
    let scalar = db.query_scalar(age_index, ScalarPredicate::Eq(json!(30)), 65536)?;
    post_crud_queries.insert(
        "scalar_age".into(),
        time_json(start, json!({"hits":scalar.len()})),
    );
    let mut expected_scalar = (0..n)
        .filter(|&i| age(i) + 3 == 30)
        .map(|i| current_ids[i])
        .collect::<Vec<_>>();
    expected_scalar.sort();
    assert_eq!(scalar, expected_scalar);
    let final_vector = final_vector_oracle(n, dimension, &query_vector, &id_sequences, |_| true);
    let start = Instant::now();
    let vector_hits = db.query_exact_vector(
        vector_index,
        &query_vector,
        VectorMetric::Cosine,
        10,
        VectorCandidates::All,
        n + 1,
        || false,
    )?;
    post_crud_queries.insert(
        "vector".into(),
        time_json(start, json!({"hits":vector_hits.len()})),
    );
    assert_eq!(
        vector_hits
            .iter()
            .map(|hit| hit.id.sequence)
            .collect::<Vec<_>>(),
        final_vector.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    let final_spatial = final_spatial_oracle(n, &id_sequences);
    let start = Instant::now();
    let spatial_hits = db.query_point_bbox(
        spatial_index,
        bounds,
        65536,
        SpatialCandidates::All,
        n + 1,
        || false,
    )?;
    post_crud_queries.insert(
        "spatial".into(),
        time_json(start, json!({"hits":spatial_hits.len()})),
    );
    assert_eq!(
        spatial_hits
            .iter()
            .map(|id| id.sequence)
            .collect::<Vec<_>>(),
        final_spatial
    );
    let final_text = final_text_oracle(n, &id_sequences);
    let start = Instant::now();
    let text_hits = db.query_text(
        text_index,
        "flood",
        TextMatch::Any,
        10,
        TextCandidates::All,
        n + 1,
        || false,
    )?;
    post_crud_queries.insert(
        "text".into(),
        time_json(start, json!({"hits":text_hits.len()})),
    );
    assert_eq!(
        text_hits
            .iter()
            .map(|hit| hit.id.sequence)
            .collect::<Vec<_>>(),
        final_text.iter().map(|hit| hit.0).collect::<Vec<_>>()
    );
    for (actual, expected) in text_hits.iter().zip(&final_text) {
        assert!((actual.score - expected.1).abs() < 1e-12);
    }
    progress.complete(
        "post_crud_queries",
        &Value::Object(post_crud_queries.clone()),
    );
    io.insert("post_crud_queries".into(), e4_io_delta(&db, &mut io_prev)?);
    progress.stage = "held_reader_oracle";
    if let Some(snapshot) = held.as_ref() {
        assert_eq!(snapshot.scan(people, None)?.count(), n);
        assert_eq!(
            snapshot.get(people, &person_key(1))?.unwrap().document,
            document(1, dimension)
        );
        let destinations = snapshot
            .neighbors(NeighborRequest {
                entity: entity(1),
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(knows),
                limit: 8,
            })?
            .into_iter()
            .map(|edge| edge.key.destination)
            .collect::<BTreeSet<_>>();
        assert_eq!(destinations, [entity(2), entity(8)].into_iter().collect());
    }
    drop(held);
    progress.stage = "checkpoint_reopen";
    db.checkpoint()?;
    io.insert("final_checkpoint".into(), e4_io_delta(&db, &mut io_prev)?);
    let final_size = sizes(root);
    drop(db);
    let start = Instant::now();
    let reopened = Database::open_snapshot(root, cfg())?;
    assert_eq!(reopened.collection("people")?, Some(people));
    let reopen_s = start.elapsed().as_secs_f64();
    assert_eq!(reopened.scan(people, None)?.count(), n);
    assert_eq!(
        reopened.get(people, &person_key(1))?.unwrap().document,
        updated_document(1, n, dimension, 2)
    );
    assert_eq!(
        reopened.get(people, &person_key(0))?.unwrap().document,
        updated_document(0, n, dimension, 2)
    );
    assert_eq!(
        reopened.query_scalar(age_index, ScalarPredicate::Eq(json!(30)), 65536)?,
        expected_scalar
    );
    let reopened_vector = reopened.query_exact_vector(
        vector_index,
        &query_vector,
        VectorMetric::Cosine,
        10,
        VectorCandidates::All,
        n + 1,
        || false,
    )?;
    assert_eq!(
        reopened_vector
            .iter()
            .map(|hit| hit.id.sequence)
            .collect::<Vec<_>>(),
        final_vector.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    let reopened_spatial = reopened.query_point_bbox(
        spatial_index,
        bounds,
        65536,
        SpatialCandidates::All,
        n + 1,
        || false,
    )?;
    assert_eq!(
        reopened_spatial
            .iter()
            .map(|id| id.sequence)
            .collect::<Vec<_>>(),
        final_spatial
    );
    let reopened_text = reopened.query_text(
        text_index,
        "flood",
        TextMatch::Any,
        10,
        TextCandidates::All,
        n + 1,
        || false,
    )?;
    assert_eq!(
        reopened_text
            .iter()
            .map(|hit| hit.id.sequence)
            .collect::<Vec<_>>(),
        final_text.iter().map(|hit| hit.0).collect::<Vec<_>>()
    );
    progress.stage = "complete";
    Ok(
        json!({"engine":"e4","entity_load":entity_load,"graph_load":graph_load,"builds":builds,"queries":queries,"crud":crud,"post_crud_queries":post_crud_queries,"io":io,"loaded_bytes":loaded,"final_bytes":final_size,"sampled_peak_bytes":peak,"reopen_seconds":reopen_s,"reader_mode":readers.name(),"reader_scope":readers.scope(),"publication_policy":policy,"rss_hwm":hwm()}),
    )
}

fn sequence_through(
    state: CompactCommittedState,
    rows: usize,
    logical: usize,
    cycles: usize,
) -> u64 {
    if logical % 10 != 0 {
        return logical as u64 + 1;
    }
    let ordinal = logical / 10;
    let mut sequence = logical as u64 + 1;
    let mut prior = 0usize;
    for cycle in 0..cycles.min(3) {
        if ordinal < state.reinserted[cycle] {
            sequence = (rows + prior + ordinal + 1) as u64;
        }
        prior += state.reinserted[cycle];
    }
    sequence
}

fn expected_document_version(state: CompactCommittedState, logical: usize) -> i8 {
    if logical >= state.entity_loaded {
        return -1;
    }
    let mut version = 0i8;
    for cycle in 0..3 {
        if version >= 0 && logical < state.updated[cycle] {
            version = cycle as i8 + 1;
        }
        if logical % 10 == 0 && logical / 10 < state.deleted[cycle] {
            version = -1;
        }
        if logical % 10 == 0 && logical / 10 < state.reinserted[cycle] {
            version = cycle as i8 + 1;
        }
    }
    version
}

fn expected_document(logical: usize, rows: usize, dimension: usize, version: i8) -> Value {
    if version == 0 {
        document(logical, dimension)
    } else {
        updated_document(logical, rows, dimension, version as usize - 1)
    }
}

fn expected_edges(
    state: CompactCommittedState,
    rows: usize,
    source: usize,
    people: CollectionId,
    organizations: CollectionId,
    knows: e4_prototype::collections::EdgeTypeId,
    member: e4_prototype::collections::EdgeTypeId,
) -> Vec<(EdgeKey, Value)> {
    let mut slots: [Option<(u64, i8)>; 3] = [None; 3];
    for (slot, destination) in [
        ((0usize), ((source + 1) % rows) as u64 + 1),
        ((1usize), ((source + 7) % rows) as u64 + 1),
        ((2usize), (source % 100) as u64 + 1),
    ] {
        if source * 3 + slot < state.graph_edges_loaded {
            slots[slot] = Some((destination, -1));
        }
    }
    for cycle in 0..3 {
        if source < state.updated[cycle] {
            slots[0] = Some((
                sequence_through(state, rows, (source + 1) % rows, cycle),
                cycle as i8,
            ));
            slots[1] = Some((
                sequence_through(state, rows, (source + 7) % rows, cycle),
                cycle as i8,
            ));
            slots[2] = Some(((source % 100) as u64 + 1, cycle as i8));
        }
        if source % 10 == 0 && source / 10 < state.deleted[cycle] {
            slots = [None; 3];
        }
        for (slot, destination) in [(0, (source + 1) % rows), (1, (source + 7) % rows)] {
            if destination % 10 == 0 && destination / 10 < state.deleted[cycle] {
                let removed = sequence_through(state, rows, destination, cycle);
                if slots[slot].is_some_and(|(sequence, _)| sequence == removed) {
                    slots[slot] = None;
                }
            }
        }
        if source % 10 == 0 && source / 10 < state.restored[cycle] {
            slots[0] = Some((
                sequence_through(state, rows, (source + 1) % rows, cycle + 1),
                cycle as i8,
            ));
            slots[1] = Some((
                sequence_through(state, rows, (source + 7) % rows, cycle + 1),
                cycle as i8,
            ));
            slots[2] = Some(((source % 100) as u64 + 1, cycle as i8));
        }
    }
    let source_id = EntityId {
        collection: people,
        sequence: sequence_through(state, rows, source, 3),
    };
    slots
        .into_iter()
        .enumerate()
        .filter_map(|(slot, value)| {
            value.map(|(destination_sequence, cycle)| {
                let (edge_type, destination, properties) = match slot {
                    0 => (
                        knows,
                        EntityId {
                            collection: people,
                            sequence: destination_sequence,
                        },
                        if cycle < 0 {
                            json!({})
                        } else {
                            json!({"round":cycle,"slot":1})
                        },
                    ),
                    1 => (
                        knows,
                        EntityId {
                            collection: people,
                            sequence: destination_sequence,
                        },
                        if cycle < 0 {
                            json!({})
                        } else {
                            json!({"round":cycle,"slot":7})
                        },
                    ),
                    _ => (
                        member,
                        EntityId {
                            collection: organizations,
                            sequence: destination_sequence,
                        },
                        if cycle < 0 {
                            json!({})
                        } else {
                            json!({"round":cycle})
                        },
                    ),
                };
                (
                    EdgeKey {
                        source: source_id,
                        context: GraphContextId::BASE,
                        edge_type,
                        destination,
                    },
                    properties,
                )
            })
        })
        .collect()
}

fn expected_text_for_version(logical: usize, version: i8) -> String {
    if version == 0 {
        body(logical)
    } else {
        format!("{} cycle{}", body(logical), version - 1)
    }
}

fn verify_refused_e4(
    root: &Path,
    rows: usize,
    dimension: usize,
    progress: &E4Progress,
) -> R<Value> {
    let state = progress.committed;
    let db = Database::open_snapshot(root, cfg())?;
    let people = db
        .collection("people")?
        .ok_or("refusal oracle: people missing")?;
    let organizations = db
        .collection("organizations")?
        .ok_or("refusal oracle: organizations missing")?;
    let knows = db
        .edge_type("knows")?
        .ok_or("refusal oracle: knows missing")?;
    let member = db
        .edge_type("member_of")?
        .ok_or("refusal oracle: member_of missing")?;

    let mut observed_people = 0usize;
    let mut previous_people_id = None;
    for row in db.scan(people, None)? {
        let row = row?;
        let logical: usize = row
            .key
            .strip_prefix("person/")
            .ok_or("refusal oracle: unexpected people key")?
            .parse()?;
        if logical >= rows {
            return Err("refusal oracle: people key is outside fixture range".into());
        }
        if previous_people_id.is_some_and(|previous| previous >= row.id) {
            return Err("refusal oracle: people IDs are not strictly unique and ordered".into());
        }
        previous_people_id = Some(row.id);
        let version = expected_document_version(state, logical);
        if version < 0 {
            return Err("refusal oracle: unexpected committed person".into());
        }
        let expected_id = sequence_through(state, rows, logical, 3);
        if row.id.sequence != expected_id
            || row.document != expected_document(logical, rows, dimension, version)
        {
            return Err("refusal oracle: committed person differs from ledger".into());
        }
        observed_people += 1;
    }
    let expected_people = (0..rows)
        .filter(|logical| expected_document_version(state, *logical) >= 0)
        .count();
    if observed_people != expected_people {
        return Err("refusal oracle: committed people count differs from ledger".into());
    }
    let mut observed_organizations = 0usize;
    for row in db.scan(organizations, None)? {
        let row = row?;
        let logical: usize = row
            .key
            .strip_prefix("org/")
            .ok_or("refusal oracle: unexpected organization key")?
            .parse()?;
        if logical >= 100
            || row.id.sequence != logical as u64 + 1
            || row.document != json!({"name":format!("Organization {logical:03}")})
        {
            return Err("refusal oracle: committed organization differs from fixture".into());
        }
        observed_organizations += 1;
    }
    let expected_organizations = usize::from(state.entity_loaded > 0) * 100;
    if observed_organizations != expected_organizations {
        return Err("refusal oracle: organization count differs from ledger".into());
    }

    let mut expected_relationships = 0usize;
    for source in 0..rows {
        let expected = expected_edges(state, rows, source, people, organizations, knows, member);
        expected_relationships += expected.len();
        if expected_document_version(state, source) < 0 {
            continue;
        }
        let source_id = EntityId {
            collection: people,
            sequence: sequence_through(state, rows, source, 3),
        };
        let actual = db.neighbors(NeighborRequest {
            entity: source_id,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: None,
            limit: 8,
        })?;
        let actual = actual
            .into_iter()
            .map(|edge| (edge.key, edge.properties))
            .collect::<BTreeMap<_, _>>();
        let expected = expected.into_iter().collect::<BTreeMap<_, _>>();
        if actual != expected {
            return Err("refusal oracle: committed relationships differ from ledger".into());
        }
    }
    if raw_graph_relationships(root)? != expected_relationships {
        return Err("refusal oracle: raw relationship count differs from ledger".into());
    }

    let actual_indexes = db.list_indexes(people)?;
    if actual_indexes.len() != progress.indexes.len() {
        return Err("refusal oracle: committed index count differs from ledger".into());
    }
    for info in &actual_indexes {
        let expected = progress
            .indexes
            .get(&info.name)
            .ok_or("refusal oracle: unexpected committed index")?;
        let expected_state = if expected.ready {
            matches!(info.state, IndexState::Ready)
        } else {
            matches!(info.state, IndexState::Building { after } if after == expected.after)
        };
        if info.id != expected.id || !expected_state {
            return Err("refusal oracle: committed index state differs from ledger".into());
        }
    }

    let mut ready_queries = Vec::new();
    if let Some(index) = progress.indexes.get("age_idx").filter(|index| index.ready) {
        let actual = db.query_scalar(index.id, ScalarPredicate::Eq(json!(30)), 65_536)?;
        let mut expected = (0..rows)
            .filter_map(|logical| {
                let version = expected_document_version(state, logical);
                (version >= 0
                    && age(logical) + if version == 0 { 0 } else { i64::from(version) } == 30)
                    .then(|| EntityId {
                        collection: people,
                        sequence: sequence_through(state, rows, logical, 3),
                    })
            })
            .collect::<Vec<_>>();
        expected.sort();
        if actual != expected {
            return Err("refusal oracle: ready scalar index differs from ledger".into());
        }
        ready_queries.push("scalar_age");
    }
    if let Some(index) = progress
        .indexes
        .get("embedding_exact")
        .filter(|index| index.ready)
    {
        let query = vector(17 % rows, dimension);
        let mut expected = Vec::with_capacity(11);
        for logical in 0..rows {
            let version = expected_document_version(state, logical);
            if version < 0 {
                continue;
            }
            let vector_row = if version == 0 {
                logical
            } else {
                (logical + version as usize) % rows
            };
            if let Some(distance) = cosine(&query, &vector(vector_row, dimension)) {
                expected.push(Ranked {
                    distance,
                    id: sequence_through(state, rows, logical, 3),
                });
                expected.sort_by(|left, right| {
                    left.distance
                        .total_cmp(&right.distance)
                        .then(left.id.cmp(&right.id))
                });
                if expected.len() > 10 {
                    expected.pop();
                }
            }
        }
        let actual = db.query_exact_vector(
            index.id,
            &query,
            VectorMetric::Cosine,
            10,
            VectorCandidates::All,
            rows + 1,
            || false,
        )?;
        if actual.iter().map(|hit| hit.id.sequence).collect::<Vec<_>>()
            != expected.iter().map(|hit| hit.id).collect::<Vec<_>>()
        {
            return Err("refusal oracle: ready vector index differs from ledger".into());
        }
        ready_queries.push("exact_vector");
    }
    if let Some(index) = progress
        .indexes
        .get("position_point")
        .filter(|index| index.ready)
    {
        let candidates = (0..rows)
            .filter(|logical| logical % 10 != 0 && expected_document_version(state, *logical) >= 0)
            .take(64)
            .map(|logical| EntityId {
                collection: people,
                sequence: sequence_through(state, rows, logical, 3),
            })
            .collect::<Vec<_>>();
        let bounds = Bounds::new(144.0, 144.01, -38.0, -37.99)?;
        let actual = db.query_point_bbox(
            index.id,
            bounds,
            64,
            SpatialCandidates::SortedUnique(&candidates),
            candidates.len() + 1,
            || false,
        )?;
        let mut expected = candidates
            .iter()
            .filter_map(|id| {
                let logical = usize::try_from(id.sequence - 1).ok()?;
                let version = expected_document_version(state, logical);
                let (mut longitude, latitude) = point(logical);
                if version > 0 {
                    longitude += 0.00001;
                }
                ((144.0..=144.01).contains(&longitude) && (-38.0..=-37.99).contains(&latitude))
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        expected.sort();
        if actual != expected {
            return Err("refusal oracle: ready spatial index differs from ledger".into());
        }
        ready_queries.push("spatial_filtered");
    }
    if let Some(index) = progress
        .indexes
        .get("body_text")
        .filter(|index| index.ready)
    {
        let mut documents = 0usize;
        let mut tokens = 0usize;
        let mut df = 0usize;
        for logical in 0..rows {
            let version = expected_document_version(state, logical);
            if version < 0 {
                continue;
            }
            let text = expected_text_for_version(logical, version);
            documents += 1;
            tokens += text.split_whitespace().count();
            df += usize::from(text.split_whitespace().any(|term| term == "flood"));
        }
        let average = tokens as f64 / documents as f64;
        let idf = (1.0 + (documents as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
        let mut expected = Vec::with_capacity(11);
        for logical in 0..rows {
            let version = expected_document_version(state, logical);
            if version < 0 {
                continue;
            }
            let text = expected_text_for_version(logical, version);
            let tf = text
                .split_whitespace()
                .filter(|term| *term == "flood")
                .count() as f64;
            if tf == 0.0 {
                continue;
            }
            let length = text.split_whitespace().count() as f64;
            expected.push((
                sequence_through(state, rows, logical, 3),
                idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * length / average)),
            ));
            expected.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
            if expected.len() > 10 {
                expected.pop();
            }
        }
        let actual = db.query_text(
            index.id,
            "flood",
            TextMatch::Any,
            10,
            TextCandidates::All,
            rows.saturating_mul(4).saturating_add(1),
            || false,
        )?;
        if actual.iter().map(|hit| hit.id.sequence).collect::<Vec<_>>()
            != expected.iter().map(|hit| hit.0).collect::<Vec<_>>()
        {
            return Err("refusal oracle: ready text index differs from ledger".into());
        }
        ready_queries.push("text_bm25");
    }

    Ok(json!({
        "committed_state_verified":true,
        "people":observed_people,
        "organizations":observed_organizations,
        "relationships":expected_relationships,
        "indexes":actual_indexes.len(),
        "ready_family_queries":ready_queries,
        "oracle_memory":"bounded streaming; no per-row graph ledger"
    }))
}

fn sqlite_setup(connection: &Connection) -> R<()> {
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE; PRAGMA foreign_keys=OFF;
      CREATE TABLE people(id INTEGER PRIMARY KEY, external_key TEXT NOT NULL UNIQUE, age INTEGER NOT NULL, name TEXT NOT NULL, active INTEGER NOT NULL, body TEXT NOT NULL, embedding BLOB NOT NULL, longitude REAL NOT NULL, latitude REAL NOT NULL, profile TEXT NOT NULL);
      CREATE TABLE organizations(id INTEGER PRIMARY KEY, external_key TEXT NOT NULL UNIQUE, name TEXT NOT NULL);
      CREATE TABLE edges(context INTEGER NOT NULL, source_collection INTEGER NOT NULL, source_id INTEGER NOT NULL, edge_type INTEGER NOT NULL, destination_collection INTEGER NOT NULL, destination_id INTEGER NOT NULL, properties TEXT NOT NULL, PRIMARY KEY(context,source_collection,source_id,edge_type,destination_collection,destination_id)) WITHOUT ROWID;
      CREATE INDEX edge_in ON edges(context,destination_collection,destination_id,edge_type,source_collection,source_id);")?;
    Ok(())
}

fn assert_sqlite_snapshot_person(
    connection: &Connection,
    rows: usize,
    dimension: usize,
    completed_cycles: usize,
) -> R<()> {
    let observed = connection.query_row(
        "SELECT age,name,active,body,embedding,longitude,latitude,profile FROM people WHERE external_key=?1",
        params![person_key(1)],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, String>(7)?,
            ))
        },
    )?;
    let (longitude, latitude) = point(1);
    let expected_body = if completed_cycles == 0 {
        body(1)
    } else {
        format!("{} cycle{}", body(1), completed_cycles - 1)
    };
    let vector_row = if completed_cycles == 0 {
        1
    } else {
        (1 + completed_cycles) % rows
    };
    assert_eq!(observed.0, age(1) + completed_cycles as i64);
    assert_eq!(observed.1, "Person 00000001");
    assert_eq!(observed.2, if active(1) { 1 } else { 0 });
    assert_eq!(observed.3, expected_body);
    assert_eq!(observed.4, vector_blob(&vector(vector_row, dimension)));
    assert_eq!(
        observed.5,
        longitude + if completed_cycles > 0 { 0.00001 } else { 0.0 }
    );
    assert_eq!(observed.6, latitude);
    assert_eq!(observed.7, profile().to_string());
    Ok(())
}

fn sqlite_tx(
    connection: &mut Connection,
    acc: &mut SqliteIoAcc,
    operation: impl FnOnce(&rusqlite::Transaction<'_>) -> R<()>,
) -> R<()> {
    let transaction = connection.transaction()?;
    operation(&transaction)?;
    transaction.commit()?;
    acc.commits += 1;
    Ok(())
}

fn sqlite_plan(connection: &Connection, sql: &str) -> R<Vec<String>> {
    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let details = statement
        .query_map([], |row| row.get(3))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(details)
}
fn assert_indexed_plan(label: &str, plan: &[String], index_names: &[&str]) {
    assert!(
        plan.iter().any(|detail| detail.contains("SEARCH")
            && index_names.iter().any(|name| detail.contains(name))),
        "{label} did not use an expected index: {plan:?}"
    );
    assert!(
        !plan.iter().any(|detail| detail.contains("SCAN edges")),
        "{label} scanned edges: {plan:?}"
    );
}

fn assert_plan_order(label: &str, plan: &[String], first: &str, second: &str) {
    let first_at = plan
        .iter()
        .position(|detail| detail.contains(first))
        .unwrap_or_else(|| panic!("{label} lacks {first:?}: {plan:?}"));
    let second_at = plan
        .iter()
        .position(|detail| detail.contains(second))
        .unwrap_or_else(|| panic!("{label} lacks {second:?}: {plan:?}"));
    assert!(
        first_at < second_at,
        "{label} has the wrong loop order ({first:?} must precede {second:?}): {plan:?}"
    );
}

fn assert_recursive_edge_primary_plan(plan: &[String]) {
    let recursive_at = plan
        .iter()
        .position(|detail| detail.contains("RECURSIVE STEP"))
        .unwrap_or_else(|| panic!("combined graph plan lacks recursive step: {plan:?}"));
    let frontier_at = plan
        .iter()
        .enumerate()
        .skip(recursive_at + 1)
        .find(|(_, detail)| detail.contains("SCAN r"))
        .map(|(at, _)| at)
        .unwrap_or_else(|| {
            panic!("combined graph recursive step does not scan frontier r: {plan:?}")
        });
    let edge_at = plan
        .iter()
        .enumerate()
        .skip(frontier_at + 1)
        .find(|(_, detail)| {
            detail.contains("SEARCH e USING")
                && detail.contains("PRIMARY KEY")
                && detail.contains("source_id=?")
        })
        .map(|(at, _)| at)
        .unwrap_or_else(|| {
            panic!("combined graph recursive step lacks point-source primary-key probe: {plan:?}")
        });
    assert!(recursive_at < frontier_at && frontier_at < edge_at);
    assert!(
        plan.iter()
            .skip(recursive_at + 1)
            .take(edge_at - recursive_at)
            .all(|detail| !detail.contains("edge_in")),
        "combined graph recursive step used broad edge_in search: {plan:?}"
    );
}

const SQLITE_SPATIAL_SQL: &str = "SELECT p.id FROM people_rtree r CROSS JOIN people p ON p.id=r.id WHERE r.min_lon<=144.01 AND r.max_lon>=144 AND r.min_lat<=-37.99 AND r.max_lat>=-38 AND p.longitude>=144 AND p.longitude<=144.01 AND p.latitude>=-38 AND p.latitude<=-37.99 ORDER BY p.id";
const SQLITE_TEXT_ACTIVE_SQL: &str = "SELECT p.id,p.embedding FROM people_fts f CROSS JOIN people p ON p.id=f.rowid WHERE people_fts MATCH 'flood' AND p.active=1";
const SQLITE_GRAPH_COMBINED_SQL: &str = "WITH RECURSIVE reach(id,depth) AS (SELECT destination_id,1 FROM edges WHERE context=0 AND source_collection=1 AND source_id=1 AND edge_type=1 AND destination_collection=1 UNION ALL SELECT e.destination_id,r.depth+1 FROM reach r CROSS JOIN edges AS e INDEXED BY sqlite_autoindex_edges_1 WHERE e.context=0 AND e.source_collection=1 AND e.source_id=r.id AND e.edge_type=1 AND e.destination_collection=1 AND r.depth<2) SELECT DISTINCT p.id,p.embedding FROM reach q CROSS JOIN people p ON p.id=q.id CROSS JOIN people_rtree s ON s.id=p.id WHERE p.active=1 AND s.min_lon<=144.01 AND s.max_lon>=144 AND s.min_lat<=-37.99 AND s.max_lat>=-38 AND p.longitude>=144 AND p.longitude<=144.01 AND p.latitude>=-38 AND p.latitude<=-37.99";
const SQLITE_MEMBERS_COMBINED_SQL: &str = "SELECT p.id,p.embedding FROM edges e CROSS JOIN people p ON p.id=e.source_id CROSS JOIN people_rtree r ON r.id=p.id WHERE e.context=0 AND e.source_collection=1 AND e.edge_type=2 AND e.destination_collection=2 AND e.destination_id=1 AND p.active=1 AND r.min_lon<=144.01 AND r.max_lon>=144 AND r.min_lat<=-37.99 AND r.max_lat>=-38 AND p.longitude>=144 AND p.longitude<=144.01 AND p.latitude>=-38 AND p.latitude<=-37.99";

fn run_sqlite(n: usize, dimension: usize, root: &Path, readers: ReaderMode) -> R<Value> {
    fs::create_dir_all(root.join("tmp"))?;
    std::env::set_var("SQLITE_TMPDIR", root.join("tmp"));
    let path = root.join("database.sqlite");
    let mut db = Connection::open(&path)?;
    sqlite_setup(&db)?;
    let mut sio = SqliteIoAcc::default();
    let mut io = serde_json::Map::new();
    let wal_autocheckpoint: i64 = db.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?;
    let page_size: i64 = db.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    io.insert("_meta".into(), json!({
        "page_size": page_size,
        "wal_autocheckpoint": wal_autocheckpoint,
        "synchronous": "FULL",
        "fullfsync": "ON",
        "journal_mode": "WAL",
        "fsync_basis": "derived: 1 WAL fsync per COMMIT (synchronous=FULL, fullfsync=ON) + 2 fsyncs per explicit TRUNCATE checkpoint (db+wal); auto-checkpoint fsyncs are not observed (no VFS hook)"
    }));
    let mut io_prev = (
        sqlite_db_status(&db, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE)?,
        sqlite_db_status(&db, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT)?,
        sqlite_db_status(&db, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS)?,
        sqlite_db_status(&db, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_SPILL)?,
        sio,
    );
    let mut peak = (0, 0);
    let start = Instant::now();
    sqlite_tx(&mut db, &mut sio, |tx| {
        for i in 0..100 {
            tx.execute(
                "INSERT INTO organizations(id,external_key,name) VALUES(?1,?2,?3)",
                params![i + 1, org_key(i), format!("Organization {i:03}")],
            )?;
        }
        Ok(())
    })?;
    for base in (0..n).step_by(BATCH) {
        sqlite_tx(&mut db, &mut sio, |tx| {
            for i in base..(base + BATCH).min(n) {
                let (x, y) = point(i);
                tx.execute("INSERT INTO people(id,external_key,age,name,active,body,embedding,longitude,latitude,profile) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![i+1,person_key(i),age(i),format!("Person {i:08}"),active(i),body(i),vector_blob(&vector(i,dimension)),x,y,profile().to_string()])?;
            }
            Ok(())
        })?;
        sample(root, &mut peak);
    }
    let entity_load = time_json(start, json!({"commits":n.div_ceil(BATCH)+1}));
    io.insert("entity_load".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    let start = Instant::now();
    for base in (0..n).step_by(BATCH) {
        sqlite_tx(&mut db, &mut sio, |tx| {
            for i in base..(base + BATCH).min(n) {
                for d in [(i + 1) % n, (i + 7) % n] {
                    tx.execute(
                        "INSERT INTO edges VALUES(0,1,?1,1,1,?2,'{}')",
                        params![i + 1, d + 1],
                    )?;
                }
                tx.execute(
                    "INSERT INTO edges VALUES(0,1,?1,2,2,?2,'{}')",
                    params![i + 1, i % 100 + 1],
                )?;
            }
            Ok(())
        })?;
        sample(root, &mut peak);
    }
    let graph_load = time_json(
        start,
        json!({"relationships":3*n,"forward_primary_key_and_reverse_index_maintained":true,"edge_table":"WITHOUT ROWID"}),
    );
    sqlite_truncate(&db, &mut sio)?;
    io.insert("graph_load".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    let loaded = sizes(root);
    let mut builds = serde_json::Map::new();
    let start = Instant::now();
    db.execute_batch("BEGIN; CREATE INDEX people_age ON people(age,id); CREATE INDEX people_active_age ON people(active,age,id); COMMIT;")?;
    sio.commits += 1;
    builds.insert(
        "scalar".into(),
        time_json(
            start,
            json!({"publication_commits":1,"policy":"SQLite atomic DDL"}),
        ),
    );
    io.insert("build_scalar".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    builds.insert("vector".into(),json!({"seconds":0.0,"publication_commits":0,"policy":"no native vector index; exact f32 blobs already loaded"}));
    io.insert("build_vector".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    let start = Instant::now();
    db.execute_batch("BEGIN; CREATE VIRTUAL TABLE people_rtree USING rtree(id,min_lon,max_lon,min_lat,max_lat); INSERT INTO people_rtree SELECT id,longitude,longitude,latitude,latitude FROM people; COMMIT;")?;
    sio.commits += 1;
    builds.insert(
        "spatial".into(),
        time_json(
            start,
            json!({"publication_commits":1,"policy":"SQLite atomic DDL+population"}),
        ),
    );
    io.insert("build_spatial".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    let start = Instant::now();
    db.execute_batch("BEGIN; CREATE VIRTUAL TABLE people_fts USING fts5(body,content='people',content_rowid='id',tokenize='unicode61 remove_diacritics 0'); INSERT INTO people_fts(people_fts) VALUES('rebuild'); CREATE VIRTUAL TABLE people_fts_vocab USING fts5vocab(people_fts,'row'); CREATE TABLE phase2_text_meta(documents INTEGER NOT NULL,tokens INTEGER NOT NULL); INSERT INTO phase2_text_meta SELECT count(*),coalesce(sum(length(body)-length(replace(body,' ',''))+1),0) FROM people; CREATE TRIGGER people_ai AFTER INSERT ON people BEGIN INSERT INTO people_rtree VALUES(new.id,new.longitude,new.longitude,new.latitude,new.latitude); INSERT INTO people_fts(rowid,body) VALUES(new.id,new.body); UPDATE phase2_text_meta SET documents=documents+1,tokens=tokens+(length(new.body)-length(replace(new.body,' ',''))+1); END; CREATE TRIGGER people_ad AFTER DELETE ON people BEGIN DELETE FROM people_rtree WHERE id=old.id; INSERT INTO people_fts(people_fts,rowid,body) VALUES('delete',old.id,old.body); UPDATE phase2_text_meta SET documents=documents-1,tokens=tokens-(length(old.body)-length(replace(old.body,' ',''))+1); END; CREATE TRIGGER people_au AFTER UPDATE ON people BEGIN DELETE FROM people_rtree WHERE id=old.id; INSERT INTO people_rtree VALUES(new.id,new.longitude,new.longitude,new.latitude,new.latitude); INSERT INTO people_fts(people_fts,rowid,body) VALUES('delete',old.id,old.body); INSERT INTO people_fts(rowid,body) VALUES(new.id,new.body); UPDATE phase2_text_meta SET tokens=tokens-(length(old.body)-length(replace(old.body,' ',''))+1)+(length(new.body)-length(replace(new.body,' ',''))+1); END; COMMIT;")?;
    sio.commits += 1;
    builds.insert(
        "text".into(),
        time_json(
            start,
            json!({"publication_commits":1,"policy":"SQLite atomic DDL+FTS rebuild"}),
        ),
    );
    io.insert("build_text".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    sample(root, &mut peak);
    let delete_out_plan = sqlite_plan(
        &db,
        "DELETE FROM edges WHERE context=0 AND source_collection=1 AND source_id=1",
    )?;
    assert_indexed_plan(
        "outgoing cascade delete",
        &delete_out_plan,
        &["PRIMARY KEY"],
    );
    let delete_in_plan = sqlite_plan(
        &db,
        "DELETE FROM edges WHERE context=0 AND destination_collection=1 AND destination_id=1",
    )?;
    assert_indexed_plan("incoming cascade delete", &delete_in_plan, &["edge_in"]);
    let membership_plan=sqlite_plan(&db,"SELECT p.id FROM edges e CROSS JOIN people p ON p.id=e.source_id WHERE e.context=0 AND e.source_collection=1 AND e.edge_type=2 AND e.destination_collection=2 AND e.destination_id=1")?;
    assert_indexed_plan("incoming membership", &membership_plan, &["edge_in"]);
    let text_active_plan = sqlite_plan(&db, SQLITE_TEXT_ACTIVE_SQL)?;
    assert_plan_order(
        "text+active",
        &text_active_plan,
        "VIRTUAL TABLE INDEX",
        "SEARCH p USING INTEGER PRIMARY KEY",
    );
    let spatial_plan = sqlite_plan(&db, SQLITE_SPATIAL_SQL)?;
    assert_plan_order(
        "spatial bbox",
        &spatial_plan,
        "VIRTUAL TABLE INDEX",
        "SEARCH p USING INTEGER PRIMARY KEY",
    );
    let graph_combined_plan = sqlite_plan(&db, SQLITE_GRAPH_COMBINED_SQL)?;
    assert_indexed_plan(
        "combined graph recursive expansion",
        &graph_combined_plan,
        &["PRIMARY KEY"],
    );
    assert_recursive_edge_primary_plan(&graph_combined_plan);
    assert_plan_order(
        "combined graph final join",
        &graph_combined_plan,
        "SCAN q",
        "SEARCH p USING INTEGER PRIMARY KEY",
    );
    let members_combined_plan = sqlite_plan(&db, SQLITE_MEMBERS_COMBINED_SQL)?;
    assert_indexed_plan(
        "members combined incoming expansion",
        &members_combined_plan,
        &["edge_in"],
    );
    let sqlite_query_plans = json!({"delete_outgoing":delete_out_plan,"delete_incoming":delete_in_plan,"incoming_membership":membership_plan,"spatial":spatial_plan,"text_active":text_active_plan,"combined_graph":graph_combined_plan,"combined_members":members_combined_plan});
    let query_vector = vector(17 % n, dimension);
    let oracle = independent_vector(n, dimension, &query_vector, 10);
    let mut queries = serde_json::Map::new();
    let start = Instant::now();
    let scalar: Vec<u64> = {
        let mut statement = db
            .prepare("SELECT id FROM people WHERE active=1 AND age=30 ORDER BY age,id LIMIT 100")?;
        let collected = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        collected
    };
    queries.insert(
        "scalar_active_age".into(),
        time_json(start, json!({"hits":scalar.len()})),
    );
    let expected_scalar = (0..n)
        .filter(|&i| active(i) && age(i) == 30)
        .map(|i| i as u64 + 1)
        .take(100)
        .collect::<Vec<_>>();
    assert_eq!(scalar, expected_scalar);
    let start = Instant::now();
    let mut all = Vec::new();
    {
        let mut statement = db.prepare("SELECT id,embedding FROM people")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if let Some(distance) = cosine(&query_vector, &blob_vector(&blob)) {
                all.push(Ranked { distance, id });
                all.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
                if all.len() > 10 {
                    all.pop();
                }
            }
        }
    }
    all.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
    queries.insert(
        "vector_cosine_k10".into(),
        time_json(start, json!({"hits":all.len()})),
    );
    assert_eq!(
        all.iter().map(|h| h.id).collect::<Vec<_>>(),
        oracle.iter().map(|h| h.id).collect::<Vec<_>>()
    );
    let start = Instant::now();
    let spatial: Vec<u64> = {
        let mut statement = db.prepare(SQLITE_SPATIAL_SQL)?;
        let values = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        values
    };
    queries.insert(
        "spatial_bbox".into(),
        time_json(start, json!({"hits":spatial.len()})),
    );
    let expected_spatial = (0..n)
        .filter(|&i| {
            let (x, y) = point(i);
            (144.0..=144.01).contains(&x) && (-38.0..=-37.99).contains(&y)
        })
        .map(|i| i as u64 + 1)
        .collect::<Vec<_>>();
    assert_eq!(spatial, expected_spatial);
    let start = Instant::now();
    let native: Vec<(i64, f64)> = {
        let mut s=db.prepare("SELECT rowid,bm25(people_fts) FROM people_fts WHERE people_fts MATCH 'flood' ORDER BY bm25(people_fts),rowid LIMIT 10")?;
        let collected = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        collected
    };
    queries.insert("sqlite_native_bm25_k10".into(),time_json(start,json!({"hits":native.len(),"semantics":"SQLite FTS5 native rank; not cross-engine BM25"})));
    let start = Instant::now();
    let (documents, corpus_tokens): (u64, u64) =
        db.query_row("SELECT documents,tokens FROM phase2_text_meta", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let df: u64 = db.query_row(
        "SELECT doc FROM people_fts_vocab WHERE term='flood'",
        [],
        |r| r.get(0),
    )?;
    let total = documents as f64;
    let average = corpus_tokens as f64 / total;
    let idf = (1.0 + (total - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
    let mut common = Vec::with_capacity(11);
    {
        let mut statement=db.prepare("SELECT p.id,p.body FROM people_fts CROSS JOIN people p ON p.id=people_fts.rowid WHERE people_fts MATCH 'flood'")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let text: String = row.get(1)?;
            let len = text.split_whitespace().count() as f64;
            let tf = text
                .split_whitespace()
                .filter(|term| *term == "flood")
                .count() as f64;
            let score = idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * len / average));
            common.push((id, score));
            common.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            if common.len() > 10 {
                common.pop();
            }
        }
    }
    common.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let common_seconds = start.elapsed().as_secs_f64();
    let text_oracle = independent_text(n, "flood", 10);
    assert_eq!(
        common.iter().map(|hit| hit.0).collect::<Vec<_>>(),
        text_oracle.iter().map(|hit| hit.0).collect::<Vec<_>>()
    );
    for (actual, expected) in common.iter().zip(&text_oracle) {
        assert!((actual.1 - expected.1).abs() < 1e-12);
    }
    queries.insert("text_positive_bm25_k10".into(),json!({"seconds":common_seconds,"hits":common.len(),"semantics":"FTS MATCH candidate-first CROSS JOIN cost plus phase2 positive BM25 scoring"}));
    let start = Instant::now();
    let mut combined = Vec::new();
    {
        let mut statement = db.prepare(SQLITE_GRAPH_COMBINED_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if let Some(distance) = cosine(&query_vector, &blob_vector(&blob)) {
                combined.push(Ranked { distance, id });
            }
        }
    }
    combined.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
    combined.truncate(10);
    queries.insert("combined_graph_active_bbox_vector".into(),time_json(start,json!({"hits":combined.len(),"driver":"recursive edge-index candidates first via CROSS JOIN, then scalar + RTree + exact Rust f32 ranking"})));
    let combined_oracle = independent_combined(n, dimension, &query_vector);
    assert_eq!(
        combined.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        combined_oracle.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    let start = Instant::now();
    let mut text_vector = Vec::with_capacity(11);
    {
        let mut statement = db.prepare(SQLITE_TEXT_ACTIVE_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if let Some(distance) = cosine(&query_vector, &blob_vector(&blob)) {
                text_vector.push(Ranked { distance, id });
                text_vector.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
                if text_vector.len() > 10 {
                    text_vector.pop();
                }
            }
        }
    }
    queries.insert("text_active_vector".into(),time_json(start,json!({"hits":text_vector.len(),"driver":"FTS MATCH candidates first via CROSS JOIN, then active refinement + exact Rust f32 ranking"})));
    let text_vector_oracle = independent_text_active_vector(n, dimension, &query_vector);
    assert_eq!(
        text_vector.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        text_vector_oracle
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>()
    );
    let start = Instant::now();
    let mut member_vector = Vec::with_capacity(11);
    {
        let mut statement = db.prepare(SQLITE_MEMBERS_COMBINED_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if let Some(distance) = cosine(&query_vector, &blob_vector(&blob)) {
                member_vector.push(Ranked { distance, id });
                member_vector
                    .sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
                if member_vector.len() > 10 {
                    member_vector.pop();
                }
            }
        }
    }
    queries.insert("members_active_spatial_vector".into(),time_json(start,json!({"hits":member_vector.len(),"driver":"incoming edge-index candidates first via CROSS JOIN, then active + RTree overlap/exact refinement + exact Rust f32 ranking"})));
    let member_oracle = independent_members_active_spatial_vector(n, dimension, &query_vector);
    assert_eq!(
        member_vector.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        member_oracle.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    // The same four traversals, the same hundred seeds, the same counts checked
    // against the same independent formulas -- see the E4 arm for why this group
    // exists. Statements are prepared once; only execution and row draining count.
    const SQLITE_OUT_1HOP_SQL: &str = "SELECT destination_id FROM edges WHERE context=0 AND source_collection=1 AND source_id=?1 AND edge_type=1 AND destination_collection=1";
    const SQLITE_IN_1HOP_SQL: &str = "SELECT source_id FROM edges WHERE context=0 AND destination_collection=1 AND destination_id=?1 AND edge_type=1 AND source_collection=1";
    const SQLITE_MEMBERS_OF_ORG_SQL: &str = "SELECT source_id FROM edges WHERE context=0 AND destination_collection=2 AND destination_id=?1 AND edge_type=2 AND source_collection=1";
    // CROSS JOIN + INDEXED BY, like the combined graph query above: left to itself
    // the planner drives the recursive step from a broad edge_in range scan instead
    // of probing one source at a time, which is a plan SQLite should not be judged on.
    const SQLITE_BFS_3HOP_SQL: &str = "WITH RECURSIVE reach(id,depth) AS (SELECT ?1,0 UNION SELECT e.destination_id,r.depth+1 FROM reach r CROSS JOIN edges AS e INDEXED BY sqlite_autoindex_edges_1 WHERE e.context=0 AND e.source_collection=1 AND e.source_id=r.id AND e.edge_type=1 AND e.destination_collection=1 AND r.depth<3 UNION SELECT e.source_id,r.depth+1 FROM reach r CROSS JOIN edges AS e INDEXED BY edge_in WHERE e.context=0 AND e.destination_collection=1 AND e.destination_id=r.id AND e.edge_type=1 AND e.source_collection=1 AND r.depth<3) SELECT DISTINCT id FROM reach LIMIT 1000";

    // EXPLAIN QUERY PLAN needs a bound statement, so plan the literal-seed spelling.
    let out_1hop_plan = sqlite_plan(&db, &SQLITE_OUT_1HOP_SQL.replace("?1", "1"))?;
    assert_indexed_plan("outgoing 1-hop", &out_1hop_plan, &["PRIMARY KEY"]);
    let in_1hop_plan = sqlite_plan(&db, &SQLITE_IN_1HOP_SQL.replace("?1", "1"))?;
    assert_indexed_plan("incoming 1-hop", &in_1hop_plan, &["edge_in"]);
    let members_of_org_plan = sqlite_plan(&db, &SQLITE_MEMBERS_OF_ORG_SQL.replace("?1", "1"))?;
    assert_indexed_plan("members of organization", &members_of_org_plan, &["edge_in"]);
    let bfs_3hop_plan = sqlite_plan(&db, &SQLITE_BFS_3HOP_SQL.replace("?1", "1"))?;
    assert!(
        bfs_3hop_plan.iter().any(|detail| detail.contains("SEARCH e USING PRIMARY KEY")
            && detail.contains("source_id=?")),
        "3-hop BFS forward step is not a point probe: {bfs_3hop_plan:?}"
    );
    assert!(
        bfs_3hop_plan.iter().any(|detail| detail.contains("SEARCH e USING COVERING INDEX edge_in")
            && detail.contains("destination_id=?")),
        "3-hop BFS reverse step is not a point probe: {bfs_3hop_plan:?}"
    );
    assert!(
        !bfs_3hop_plan.iter().any(|detail| detail.contains("SCAN edges")),
        "3-hop BFS scanned edges: {bfs_3hop_plan:?}"
    );

    // Record the traversal plans next to the others, as the fairness evidence.
    let mut sqlite_query_plans = sqlite_query_plans;
    sqlite_query_plans["traversal_out_1hop"] = json!(out_1hop_plan);
    sqlite_query_plans["traversal_in_1hop"] = json!(in_1hop_plan);
    sqlite_query_plans["traversal_members_of_org"] = json!(members_of_org_plan);
    sqlite_query_plans["traversal_bfs_3hop"] = json!(bfs_3hop_plan);

    let graph_seeds = (0..GRAPH_SEEDS).map(|j| graph_seed(j, n)).collect::<Vec<_>>();
    let warm_seed = graph_seed(0, n);

    let mut out_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut out_rows = 0usize;
    {
        let mut statement = db.prepare(SQLITE_OUT_1HOP_SQL)?;
        statement
            .query_map(params![warm_seed as i64 + 1], |row| row.get::<_, u64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for &seed in &graph_seeds {
            let start = Instant::now();
            let mut found = BTreeSet::new();
            let mut rows = statement.query(params![seed as i64 + 1])?;
            while let Some(row) = rows.next()? {
                found.insert(row.get::<_, u64>(0)?);
            }
            out_micros.push(start.elapsed().as_secs_f64() * 1e6);
            assert_eq!(found, independent_knows_out(n, seed));
            out_rows += found.len();
        }
    }
    queries.insert(
        "graph_out_1hop".into(),
        graph_timing_json(
            out_micros,
            out_rows,
            json!({"work":"outgoing knows neighbours through the edges primary key"}),
        ),
    );

    let mut in_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut in_rows = 0usize;
    {
        let mut statement = db.prepare(SQLITE_IN_1HOP_SQL)?;
        statement
            .query_map(params![warm_seed as i64 + 1], |row| row.get::<_, u64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for &seed in &graph_seeds {
            let start = Instant::now();
            let mut found = BTreeSet::new();
            let mut rows = statement.query(params![seed as i64 + 1])?;
            while let Some(row) = rows.next()? {
                found.insert(row.get::<_, u64>(0)?);
            }
            in_micros.push(start.elapsed().as_secs_f64() * 1e6);
            assert_eq!(found, independent_knows_in(n, seed));
            in_rows += found.len();
        }
    }
    queries.insert(
        "graph_in_1hop".into(),
        graph_timing_json(
            in_micros,
            in_rows,
            json!({"work":"incoming knows neighbours through the edge_in reverse index"}),
        ),
    );

    let mut bfs_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut bfs_rows = 0usize;
    {
        let mut statement = db.prepare(SQLITE_BFS_3HOP_SQL)?;
        statement
            .query_map(params![warm_seed as i64 + 1], |row| row.get::<_, u64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for &seed in &graph_seeds {
            let start = Instant::now();
            let mut visited = 0usize;
            let mut rows = statement.query(params![seed as i64 + 1])?;
            while let Some(row) = rows.next()? {
                let _: u64 = row.get(0)?;
                visited += 1;
            }
            bfs_micros.push(start.elapsed().as_secs_f64() * 1e6);
            assert_eq!(visited, independent_bfs_visited(n, seed, GRAPH_BFS_DEPTH));
            bfs_rows += visited;
        }
    }
    queries.insert(
        "graph_bfs_3hop".into(),
        graph_timing_json(
            bfs_micros,
            bfs_rows,
            json!({"work":"WITH RECURSIVE over edges, forward and reverse, depth<=3, DISTINCT visited set including the seed, LIMIT 1000"}),
        ),
    );

    let mut org_micros = Vec::with_capacity(GRAPH_SEEDS);
    let mut org_rows = 0usize;
    {
        let mut statement = db.prepare(SQLITE_MEMBERS_OF_ORG_SQL)?;
        statement
            .query_map(params![(warm_seed % 100) as i64 + 1], |row| {
                row.get::<_, u64>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for &seed in &graph_seeds {
            let start = Instant::now();
            let mut members = 0usize;
            let mut rows = statement.query(params![(seed % 100) as i64 + 1])?;
            while let Some(row) = rows.next()? {
                let _: u64 = row.get(0)?;
                members += 1;
            }
            org_micros.push(start.elapsed().as_secs_f64() * 1e6);
            assert_eq!(
                members,
                independent_org_member_count(n, (seed % 100) as u64 + 1)
            );
            org_rows += members;
        }
    }
    queries.insert(
        "graph_members_of_org".into(),
        graph_timing_json(
            org_micros,
            org_rows,
            json!({"work":"every person with a member_of edge to organization (seed%100)+1, through the edge_in reverse index"}),
        ),
    );

    io.insert("pre_crud_queries".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    let held = matches!(readers, ReaderMode::Held)
        .then(|| Connection::open(&path))
        .transpose()?;
    if let Some(reader) = held.as_ref() {
        reader.execute_batch("BEGIN")?;
        let _: i64 = reader.query_row("SELECT count(*) FROM people", [], |r| r.get(0))?;
    }
    let mut current_ids = (1..=n as i64).collect::<Vec<_>>();
    let mut crud = Vec::new();
    for cycle in 0..3usize {
        let short = if matches!(readers, ReaderMode::Short) {
            let reader = Connection::open(&path)?;
            reader.execute_batch("BEGIN")?;
            assert_sqlite_snapshot_person(&reader, n, dimension, cycle)?;
            Some(reader)
        } else {
            None
        };
        let mut batch = if matches!(readers, ReaderMode::Batch) {
            let reader = Connection::open(&path)?;
            reader.execute_batch("BEGIN")?;
            assert_sqlite_snapshot_person(&reader, n, dimension, cycle)?;
            Some(reader)
        } else {
            None
        };
        let start = Instant::now();
        for base in (0..n).step_by(BATCH) {
            sqlite_tx(&mut db, &mut sio, |tx| {
                for i in base..(base + BATCH).min(n) {
                    let (x, y) = point(i);
                    tx.execute("UPDATE people SET age=?2,body=?3,embedding=?4,longitude=?5,latitude=?6 WHERE id=?1",params![current_ids[i],age(i)+cycle as i64+1,format!("{} cycle{cycle}",body(i)),vector_blob(&vector((i+cycle+1)%n,dimension)),x+0.00001,y])?;
                    tx.execute(
                        "INSERT OR REPLACE INTO edges VALUES(0,1,?1,1,1,?2,?3)",
                        params![
                            current_ids[i],
                            current_ids[(i + 1) % n],
                            json!({"round":cycle,"slot":1}).to_string()
                        ],
                    )?;
                    tx.execute(
                        "INSERT OR REPLACE INTO edges VALUES(0,1,?1,1,1,?2,?3)",
                        params![
                            current_ids[i],
                            current_ids[(i + 7) % n],
                            json!({"round":cycle,"slot":7}).to_string()
                        ],
                    )?;
                    tx.execute(
                        "INSERT OR REPLACE INTO edges VALUES(0,1,?1,2,2,?2,?3)",
                        params![
                            current_ids[i],
                            i % 100 + 1,
                            json!({"round":cycle}).to_string()
                        ],
                    )?;
                }
                Ok(())
            })?;
            sample(root, &mut peak);
            if let Some(reader) = batch.take() {
                assert_sqlite_snapshot_person(&reader, n, dimension, cycle)?;
                reader.execute_batch("COMMIT")?;
                drop(reader);
            }
        }
        let update_s = start.elapsed().as_secs_f64();
        io.insert(format!("crud_cycle_{cycle}_update"), sqlite_io_delta(&db, &sio, &mut io_prev)?);
        let deleted_indices = (0..n).step_by(10).collect::<Vec<_>>();
        let start = Instant::now();
        for chunk in deleted_indices.chunks(BATCH) {
            sqlite_tx(&mut db, &mut sio, |tx| {
                for &i in chunk {
                    tx.execute("DELETE FROM edges WHERE context=0 AND source_collection=1 AND source_id=?1",params![current_ids[i]])?;
                    tx.execute("DELETE FROM edges WHERE context=0 AND destination_collection=1 AND destination_id=?1",params![current_ids[i]])?;
                    tx.execute("DELETE FROM people WHERE id=?1", params![current_ids[i]])?;
                }
                Ok(())
            })?;
            sample(root, &mut peak);
        }
        let delete_s = start.elapsed().as_secs_f64();
        io.insert(format!("crud_cycle_{cycle}_delete"), sqlite_io_delta(&db, &sio, &mut io_prev)?);
        let start = Instant::now();
        for chunk in deleted_indices.chunks(BATCH) {
            sqlite_tx(&mut db, &mut sio, |tx| {
                for &i in chunk {
                    let (x, y) = point(i);
                    tx.execute("INSERT INTO people(external_key,age,name,active,body,embedding,longitude,latitude,profile) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![person_key(i),age(i)+cycle as i64+1,format!("Person {i:08}"),active(i),format!("{} cycle{cycle}",body(i)),vector_blob(&vector((i+cycle+1)%n,dimension)),x+0.00001,y,profile().to_string()])?;
                    current_ids[i] = tx.last_insert_rowid();
                }
                Ok(())
            })?;
            sample(root, &mut peak);
        }
        for chunk in deleted_indices.chunks(BATCH) {
            sqlite_tx(&mut db, &mut sio, |tx| {
                for &i in chunk {
                    tx.execute(
                        "INSERT INTO edges VALUES(0,1,?1,1,1,?2,?3)",
                        params![
                            current_ids[i],
                            current_ids[(i + 1) % n],
                            json!({"round":cycle,"slot":1}).to_string()
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO edges VALUES(0,1,?1,1,1,?2,?3)",
                        params![
                            current_ids[i],
                            current_ids[(i + 7) % n],
                            json!({"round":cycle,"slot":7}).to_string()
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO edges VALUES(0,1,?1,2,2,?2,?3)",
                        params![
                            current_ids[i],
                            i % 100 + 1,
                            json!({"round":cycle}).to_string()
                        ],
                    )?;
                }
                Ok(())
            })?;
            sample(root, &mut peak);
        }
        let insert_s = start.elapsed().as_secs_f64();
        io.insert(format!("crud_cycle_{cycle}_reinsert"), sqlite_io_delta(&db, &sio, &mut io_prev)?);
        let observed_age: i64 = db.query_row(
            "SELECT age FROM people WHERE external_key=?1",
            params![person_key(1)],
            |row| row.get(0),
        )?;
        assert_eq!(observed_age, age(1) + cycle as i64 + 1);
        let mut destinations = {
            let mut statement=db.prepare("SELECT destination_id FROM edges WHERE context=0 AND source_collection=1 AND source_id=?1 AND edge_type=1 ORDER BY destination_id")?;
            let values = statement
                .query_map(params![current_ids[1]], |row| row.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;
            values
        };
        let mut expected = vec![current_ids[2], current_ids[8]];
        destinations.sort();
        expected.sort();
        assert_eq!(destinations, expected);
        if let Some(reader) = short.as_ref() {
            assert_sqlite_snapshot_person(reader, n, dimension, cycle)?;
            reader.execute_batch("COMMIT")?;
        }
        drop(short);
        crud.push(json!({"cycle":cycle,"profile":"scale","update_seconds":update_s,"delete_seconds":delete_s,"reinsert_edges_seconds":insert_s,"updated":n,"deleted_reinserted":deleted_indices.len(),"relationships_reasserted_during_update":3*n,"deleted_fraction":deleted_indices.len() as f64/n as f64}));
    }
    {
        let mut statement=db.prepare("SELECT id,external_key,age,name,active,body,embedding,longitude,latitude,profile FROM people")?;
        let mut rows = statement.query([])?;
        let mut verified = 0usize;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let key: String = row.get(1)?;
            let i: usize = key.strip_prefix("person/").unwrap().parse()?;
            assert_eq!(id, current_ids[i]);
            assert_eq!(row.get::<_, i64>(2)?, age(i) + 3);
            assert_eq!(row.get::<_, String>(3)?, format!("Person {i:08}"));
            assert_eq!(row.get::<_, bool>(4)?, active(i));
            assert_eq!(row.get::<_, String>(5)?, format!("{} cycle2", body(i)));
            assert_eq!(
                row.get::<_, Vec<u8>>(6)?,
                vector_blob(&vector((i + 3) % n, dimension))
            );
            let (x, y) = point(i);
            assert_eq!(
                (row.get::<_, f64>(7)?, row.get::<_, f64>(8)?),
                (x + 0.00001, y)
            );
            assert_eq!(
                serde_json::from_str::<Value>(&row.get::<_, String>(9)?)?,
                profile()
            );
            verified += 1;
        }
        assert_eq!(verified, n);
    }
    let relationship_count: i64 =
        db.query_row("SELECT count(*) FROM edges", [], |row| row.get(0))?;
    assert_eq!(relationship_count as usize, expected_final_relationships(n));
    let id_sequences = current_ids.iter().map(|id| *id as u64).collect::<Vec<_>>();
    let mut post_crud_queries = serde_json::Map::new();
    let start = Instant::now();
    let scalar: Vec<u64> = {
        let mut statement = db.prepare("SELECT id FROM people WHERE age=30 ORDER BY id")?;
        let values = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        values
    };
    post_crud_queries.insert(
        "scalar_age".into(),
        time_json(start, json!({"hits":scalar.len()})),
    );
    let mut expected_scalar = (0..n)
        .filter(|&i| age(i) + 3 == 30)
        .map(|i| id_sequences[i])
        .collect::<Vec<_>>();
    expected_scalar.sort();
    assert_eq!(scalar, expected_scalar);
    let final_vector = final_vector_oracle(n, dimension, &query_vector, &id_sequences, |_| true);
    let start = Instant::now();
    let mut vector_hits = Vec::with_capacity(11);
    {
        let mut statement = db.prepare("SELECT id,embedding FROM people")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if let Some(distance) = cosine(&query_vector, &blob_vector(&blob)) {
                vector_hits.push(Ranked { distance, id });
                vector_hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
                if vector_hits.len() > 10 {
                    vector_hits.pop();
                }
            }
        }
    }
    post_crud_queries.insert(
        "vector".into(),
        time_json(start, json!({"hits":vector_hits.len()})),
    );
    assert_eq!(
        vector_hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        final_vector.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    let final_spatial = final_spatial_oracle(n, &id_sequences);
    let start = Instant::now();
    let spatial_hits: Vec<u64> = {
        let mut statement = db.prepare(SQLITE_SPATIAL_SQL)?;
        let values = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        values
    };
    post_crud_queries.insert(
        "spatial".into(),
        time_json(start, json!({"hits":spatial_hits.len()})),
    );
    assert_eq!(spatial_hits, final_spatial);
    let final_text = final_text_oracle(n, &id_sequences);
    let start = Instant::now();
    let (documents, tokens): (f64, f64) =
        db.query_row("SELECT documents,tokens FROM phase2_text_meta", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let df: f64 = db.query_row(
        "SELECT doc FROM people_fts_vocab WHERE term='flood'",
        [],
        |row| row.get(0),
    )?;
    let average = tokens / documents;
    let idf = (1.0 + (documents - df + 0.5) / (df + 0.5)).ln();
    let mut text_hits = Vec::with_capacity(11);
    {
        let mut statement=db.prepare("SELECT p.id,p.body FROM people_fts f CROSS JOIN people p ON p.id=f.rowid WHERE people_fts MATCH 'flood'")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let text: String = row.get(1)?;
            let tf = text
                .split_whitespace()
                .filter(|term| *term == "flood")
                .count() as f64;
            let len = text.split_whitespace().count() as f64;
            text_hits.push((
                id,
                idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * len / average)),
            ));
            text_hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            if text_hits.len() > 10 {
                text_hits.pop();
            }
        }
    }
    post_crud_queries.insert(
        "text".into(),
        time_json(start, json!({"hits":text_hits.len()})),
    );
    assert_eq!(
        text_hits.iter().map(|hit| hit.0).collect::<Vec<_>>(),
        final_text.iter().map(|hit| hit.0).collect::<Vec<_>>()
    );
    for (actual, expected) in text_hits.iter().zip(&final_text) {
        assert!((actual.1 - expected.1).abs() < 1e-12);
    }
    io.insert("post_crud_queries".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    if let Some(reader) = held.as_ref() {
        let count: i64 = reader.query_row("SELECT count(*) FROM people", [], |r| r.get(0))?;
        assert_eq!(count, n as i64);
        let age_value: i64 =
            reader.query_row("SELECT age FROM people WHERE id=2", [], |row| row.get(0))?;
        assert_eq!(age_value, age(1));
        let destinations = {
            let mut statement=reader.prepare("SELECT destination_id FROM edges WHERE context=0 AND source_collection=1 AND source_id=2 AND edge_type=1 ORDER BY destination_id")?;
            let values = statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;
            values
        };
        assert_eq!(destinations, vec![3, 9]);
        reader.execute_batch("COMMIT")?;
    }
    drop(held);
    sqlite_truncate(&db, &mut sio)?;
    io.insert("final_checkpoint".into(), sqlite_io_delta(&db, &sio, &mut io_prev)?);
    let final_size = sizes(root);
    drop(db);
    let start = Instant::now();
    let reopened = Connection::open(&path)?;
    reopened.execute_batch("PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA journal_mode=WAL; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE")?;
    let table: String = reopened.query_row(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name='people'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(table, "people");
    let reopen_s = start.elapsed().as_secs_f64();
    let count: i64 = reopened.query_row("SELECT count(*) FROM people", [], |r| r.get(0))?;
    assert_eq!(count, n as i64);
    let age_one: i64 = reopened.query_row(
        "SELECT age FROM people WHERE external_key=?1",
        params![person_key(1)],
        |row| row.get(0),
    )?;
    assert_eq!(age_one, age(1) + 3);
    let reopened_scalar: Vec<u64> = {
        let mut statement = reopened.prepare("SELECT id FROM people WHERE age=30 ORDER BY id")?;
        let values = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        values
    };
    assert_eq!(reopened_scalar, expected_scalar);
    let reopened_spatial: Vec<u64> = {
        let mut statement = reopened.prepare(SQLITE_SPATIAL_SQL)?;
        let values = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        values
    };
    assert_eq!(reopened_spatial, final_spatial);
    let mut reopened_vector = Vec::with_capacity(11);
    {
        let mut statement = reopened.prepare("SELECT id,embedding FROM people")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            if let Some(distance) = cosine(&query_vector, &blob_vector(&blob)) {
                reopened_vector.push(Ranked { distance, id });
                reopened_vector
                    .sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
                if reopened_vector.len() > 10 {
                    reopened_vector.pop();
                }
            }
        }
    }
    assert_eq!(
        reopened_vector.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        final_vector.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    let fts_count: i64 = reopened.query_row(
        "SELECT count(*) FROM people_fts WHERE people_fts MATCH 'flood'",
        [],
        |row| row.get(0),
    )?;
    assert!(fts_count > 0);
    Ok(
        json!({"engine":"sqlite","entity_load":entity_load,"graph_load":graph_load,"builds":builds,"queries":queries,"crud":crud,"post_crud_queries":post_crud_queries,"io":io,"sqlite_query_plans":sqlite_query_plans,"loaded_bytes":loaded,"final_bytes":final_size,"sampled_peak_bytes":peak,"reopen_seconds":reopen_s,"reader_mode":readers.name(),"reader_scope":readers.scope(),"publication_policy":"native SQLite atomic DDL","rss_hwm":hwm(),"sqlite_version":rusqlite::version(),"runtime_settings":{"journal_mode":"WAL","synchronous":"FULL","fullfsync":"ON (macOS parity with E4 F_FULLFSYNC)","cache_bytes":CACHE,"temp_store":"FILE","vector":"identical Rust exact f32 math over blobs","fts":"FTS5 unicode61 remove_diacritics=0"}}),
    )
}

fn is_typed_resource_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        error.downcast_ref::<CollectionError>(),
        Some(CollectionError::Kernel(kernel::Error::ResourceLimit(_)))
    )
}

fn main() -> R<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 6 {
        return Err("usage: phase2_multimodel_bench e4|sqlite N DIM FRESH_DIR atomic|resumable none|batch|short|held".into());
    }
    let engine = &args[0];
    let n: usize = args[1].parse()?;
    let dimension: usize = args[2].parse()?;
    let root = Path::new(&args[3]);
    let policy = &args[4];
    let readers = ReaderMode::parse(&args[5])?;
    if n < 8 || n > 1_000_000 {
        return Err("N must be 8..=1000000".into());
    }
    if !matches!(dimension, 32 | 1536) {
        return Err("DIM must be 32 or 1536".into());
    }
    if dimension == 1536 && n > 10_000 {
        return Err("1536-dimensional sample is bounded to N<=10000".into());
    }
    if root.exists() {
        return Err("benchmark directory must be fresh".into());
    }
    if engine == "sqlite" && policy != "atomic" {
        return Err("SQLite late DDL has atomic publication; use atomic".into());
    }
    if engine == "e4" && !matches!(policy.as_str(), "atomic" | "resumable") {
        return Err("E4 policy must be atomic|resumable".into());
    }
    let start = Instant::now();
    let result = match engine.as_str() {
        "e4" => {
            let mut progress = E4Progress::new(n);
            match run_e4(n, dimension, root, policy, readers, &mut progress) {
                Ok(result) => result,
                Err(error) if is_typed_resource_limit(error.as_ref()) => {
                    let refusal_reason = error.to_string();
                    let oracle_started = Instant::now();
                    let mut oracle = verify_refused_e4(root, n, dimension, &progress).map_err(
                        |oracle| {
                            format!(
                                "typed resource refusal at {} but committed-state verification failed: {oracle}",
                                progress.stage
                            )
                        },
                    )?;
                    oracle["seconds"] = json!(oracle_started.elapsed().as_secs_f64());
                    json!({"engine":"e4","refused":true,"operation":"late build or scale CRUD under configured storage limits","reason":refusal_reason,"classification":"collections::Error::Kernel(kernel::Error::ResourceLimit)","publication_policy":policy,"reader_mode":readers.name(),"reader_scope":readers.scope(),"progress":progress.evidence(),"refusal_oracle":oracle,"partial_tree_bytes":sizes(root),"rss_hwm":hwm()})
                }
                Err(error) => return Err(error),
            }
        }
        "sqlite" => run_sqlite(n, dimension, root, readers)?,
        _ => return Err("engine must be e4|sqlite".into()),
    };
    println!(
        "{}",
        json!({"format":"phase2-multimodel-bench-v1","profile":"scale-full-population-3round","generator":"phase2-synthetic-v1","input_digest":digest(n,dimension),"rows":n,"organizations":100,"relationships":3*n,"dimension":dimension,"cache_bytes":CACHE,"commit_batch":BATCH,"durability":"FULL WAL","disk_scope":"all regular files recursively under dedicated root including tmp","peak_sampling":"in-process after commits/stages; discrete lower bound; driver adds timed polling and SQLite unlinked temp remains unobservable","refusal_accounting":"timed workload retains only compact committed boundaries; the bounded streaming committed-state oracle runs after a typed resource refusal and outside named stage timings","e4_create_compact_cells":create_compact_cells(),"compile_features":{"compact-cells":cfg!(feature="compact-cells"),"sqlite-balance":cfg!(feature="sqlite-balance"),"keyspace-append":cfg!(feature="keyspace-append"),"slotref-split":cfg!(feature="slotref-split")},"elapsed_seconds":start.elapsed().as_secs_f64(),"result":result})
    );
    Ok(())
}

#[cfg(test)]
mod ledger_tests {
    use super::*;
    use e4_prototype::collections::EdgeTypeId;

    fn replay_edges(state: CompactCommittedState, source: usize) -> Vec<(EdgeKey, Value)> {
        expected_edges(
            state,
            20,
            source,
            CollectionId(1),
            CollectionId(2),
            EdgeTypeId(1),
            EdgeTypeId(2),
        )
    }

    #[test]
    fn compact_ledger_replays_partial_delete_reinsert_and_edge_restore() {
        let mut state = CompactCommittedState {
            entity_loaded: 20,
            graph_edges_loaded: 60,
            updated: [20, 0, 0],
            deleted: [1, 0, 0],
            reinserted: [0, 0, 0],
            restored: [0, 0, 0],
        };

        assert_eq!(expected_document_version(state, 0), -1);
        assert!(replay_edges(state, 0).is_empty());
        assert_eq!(replay_edges(state, 13).len(), 2);
        assert_eq!(replay_edges(state, 19).len(), 2);

        state.reinserted[0] = 1;
        assert_eq!(sequence_through(state, 20, 0, 3), 21);
        assert_eq!(expected_document_version(state, 0), 1);
        assert!(replay_edges(state, 0).is_empty());

        state.restored[0] = 1;
        let restored = replay_edges(state, 0);
        assert_eq!(restored.len(), 3);
        assert!(restored.iter().all(|(key, _)| key.source.sequence == 21));
        assert!(restored.iter().any(|(key, properties)| {
            key.destination.collection == CollectionId(1)
                && key.destination.sequence == 2
                && properties == &json!({"round":0,"slot":1})
        }));

        state.updated[1] = 14;
        state.deleted[1] = 1;
        assert_eq!(expected_document_version(state, 0), -1);
        assert!(replay_edges(state, 0).is_empty());
        assert_eq!(replay_edges(state, 13).len(), 2);
        assert_eq!(replay_edges(state, 19).len(), 2);

        state.reinserted[1] = 1;
        assert_eq!(sequence_through(state, 20, 0, 3), 22);
        assert_eq!(expected_document_version(state, 0), 2);
        assert!(replay_edges(state, 0).is_empty());

        state.restored[1] = 1;
        let restored_again = replay_edges(state, 0);
        assert_eq!(restored_again.len(), 3);
        assert!(restored_again
            .iter()
            .all(|(key, _)| key.source.sequence == 22));
        assert!(restored_again.iter().any(|(key, properties)| {
            key.destination.collection == CollectionId(1)
                && key.destination.sequence == 2
                && properties == &json!({"round":1,"slot":1})
        }));
    }
}
