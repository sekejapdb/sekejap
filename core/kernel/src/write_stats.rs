//! Diagnostic accounting for bytes issued by the load write path.
//!
//! Counters are dormant until [`reset`] is called.  The benchmark does that
//! only for `--load-breakdown`, so headline runs do not pay an atomic update
//! at every page boundary.  These count bytes handed successfully to each
//! writer, not allocated file length (recycled candidate pages still count).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

#[derive(Clone, Copy, Debug, Default)]
pub struct WriteBytes {
    pub transaction_spool: u64,
    pub sort_scratch: u64,
    pub pack_scratch: u64,
    pub candidate_pages: u64,
    pub wal: u64,
    pub manifests: u64,
    pub final_pages: u64,
    pub sidecars: u64,
}

impl WriteBytes {
    pub fn total(self) -> u64 {
        self.transaction_spool
            .saturating_add(self.sort_scratch)
            .saturating_add(self.pack_scratch)
            .saturating_add(self.candidate_pages)
            .saturating_add(self.wal)
            .saturating_add(self.manifests)
            .saturating_add(self.final_pages)
            .saturating_add(self.sidecars)
    }
}

#[derive(Clone, Copy)]
pub enum Phase {
    TransactionSpool,
    SortScratch,
    PackScratch,
    CandidatePages,
    Wal,
    Manifest,
    FinalPages,
    Sidecar,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static TRANSACTION_SPOOL: AtomicU64 = AtomicU64::new(0);
static SORT_SCRATCH: AtomicU64 = AtomicU64::new(0);
static PACK_SCRATCH: AtomicU64 = AtomicU64::new(0);
static CANDIDATE_PAGES: AtomicU64 = AtomicU64::new(0);
static WAL: AtomicU64 = AtomicU64::new(0);
static MANIFESTS: AtomicU64 = AtomicU64::new(0);
static FINAL_PAGES: AtomicU64 = AtomicU64::new(0);
static SIDECARS: AtomicU64 = AtomicU64::new(0);

fn counter(phase: Phase) -> &'static AtomicU64 {
    match phase {
        Phase::TransactionSpool => &TRANSACTION_SPOOL,
        Phase::SortScratch => &SORT_SCRATCH,
        Phase::PackScratch => &PACK_SCRATCH,
        Phase::CandidatePages => &CANDIDATE_PAGES,
        Phase::Wal => &WAL,
        Phase::Manifest => &MANIFESTS,
        Phase::FinalPages => &FINAL_PAGES,
        Phase::Sidecar => &SIDECARS,
    }
}

pub fn add(phase: Phase, bytes: u64) {
    if ACTIVE.load(Relaxed) { counter(phase).fetch_add(bytes, Relaxed); }
}

pub fn reset() {
    for phase in [
        Phase::TransactionSpool,
        Phase::SortScratch,
        Phase::PackScratch,
        Phase::CandidatePages,
        Phase::Wal,
        Phase::Manifest,
        Phase::FinalPages,
        Phase::Sidecar,
    ] {
        counter(phase).store(0, Relaxed);
    }
    ACTIVE.store(true, Relaxed);
}

pub fn take() -> WriteBytes {
    ACTIVE.store(false, Relaxed);
    WriteBytes {
        transaction_spool: TRANSACTION_SPOOL.swap(0, Relaxed),
        sort_scratch: SORT_SCRATCH.swap(0, Relaxed),
        pack_scratch: PACK_SCRATCH.swap(0, Relaxed),
        candidate_pages: CANDIDATE_PAGES.swap(0, Relaxed),
        wal: WAL.swap(0, Relaxed),
        manifests: MANIFESTS.swap(0, Relaxed),
        final_pages: FINAL_PAGES.swap(0, Relaxed),
        sidecars: SIDECARS.swap(0, Relaxed),
    }
}
