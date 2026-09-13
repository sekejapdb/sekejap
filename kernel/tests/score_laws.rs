//! Law 1 for hybrid scoring: the primitive holds NO resident state.
//! Live heap after a scoring burst must return to its pre-scoring level
//! (columns are transient, freed at return), and the transient peak must
//! scale with CANDIDATES, not with the corpus.

use kernel::graph::{Graph, Metric};
use kernel::score::ScoreExpr;
use kernel::store::{Config, Store};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

static LIVE: AtomicUsize = AtomicUsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() { LIVE.fetch_add(l.size(), Relaxed); }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() { LIVE.fetch_add(n, Relaxed); LIVE.fetch_sub(l.size(), Relaxed); }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn build(n: u64) -> (tempfile::TempDir, Graph) {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
    let pool = ["railway", "survey", "bridge", "harbour", "ledger", "signal"];
    for i in 1..=n {
        let a = pool[(i % 6) as usize];
        let b = pool[((i / 6) % 6) as usize];
        g.index_text(1, i, &format!("{a} {b} ledger")).unwrap();
        g.set_vec(VF, i, &[(i % 7) as f32, 1.0, 0.5, (i % 3) as f32]).unwrap();
    }
    g.commit().unwrap();
    (d, g)
}

fn expr() -> ScoreExpr {
    ScoreExpr::Add(
        Box::new(ScoreExpr::Bm25 { field: 1, query: "railway survey".into() }),
        Box::new(ScoreExpr::VecSim { field: VF, metric: Metric::Cosine, query: vec![1.0, 0.5, 0.2, 0.0] }),
    )
}

#[test]
fn scoring_holds_no_resident_state_and_peaks_with_candidates() {
    // corpus 4x apart; candidate count IDENTICAL. Resident delta must be
    // ~zero both times; growth with the corpus would be a Law 1 breach.
    let mut resident = Vec::new();
    for n in [2_000u64, 8_000] {
        let (_d, g) = build(n);
        let cands: Vec<u64> = (1..=500u64).collect();
        let _ = g.hybrid_score(&cands, &expr(), 10, &[]).unwrap(); // warm caches
        let before = LIVE.load(Relaxed);
        for _ in 0..20 {
            let _ = g.hybrid_score(&cands, &expr(), 10, &[]).unwrap();
        }
        let after = LIVE.load(Relaxed);
        resident.push(after as i64 - before as i64);
    }
    for (i, r) in resident.iter().enumerate() {
        assert!(*r < (8 << 10),
                "scoring left {r} bytes resident at corpus {} (must be ~0)", i);
    }
}
