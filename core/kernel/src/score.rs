//! 2l: hybrid scoring -- the kernel primitive SGQL's ScoreExpr lowers onto.
//!
//! A newcomer's map. Retrieval FINDS candidates (any family: a text
//! search, a vector walk, a spatial radius, a graph traversal). Scoring
//! RANKS them: an arithmetic expression over per-candidate atoms --
//! BM25 relevance, vector similarity, geodesic distance -- evaluated in
//! one pass. The structural rule (the contract's): scoring never re-runs
//! retrieval. Candidates arrive as ids; every atom is a point evaluation
//! against rows those ids name; cost is candidates x atoms, resident
//! state is one f32 column per distinct atom, freed at return.
//!
//! Atom semantics (documented, not configurable):
//! - Bm25: Okapi BM25 (K1=1.2, B=0.75), same math as text_search; a
//!   candidate without the term(s) scores 0.0.
//! - Bm25Norm: s/(s+k) saturation of the Bm25 score, bounded [0,1] so it
//!   blends with cosine on equal footing (e1's BM25_NORM).
//! - VecSim: HIGHER IS BETTER for every metric -- cosine and dot as-is,
//!   L2/L1 negated. A candidate without a vector IN THAT FIELD scores 0.0.
//! - StDistanceM: PostGIS-geography metres (Vincenty). A candidate
//!   without geometry is INFINITELY far (it sorts last under any
//!   positive distance weighting) -- unknown location never ranks near.
//! - Extern(i): the caller's per-candidate column (payload-derived
//!   values computed above the kernel; the payload stays opaque here).
//! - Division by zero yields 0.0 -- a poisoned NaN would make the final
//!   ordering depend on sort internals instead of the expression.

use crate::graph::{Graph, Metric};
use crate::Result;

/// The expression tree. Leaves are atoms or constants; interior nodes
/// are arithmetic. Built once per query, evaluated per candidate.
#[derive(Clone, Debug)]
pub enum ScoreExpr {
    Const(f32),
    Add(Box<ScoreExpr>, Box<ScoreExpr>),
    Sub(Box<ScoreExpr>, Box<ScoreExpr>),
    Mul(Box<ScoreExpr>, Box<ScoreExpr>),
    Div(Box<ScoreExpr>, Box<ScoreExpr>),
    Bm25 { field: u64, query: String },
    Bm25Norm { field: u64, query: String, k: f32 },
    VecSim { field: u64, metric: Metric, query: Vec<f32> },
    StDistanceM { field: u64, lat: f64, lon: f64 },
    Extern(usize),
}

/// One resolved atom: what to batch-evaluate. Two Bm25 atoms over the
/// same (field, query) resolve to ONE column (the tree may reference a
/// score twice, e.g. raw and saturated; the postings are read once).
enum Atom {
    Bm25 { field: u64, query: String },
    VecSim { field: u64, metric: Metric, query: Vec<f32> },
    StDistanceM { field: u64, lat: f64, lon: f64 },
    Extern(usize),
}

impl Atom {
    fn key(&self) -> String {
        match self {
            Atom::Bm25 { field, query } => format!("b:{field}:{query}"),
            Atom::VecSim { field, metric, query } => {
                let h: u64 = query.iter().fold(0u64, |a, x| {
                    a.wrapping_mul(1099511628211).wrapping_add(x.to_bits() as u64)
                });
                format!("v:{field}:{:?}:{h}", metric)
            }
            Atom::StDistanceM { field, lat, lon } => format!("g:{field}:{lat}:{lon}"),
            Atom::Extern(i) => format!("x:{i}"),
        }
    }
}

fn collect_atoms(e: &ScoreExpr, out: &mut Vec<Atom>) {
    match e {
        ScoreExpr::Const(_) => {}
        ScoreExpr::Add(a, b) | ScoreExpr::Sub(a, b)
        | ScoreExpr::Mul(a, b) | ScoreExpr::Div(a, b) => {
            collect_atoms(a, out);
            collect_atoms(b, out);
        }
        ScoreExpr::Bm25 { field, query } | ScoreExpr::Bm25Norm { field, query, .. } => {
            out.push(Atom::Bm25 { field: *field, query: query.clone() })
        }
        ScoreExpr::VecSim { field, metric, query } => {
            out.push(Atom::VecSim { field: *field, metric: *metric, query: query.clone() })
        }
        ScoreExpr::StDistanceM { field, lat, lon } => {
            out.push(Atom::StDistanceM { field: *field, lat: *lat, lon: *lon })
        }
        ScoreExpr::Extern(i) => out.push(Atom::Extern(*i)),
    }
}

