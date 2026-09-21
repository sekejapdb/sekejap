//! 2l: hybrid scoring against an INDEPENDENT oracle.
//!
//! The oracle recomputes BM25 and cosine with its own arithmetic from
//! its own copy of the corpus -- it never calls the engine's scoring
//! kernels (the 2g lesson: an oracle that calls the code it checks
//! lets mutations survive). Geodesic metres are the one shared value
//! (Vincenty is already pinned to live PostGIS fixtures in geomath).

use kernel::graph::{Graph, Metric};
use kernel::score::ScoreExpr;
use kernel::spatial::Geom;
use kernel::store::{Config, Store};
use kernel::text::tokenize;

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

const TITLE: u64 = 1;
const BODY: u64 = 2;
const GEO: u64 = 1;

struct Doc {
    id: u64,
    title: &'static str,
    body: &'static str,
    vec: [f32; 4],
    lonlat: Option<(f64, f64)>,
}

fn corpus() -> Vec<Doc> {
    vec![
        Doc { id: 1, title: "railway survey ledger", body: "the northern railway survey recorded fourteen bridges", vec: [1.0, 0.2, 0.0, 0.1], lonlat: Some((144.96, -37.81)) },
        Doc { id: 2, title: "harbour survey", body: "soundings of the harbour channel were taken at low tide", vec: [0.9, 0.3, 0.1, 0.0], lonlat: Some((151.21, -33.87)) },
        Doc { id: 3, title: "botanical catalogue", body: "pressed specimens of alpine flora with survey notes", vec: [0.0, 1.0, 0.4, 0.0], lonlat: Some((147.32, -42.88)) },
        Doc { id: 4, title: "railway timetable", body: "departures from the terminus every quarter hour", vec: [0.8, 0.1, 0.3, 0.4], lonlat: None },
        Doc { id: 5, title: "observatory log", body: "the transit instrument required realignment after the storm", vec: [0.1, 0.9, 0.9, 0.2], lonlat: Some((138.60, -34.93)) },
        Doc { id: 6, title: "survey of the goldfields railway", body: "gradients and cuttings along the goldfields railway survey", vec: [0.95, 0.25, 0.05, 0.05], lonlat: Some((143.85, -37.56)) },
    ]
}

fn build() -> (tempfile::TempDir, Graph) {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
    for doc in corpus() {
        g.index_text(TITLE, doc.id, doc.title).unwrap();
        g.index_text(BODY, doc.id, doc.body).unwrap();
        g.set_vec(VF, doc.id, &doc.vec).unwrap();
        if let Some((lon, lat)) = doc.lonlat {
            g.set_geo(GEO, doc.id, &Geom::Point(lon, lat)).unwrap();
        }
    }
    g.commit().unwrap();
    (d, g)
}

/// Oracle BM25: own math, own stats, over the oracle's own corpus copy.
fn oracle_bm25(field: u64, query: &str, docid: u64) -> f64 {
    let docs = corpus();
    let text = |d: &Doc| if field == TITLE { d.title } else { d.body };
    let toks: Vec<Vec<String>> = docs.iter().map(|d| tokenize(text(d))).collect();
    let n = docs.len() as f64;
    let avg = toks.iter().map(|t| t.len() as f64).sum::<f64>() / n;
    let (k1, b) = (1.2f64, 0.75f64);
    let mut score = 0.0;
    let mut seen = std::collections::HashSet::new();
    for term in tokenize(query) {
        if !seen.insert(term.clone()) { continue; }
        let df = toks.iter().filter(|t| t.contains(&term)).count() as f64;
        if df == 0.0 { continue; }
        let i = docs.iter().position(|d| d.id == docid).unwrap();
        let tf = toks[i].iter().filter(|t| **t == term).count() as f64;
        if tf == 0.0 { continue; }
        let dl = toks[i].len() as f64;
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        score += idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avg));
    }
    score
}

