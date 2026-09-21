//! Source-preserving salvage. Only verified parent links establish membership.
//! Unreachable leaves are exported as raw evidence, never merged into current rows.
use super::{copy_overflow, decode_leaf_record, quarantine_damaged_wal, RecoveryReport};
use crate::{
    budget::MemoryBudget,
    bulk::{pack_tree, ExternalSort},
    io::{open_file, open_recovery_source, Barrier, FileIo, IoMode},
    meta::Meta,
    page::{PageKind, PageRef, PAGE_SIZE},
    pool::BufferPool,
    store::{Config, Store, SyncMode},
    Error, Result,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryClass {
    /// Surviving rows were reached through verified published links and WAL.
    /// This does not assert completeness: consult losses and unknown extents.
    VerifiedSubset,
    /// Meta/WAL evidence is missing: even membership in the latest state is uncertain.
    MembershipUncertain,
}

#[derive(Debug)]
pub struct SafeRecoveryReport {
    pub version: u32,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub database: PathBuf,
    pub class: RecoveryClass,
    pub entries_recovered: u64,
    pub entries_before_wal: u64,
    pub known_value_losses: u64,
    pub unknown_extents: u64,
    pub pages_read: u64,
    pub raw_candidate_records: u64,
    pub loss_journal: PathBuf,
    /// Framed raw cells; overflow cells retain SOURCE markers, not literal values.
    pub candidate_archive: Option<PathBuf>,
    pub wal: RecoveryReport,
}

fn bad(no: u32, why: &'static str) -> Error {
    Error::Corrupt { page_no: no, why }
}
fn fresh(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
}
fn hex(w: &mut impl Write, bytes: &[u8]) -> Result<()> {
    for b in bytes {
        write!(w, "{b:02x}")?;
    }
    Ok(())
}

struct Scan<'a> {
    file: &'a dyn FileIo,
    pages: u64,
    sort: ExternalSort,
    journal: BufWriter<File>,
    rep: SafeRecoveryReport,
    need_candidates: bool,
    max_lsn: u64,
}
impl Scan<'_> {
    fn loss(
        &mut self,
        no: u32,
        key: Option<&[u8]>,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        why: &'static str,
    ) -> Result<()> {
        if key.is_some() {
            self.rep.known_value_losses += 1;
        } else {
            self.rep.unknown_extents += 1;
        }
        write!(
            self.journal,
            "{}\t{no}\t",
            if key.is_some() { "key" } else { "extent" }
        )?;
        if let Some(k) = key {
            hex(&mut self.journal, k)?;
        }
        write!(self.journal, "\t")?;
        if let Some(k) = lower {
            hex(&mut self.journal, k)?;
        }
        write!(self.journal, "\t")?;
        if let Some(k) = upper {
            hex(&mut self.journal, k)?;
        }
        writeln!(self.journal, "\t{why}")?;
        Ok(())
    }

    // One 4KiB page per level, depth <= 64; no visited set proportional to file.
    // Strict parent bounds and leaf ordering make shared/cyclic children fail.
    fn walk(
        &mut self,
        no: u32,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        depth: usize,
    ) -> Result<()> {
        if depth >= 64 || no < 2 || no as u64 >= self.pages {
            self.need_candidates = true;
            return self.loss(no, None, lower, upper, "invalid child or depth");
        }
        let mut buf = [0u8; PAGE_SIZE];
        self.rep.pages_read += 1;
        // Actual I/O errors abort; corruption/truncation are named data losses.
        self.file.read_at(&mut buf, no as u64 * PAGE_SIZE as u64)?;
        let page = match PageRef::open(&buf, no) {
            Ok(p) if p.tree_id() == 1 => p,
            _ => {
                self.need_candidates = true;
                return self.loss(no, None, lower, upper, "page integrity or tree identity");
            }
        };
        self.max_lsn = self.max_lsn.max(page.lsn());
        match page.kind() {
            PageKind::Leaf => {
                // Validate the whole leaf before admitting any record. A malformed
                // cell/order makes its extent uncertain; do not partially trust it.
                let mut previous: Option<&[u8]> = None;
                for i in 0..page.nentries() {
                    let k = match decode_leaf_record(page.slot(i), no) {
                        Ok((k, _, _)) => k,
                        Err(_) => {
                            self.need_candidates = true;
                            return self.loss(no, None, lower, upper, "malformed leaf cell");
                        }
                    };
                    if previous.is_some_and(|p| k <= p)
                        || lower.is_some_and(|p| k < p)
                        || upper.is_some_and(|p| k >= p)
                    {
                        self.need_candidates = true;
                        return self.loss(no, None, lower, upper, "leaf ordering or parent bounds");
                    }
                    previous = Some(k);
                }
                for i in 0..page.nentries() {
                    let (k, v, overflow) = decode_leaf_record(page.slot(i), no)?;
                    if crate::keys::is_field_aggregate_key(k) {
                        continue;
                    }
                    if overflow {
                        match verify_overflow(self.file, v) {
                            Ok(()) => (),
                            Err(Error::Corrupt { why, .. }) => {
                                self.loss(no, Some(k), None, None, why)?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    self.sort.push_flagged(k.to_vec(), v.to_vec(), overflow)?;
                    self.rep.entries_before_wal += 1;
                }
            }
            PageKind::Interior => {
                // Decode every separator before following ANY child of this page.
                let mut separators = Vec::new();
                for i in 0..page.nentries() {
                    let (k, child) =
                        match crate::verify::decode_record(page.slot(i), no, PageKind::Interior) {
                            Ok(crate::verify::DecodedRecord::Interior { key, child }) => {
                                (key, child)
                            }
                            _ => {
                                self.need_candidates = true;
                                return self.loss(
                                    no,
                                    None,
                                    lower,
                                    upper,
                                    "malformed interior cell",
                                );
                            }
                        };
                    if separators
                        .last()
                        .is_some_and(|(p, _): &(&[u8], u32)| k <= *p)
                        || lower.is_some_and(|p| k < p)
                        || upper.is_some_and(|p| k >= p)
                    {
                        self.need_candidates = true;
                        return self.loss(
                            no,
                            None,
                            lower,
                            upper,
                            "interior ordering or parent bounds",
                        );
                    }
                    separators.push((k, child));
                }
                for i in 0..=separators.len() {
                    let child = if i == 0 {
                        page.child0()
                    } else {
                        separators[i - 1].1
                    };
                    let lo = if i == 0 {
                        lower
                    } else {
                        Some(separators[i - 1].0)
                    };
                    let hi = if i == separators.len() {
                        upper
                    } else {
                        Some(separators[i].0)
                    };
                    self.walk(child, lo, hi, depth + 1)?;
                }
            }
            _ => {
                self.need_candidates = true;
                self.loss(no, None, lower, upper, "unexpected tree page kind")?;
            }
        }
        Ok(())
    }

    fn archive_candidates(&mut self, path: &Path) -> Result<()> {
        let mut out = BufWriter::new(fresh(path)?);
        out.write_all(b"E4RAW001")?;
        let mut buf = [0u8; PAGE_SIZE];
        for no in 2..self.pages {
            self.rep.pages_read += 1;
            self.file.read_at(&mut buf, no * PAGE_SIZE as u64)?;
            let page = match PageRef::open(&buf, no as u32) {
                Ok(p) if p.kind() == PageKind::Leaf && p.tree_id() == 1 => p,
                _ => continue,
            };
            for i in 0..page.nentries() {
                let cell = page.slot(i);
                if decode_leaf_record(cell, no as u32).is_err() {
                    continue;
                }
                // The raw source cell and origin remain intact; no version winner
                // or currentness is inferred, and marker bytes are never values.
                let mut header = Vec::with_capacity(16);
                header.extend_from_slice(&(no as u32).to_le_bytes());
                header.extend_from_slice(&page.lsn().to_le_bytes());
                header.extend_from_slice(&(cell.len() as u32).to_le_bytes());
                let crc = crc32c::crc32c_append(crc32c::crc32c(&header), cell);
                out.write_all(&header)?;
                out.write_all(cell)?;
                out.write_all(&crc.to_le_bytes())?;
                self.rep.raw_candidate_records += 1;
            }
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        self.rep.candidate_archive = Some(path.to_path_buf());
        Ok(())
    }
}

// Read-only preflight prevents orphan destination allocations for damaged values.
// Whole-value CRC rejects individually valid pages crossed between chains.
fn verify_overflow(file: &dyn FileIo, marker: &[u8]) -> Result<()> {
    super::reader::visit_overflow(file, marker, |_| {})
}

/// Recover into a NEW directory, leaving all source files unchanged on every
/// result. `COMPLETE` is written last; failed attempts retain evidence and must
/// be retried with another destination. This API never publishes over source.
///
/// Costs: tree walk + two reads of each healthy overflow + fresh build/verification;
/// damage to ancestry adds a sequential raw-cell archive pass. Minimum budget
/// 8MiB. The report streams losses rather than keeping O(losses) collections.
pub fn recover_to(source: &Path, destination: &Path, cfg: Config) -> Result<SafeRecoveryReport> {
    recover_with_hook(source, destination, cfg, |_| Ok(()))
}

fn recover_with_hook(
    source: &Path,
    destination: &Path,
    mut cfg: Config,
    mut phase: impl FnMut(&'static str) -> Result<()>,
) -> Result<SafeRecoveryReport> {
    if cfg.budget_bytes < 8 << 20 {
        return Err(Error::OutOfBudget);
    }
    cfg.sync = SyncMode::Full;
    cfg.io = IoMode::Buffered;
    let source = fs::canonicalize(source)?;
    let parent = fs::canonicalize(destination.parent().ok_or(Error::TooLarge)?)?;
    let destination = parent.join(destination.file_name().ok_or(Error::TooLarge)?);
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err(bad(0, "recovery destination aliases or contains source"));
    }
    // Do not let open_file_writer's create semantics create a missing source.
    if !fs::metadata(source.join("data"))?.is_file() {
        return Err(bad(0, "source data is not a file"));
    }
    let file = open_recovery_source(&source.join("data"))?;
    phase("locked")?;
    fs::create_dir(&destination)?; // exclusive; never delete or reuse an attempt
    crate::io::sync_directory(&parent)?;
    phase("destination-created")?;
    let database = destination.join("database");
    fs::create_dir(&database)?;
    let journal_path = destination.join("losses.tsv");
    let mut journal = BufWriter::new(fresh(&journal_path)?);
    writeln!(
        journal,
        "kind\tpage\tkey_hex\tlower_inclusive_hex\tupper_exclusive_hex\treason"
    )?;
    let rep = SafeRecoveryReport {
        version: 1,
        source: source.clone(),
        destination: destination.clone(),
        database: database.clone(),
        class: RecoveryClass::VerifiedSubset,
        entries_recovered: 0,
        entries_before_wal: 0,
        known_value_losses: 0,
        unknown_extents: 0,
        pages_read: 0,
        raw_candidate_records: 0,
        loss_journal: journal_path,
        candidate_archive: None,
        wal: RecoveryReport::default(),
    };
    let scratch = destination.join("scratch");
    let mut scan = Scan {
        file: &*file,
        pages: file.len()? / PAGE_SIZE as u64,
        sort: ExternalSort::new(&scratch, cfg.budget_bytes / 4)?,
        journal,
        rep,
        need_candidates: false,
        max_lsn: 0,
    };
    let mut chosen: Option<Meta> = None;
    let mut buf = [0u8; PAGE_SIZE];
    for no in 0..2u32 {
        scan.rep.pages_read += 1;
        let m = if no as u64 >= scan.pages {
            Err(bad(no, "missing meta"))
        } else {
            file.read_at(&mut buf, no as u64 * PAGE_SIZE as u64)?;
            PageRef::open(&buf, no).and_then(|p| {
                if p.kind() != PageKind::Meta || p.tree_id() != 0 {
                    return Err(bad(no, "meta identity"));
                }
                // Empty slot B is the valid unused initial slot, not lost state.
                if no == 1 && p.nentries() == 0 {
                    return Ok(None);
                }
                Meta::from_page(&p).map(Some)
            })
        };
        match m {
            Ok(Some(m)) => {
                if m.roots[1..].iter().any(|r| *r != 0) {
                    return Err(bad(no, "multiple trees require explicit salvage mapping"));
                }
                scan.max_lsn = scan.max_lsn.max(m.next_lsn);
                if chosen
                    .as_ref()
                    .is_none_or(|old| m.generation > old.generation)
                {
                    chosen = Some(m);
                }
            }
            Ok(None) => (),
            Err(_) => {
                scan.rep.class = RecoveryClass::MembershipUncertain;
                scan.need_candidates = true;
                scan.loss(no, None, None, None, "meta evidence missing")?;
            }
        }
    }
    if let Some(meta) = chosen {
        scan.walk(meta.roots[0], None, None, 0)?;
    } else {
        scan.need_candidates = true;
        scan.rep.class = RecoveryClass::MembershipUncertain;
    }
    if file.len()? % PAGE_SIZE as u64 != 0 {
        scan.need_candidates = true;
        scan.loss(scan.pages as u32, None, None, None, "truncated file tail")?;
    }
    if scan.need_candidates {
        scan.archive_candidates(&destination.join("candidates.raw"))?;
    }
    scan.journal.flush()?;
    scan.journal.get_ref().sync_all()?;

    phase("scanned")?;
    let mut runs = scan.sort.finish()?;
    let data = database.join("data");
    let (nf, _) = open_file(&data, cfg.io)?;
    let pool = BufferPool::new(
        nf.into(),
        Arc::new(MemoryBudget::new(cfg.budget_bytes)),
        cfg.budget_bytes / 2 / PAGE_SIZE,
    )?;
    drop(pool.allocate()?);
    drop(pool.allocate()?);
    Meta::init_slot_b(&pool)?;
    let iter = runs.iter()?.map(|item| {
        let (k, v, overflow) = item?;
        let v = if overflow {
            copy_overflow(&*file, &pool, &v)?
        } else {
            v
        };
        Ok((k, v, overflow))
    });
    let root = pack_tree(&pool, 1, iter, 0.9, &scratch)?;
    let mut roots = [0; crate::meta::MAX_TREES];
    roots[0] = root;
    let meta = Meta {
        format_version: crate::meta::FORMAT_VERSION,
        roots,
        generation: 0,
        next_lsn: scan.max_lsn.checked_add(1).ok_or(Error::TooLarge)?,
    };
    meta.write(&pool)?;
    Meta::mark_salvaged(&pool)?;
    pool.flush_all(Barrier::Full)?;
    drop(pool);
    crate::verify::verify_rebuild(&data, cfg.io, &meta, scan.rep.entries_before_wal)?;

    phase("build-verified")?;
    let wal_source = source.join("wal");
    match fs::metadata(&wal_source) {
        Ok(m) => {
            if !m.is_file() {
                return Err(bad(0, "source WAL is not a file"));
            }
            let wal_dest = database.join("wal");
            let want = crate::wal::hash_prefix(&wal_source, m.len())?;
            fs::copy(&wal_source, &wal_dest)?;
            File::open(&wal_dest)?.sync_all()?;
            if fs::metadata(&wal_dest)?.len() != m.len()
                || crate::wal::hash_prefix(&wal_dest, m.len())? != want
            {
                return Err(bad(0, "copied WAL failed verification"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    quarantine_damaged_wal(&database, cfg, &mut scan.rep.wal)?;
    if scan.rep.wal.wal_quarantined.is_some() {
        scan.rep.class = RecoveryClass::MembershipUncertain;
    }
    // Replay only into the new database. An independent reopen verifies values,
    // including overflow; counts after replay are reported separately.
    phase("wal-preserved")?;
    let mut store = Store::open(&database, cfg)?;
    store.checkpoint()?;
    drop(store);
    let store = Store::open(&database, cfg)?;
    scan.rep.entries_recovered =
        crate::verify::verify_published_tree(&data, cfg.io, store.published_root(), 1)?.0;
    drop(store);
    let (durable, _) = open_file(&data, cfg.io)?;
    durable.sync_dir()?;
    phase("replay-verified")?;
    let mut complete = fresh(&destination.join("COMPLETE"))?;
    writeln!(complete, "e4-recovery-v1\nclass={:?}\nrows={}\nknown_value_losses={}\nunknown_extents={}\nraw_candidates={}",
        scan.rep.class, scan.rep.entries_recovered, scan.rep.known_value_losses,
        scan.rep.unknown_extents, scan.rep.raw_candidate_records)?;
    complete.sync_all()?;
    let (marker, _) = open_file(&destination.join("COMPLETE"), cfg.io)?;
    marker.sync_dir()?;
    Ok(scan.rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_repair_is_safe_to_retry() {
        let cfg = Config {
            budget_bytes: 8 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        };
        for stop in [
            "locked",
            "destination-created",
            "scanned",
            "build-verified",
            "wal-preserved",
            "replay-verified",
        ] {
            let d = tempfile::tempdir().unwrap();
            let source = d.path().join("source");
            let mut s = Store::create(&source, cfg).unwrap();
            s.put(b"keep", &vec![9; 9000]).unwrap();
            s.commit().unwrap();
            s.checkpoint().unwrap();
            s.put(b"wal", b"also keep").unwrap();
            s.commit().unwrap();
            drop(s);
            let before: Vec<_> = ["data", "wal", "free"]
                .map(|f| fs::read(source.join(f)).ok())
                .into();
            let dest = d.path().join("interrupted");
            let result = recover_with_hook(&source, &dest, cfg, |phase| {
                if phase == stop {
                    Err(std::io::Error::other("injected repair interruption").into())
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "{stop}");
            assert!(!dest.join("COMPLETE").exists(), "{stop}");
            let after: Vec<_> = ["data", "wal", "free"]
                .map(|f| fs::read(source.join(f)).ok())
                .into();
            assert_eq!(before, after, "{stop}");
            if dest.exists() {
                assert!(recover_to(&source, &dest, cfg).is_err());
            }
            let report = recover_to(&source, &d.path().join("retry"), cfg).unwrap();
            assert_eq!(report.entries_recovered, 2, "{stop}");
            let s = Store::open(&report.database, cfg).unwrap();
            assert_eq!(s.get(b"keep").unwrap(), Some(vec![9; 9000]));
            assert_eq!(s.get(b"wal").unwrap(), Some(b"also keep".to_vec()));
        }
    }
}