impl Graph {
    /// Rank `cands` by `expr`, descending, top `k`. Ties break on id
    /// ascending (deterministic under snapshots). `externs` supplies the
    /// Extern(i) columns; each must be cands.len() long.
    pub fn hybrid_score(&self, cands: &[u64], expr: &ScoreExpr, k: usize,
                        externs: &[&[f32]]) -> Result<Vec<(u64, f32)>> {
        // 1) resolve distinct atoms
        let mut atoms: Vec<Atom> = Vec::new();
        collect_atoms(expr, &mut atoms);
        let mut cols: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        // 2) one batch pass per distinct atom -> a column aligned to cands
        for a in &atoms {
            let key = a.key();
            if cols.contains_key(&key) { continue; }
            let col = match a {
                Atom::Bm25 { field, query } => self.bm25_batch(*field, query, cands)?,
                Atom::VecSim { field, metric, query } => {
                    let mut c = Vec::with_capacity(cands.len());
                    for &id in cands {
                        c.push(match self.get_vec(*field, id)? {
                            Some(v) => similarity(*metric, &v, query),
                            None => 0.0,
                        });
                    }
                    c
                }
                Atom::StDistanceM { field, lat, lon } => {
                    let mut c = Vec::with_capacity(cands.len());
                    for &id in cands {
                        c.push(match self.st_distance(*field, id, *lat, *lon)? {
                            Some(m) => m as f32,
                            None => f32::INFINITY,
                        });
                    }
                    c
                }
                Atom::Extern(i) => {
                    let col = externs.get(*i).copied()
                        .expect("Extern(i) references a column the caller did not pass");
                    assert_eq!(col.len(), cands.len(),
                               "extern column length must equal candidate count");
                    col.to_vec()
                }
            };
            cols.insert(key, col);
        }
        // 3) fold the tree per candidate
        let mut out: Vec<(u64, f32)> = Vec::with_capacity(cands.len());
        for (i, &id) in cands.iter().enumerate() {
            out.push((id, eval(expr, i, &cols)));
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out.truncate(k);
        Ok(out)
    }

    /// BM25 for `cands` only. Live field/term statistics and each candidate's
    /// compact norm row are point-read; posting extents are retrieval data and
    /// are never reopened at the scoring boundary required by D33.
    fn bm25_batch(&self, field: u64, query: &str, cands: &[u64]) -> Result<Vec<f32>> {
        self.text_score_candidates(field, query, cands)
    }
}

fn similarity(metric: Metric, v: &[f32], q: &[f32]) -> f32 {
    let dot: f32 = v.iter().zip(q).map(|(a, b)| a * b).sum();
    match metric {
        Metric::Dot => dot,
        Metric::Cosine => {
            let nv: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nq: f32 = q.iter().map(|x| x * x).sum::<f32>().sqrt();
            if nv > 0.0 && nq > 0.0 { dot / (nv * nq) } else { 0.0 }
        }
        Metric::L2 => -v.iter().zip(q).map(|(a, b)| (a - b) * (a - b)).sum::<f32>(),
        Metric::L1 => -v.iter().zip(q).map(|(a, b)| (a - b).abs()).sum::<f32>(),
    }
}

fn eval(e: &ScoreExpr, i: usize, cols: &std::collections::HashMap<String, Vec<f32>>) -> f32 {
    match e {
        ScoreExpr::Const(c) => *c,
        ScoreExpr::Add(a, b) => eval(a, i, cols) + eval(b, i, cols),
        ScoreExpr::Sub(a, b) => eval(a, i, cols) - eval(b, i, cols),
        ScoreExpr::Mul(a, b) => eval(a, i, cols) * eval(b, i, cols),
        ScoreExpr::Div(a, b) => {
            let d = eval(b, i, cols);
            if d == 0.0 { 0.0 } else { eval(a, i, cols) / d }
        }
        ScoreExpr::Bm25 { field, query } => {
            cols[&Atom::Bm25 { field: *field, query: query.clone() }.key()][i]
        }
        ScoreExpr::Bm25Norm { field, query, k } => {
            let s = cols[&Atom::Bm25 { field: *field, query: query.clone() }.key()][i];
            if s > 0.0 { s / (s + k.max(f32::MIN_POSITIVE)) } else { 0.0 }
        }
        ScoreExpr::VecSim { field, metric, query } => {
            cols[&Atom::VecSim { field: *field, metric: *metric, query: query.clone() }.key()][i]
        }
        ScoreExpr::StDistanceM { field, lat, lon } => {
            cols[&Atom::StDistanceM { field: *field, lat: *lat, lon: *lon }.key()][i]
        }
        ScoreExpr::Extern(i_ext) => cols[&Atom::Extern(*i_ext).key()][i],
    }
}