fn oracle_cosine(docid: u64, q: &[f32]) -> f64 {
    let d = corpus().into_iter().find(|d| d.id == docid).unwrap();
    let dot: f64 = d.vec.iter().zip(q).map(|(a, b)| *a as f64 * *b as f64).sum();
    let nv = d.vec.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nq = q.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if nv > 0.0 && nq > 0.0 { dot / (nv * nq) } else { 0.0 }
}

#[test]
fn scores_match_an_independent_oracle() {
    let (_d, g) = build();
    let qvec = vec![1.0f32, 0.2, 0.0, 0.0];
    let cands: Vec<u64> = vec![1, 2, 3, 4, 5, 6];
    let expr = ScoreExpr::Add(
        Box::new(ScoreExpr::Mul(
            Box::new(ScoreExpr::Bm25 { field: TITLE, query: "railway survey".into() }),
            Box::new(ScoreExpr::Const(0.4)),
        )),
        Box::new(ScoreExpr::Add(
            Box::new(ScoreExpr::Mul(
                Box::new(ScoreExpr::Bm25Norm { field: BODY, query: "survey".into(), k: 1.0 }),
                Box::new(ScoreExpr::Const(0.3)),
            )),
            Box::new(ScoreExpr::Mul(
                Box::new(ScoreExpr::VecSim { field: VF, metric: Metric::Cosine, query: qvec.clone() }),
                Box::new(ScoreExpr::Const(0.3)),
            )),
        )),
    );
    let got = g.hybrid_score(&cands, &expr, 6, &[]).unwrap();
    for &(id, s) in &got {
        let bm_t = oracle_bm25(TITLE, "railway survey", id);
        let bm_b = oracle_bm25(BODY, "survey", id);
        let norm = if bm_b > 0.0 { bm_b / (bm_b + 1.0) } else { 0.0 };
        let want = 0.4 * bm_t + 0.3 * norm + 0.3 * oracle_cosine(id, &qvec);
        assert!((s as f64 - want).abs() < 1e-4,
                "doc {id}: engine {s} vs oracle {want}");
    }
    // and the ranking is the oracle's ranking
    let mut want: Vec<(u64, f64)> = cands.iter().map(|&id| {
        let bm_b = oracle_bm25(BODY, "survey", id);
        let norm = if bm_b > 0.0 { bm_b / (bm_b + 1.0) } else { 0.0 };
        (id, 0.4 * oracle_bm25(TITLE, "railway survey", id)
             + 0.3 * norm + 0.3 * oracle_cosine(id, &qvec))
    }).collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let got_ids: Vec<u64> = got.iter().map(|(id, _)| *id).collect();
    let want_ids: Vec<u64> = want.iter().map(|(id, _)| *id).collect();
    assert_eq!(got_ids, want_ids);
}

#[test]
fn scoring_survives_a_fold() {
    // segment postings and head rows must score identically
    let (_d, mut g) = build();
    let before = g.hybrid_score(&[1, 2, 3, 4, 5, 6],
        &ScoreExpr::Bm25 { field: TITLE, query: "railway survey".into() }, 6, &[]).unwrap();
    g.fold_text(TITLE).unwrap();
    g.fold_text(BODY).unwrap();
    let after = g.hybrid_score(&[1, 2, 3, 4, 5, 6],
        &ScoreExpr::Bm25 { field: TITLE, query: "railway survey".into() }, 6, &[]).unwrap();
    assert_eq!(before, after, "fold must not move a BM25 score");
}

#[test]
fn missing_rows_are_neutral_or_infinitely_far() {
    let (_d, g) = build();
    // doc 4 has no geometry: under -distance it must sort LAST
    let expr = ScoreExpr::Sub(
        Box::new(ScoreExpr::Const(0.0)),
        Box::new(ScoreExpr::StDistanceM { field: GEO, lat: -37.81, lon: 144.96 }),
    );
    let got = g.hybrid_score(&[1, 2, 4, 5], &expr, 4, &[]).unwrap();
    assert_eq!(got.last().unwrap().0, 4, "unknown location must never rank near");
    assert_eq!(got[0].0, 1, "doc at the query point must rank first");
    // a candidate id with no rows at all scores 0.0 under text+vector
    let expr = ScoreExpr::Add(
        Box::new(ScoreExpr::Bm25 { field: TITLE, query: "railway".into() }),
        Box::new(ScoreExpr::VecSim { field: VF, metric: Metric::Cosine, query: vec![1.0, 0.0, 0.0, 0.0] }),
    );
    let got = g.hybrid_score(&[999], &expr, 1, &[]).unwrap();
    assert_eq!(got, vec![(999, 0.0)]);
}

#[test]
fn extern_columns_join_the_expression() {
    let (_d, g) = build();
    let pop: Vec<f32> = vec![0.1, 0.9, 0.5];
    let expr = ScoreExpr::Add(
        Box::new(ScoreExpr::Mul(
            Box::new(ScoreExpr::Bm25 { field: TITLE, query: "railway".into() }),
            Box::new(ScoreExpr::Const(0.5)),
        )),
        Box::new(ScoreExpr::Extern(0)),
    );
    let got = g.hybrid_score(&[3, 5, 2], &expr, 3, &[&pop]).unwrap();
    // none of these docs match "railway" in title: ranking is the extern column
    assert_eq!(got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![5, 2, 3]);
}

#[test]
fn snapshot_scores_are_immovable_during_churn() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
    for doc in corpus() {
        g.index_text(TITLE, doc.id, doc.title).unwrap();
        g.set_vec(VF, doc.id, &doc.vec).unwrap();
    }
    g.commit().unwrap();
    g.checkpoint().unwrap();
    let reader = Graph::new(Store::open_snapshot(d.path(), Config::default()).unwrap()).unwrap();
    let expr = ScoreExpr::Add(
        Box::new(ScoreExpr::Bm25 { field: TITLE, query: "railway survey".into() }),
        Box::new(ScoreExpr::VecSim { field: VF, metric: Metric::Cosine, query: vec![1.0, 0.2, 0.0, 0.0] }),
    );
    let before = reader.hybrid_score(&[1, 2, 3, 4, 5, 6], &expr, 6, &[]).unwrap();
    // churn: new docs shift df, avg_len, and vectors
    for i in 100..160u64 {
        g.index_text(TITLE, i, "railway railway survey survey railway").unwrap();
        g.set_vec(VF, i, &[0.5, 0.5, 0.5, 0.5]).unwrap();
    }
    g.commit().unwrap();
    g.checkpoint().unwrap();
    let after = reader.hybrid_score(&[1, 2, 3, 4, 5, 6], &expr, 6, &[]).unwrap();
    assert_eq!(before, after, "a pinned reader's scores must be byte-stable");
}

#[test]
fn weights_change_the_ranking() {
    // guards the weight plumbing: text-heavy vs vector-heavy must disagree
    let (_d, g) = build();
    let qvec = vec![0.0f32, 1.0, 0.8, 0.0]; // near doc 5, unlike "railway" docs
    let mk = |wt: f32, wv: f32| ScoreExpr::Add(
        Box::new(ScoreExpr::Mul(
            Box::new(ScoreExpr::Bm25 { field: TITLE, query: "railway".into() }),
            Box::new(ScoreExpr::Const(wt)),
        )),
        Box::new(ScoreExpr::Mul(
            Box::new(ScoreExpr::VecSim { field: VF, metric: Metric::Cosine, query: qvec.clone() }),
            Box::new(ScoreExpr::Const(wv)),
        )),
    );
    let text_heavy = g.hybrid_score(&[1, 4, 5, 6], &mk(1.0, 0.01), 1, &[]).unwrap();
    let vec_heavy = g.hybrid_score(&[1, 4, 5, 6], &mk(0.01, 1.0), 1, &[]).unwrap();
    assert_ne!(text_heavy[0].0, vec_heavy[0].0);
    assert_eq!(vec_heavy[0].0, 5);
}
