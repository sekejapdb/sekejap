//! Read-only access to the current committed raw key/value tree.
//!
//! This is a recovery primitive, not an ordinary snapshot.  It takes the
//! existing writer lock exclusively and opens the data and WAL read-only for
//! its whole lifetime.  Membership is established only by walking from the
//! committed root; unreachable leaf pages are never considered current.

use super::*;
use kernel::{
    recover::{CandidateReader, LeafCandidate},
    verify::{decode_record, DecodedRecord},
};
use std::{ffi::OsString, fs, io::Read};

const TREE_ID: u16 = 1;
// FORMAT COUPLING: PAGE_SIZE - kernel btree's OV_DATA (page header + next
// pointer + used length). Update this precharge if that persisted overflow
// layout changes. CandidateReader remains the authority that validates every
// overflow page and byte.
const OVERFLOW_CAP: usize = PAGE - 46;

/// Hard limits for one current-state reader.
///
/// `max_work` is charged per tree page, decoded cell, and overflow page for
/// each `get` or `visit_range` call.  Fingerprinting has separate byte/file
/// ceilings because it deliberately reads the complete source inventory.
#[derive(Clone, Copy, Debug)]
pub struct CurrentReaderLimits {
    pub max_value_bytes: usize,
    pub max_work: u64,
    pub max_depth: usize,
    pub max_source_bytes: u64,
    pub max_source_files: usize,
}

impl Default for CurrentReaderLimits {
    fn default() -> Self {
        Self {
            max_value_bytes: 16 << 20,
            max_work: 4_000_000,
            max_depth: 64,
            max_source_bytes: 1 << 40,
            max_source_files: 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileFingerprint {
    name: OsString,
    bytes: u64,
    crc32c: u32,
    identity: (u64, u64),
}

/// A bounded fingerprint of the flat source directory.
///
/// Equality includes sorted names, lengths, byte checksums and, on Unix,
/// device/inode identity.  The detailed list stays private so callers cannot
/// accidentally treat a checksum as an independently authenticated format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub file_count: usize,
    pub total_bytes: u64,
    files: Vec<FileFingerprint>,
}

/// A root-driven, read-only view of the latest committed transaction.
pub struct CurrentSourceReader {
    dir: PathBuf,
    source: repair::Source,
    values: CandidateReader,
    root: u32,
    compact_cells: bool,
    limits: CurrentReaderLimits,
    fingerprint: SourceFingerprint,
    _writer_lock: File,
}

#[derive(Default)]
struct Work {
    used: u64,
}

impl Work {
    fn charge(&mut self, limits: CurrentReaderLimits, amount: u64) -> Result<()> {
        self.used = self
            .used
            .checked_add(amount)
            .ok_or(Error::ResourceLimit("current-reader work budget exceeded"))?;
        if self.used > limits.max_work {
            return Err(Error::ResourceLimit("current-reader work budget exceeded"));
        }
        Ok(())
    }
}

fn tree_bad(page_no: u32, why: &'static str) -> Error {
    Error::Corrupt { page_no, why }
}

fn validate_limits(limits: CurrentReaderLimits) -> Result<()> {
    if limits.max_value_bytes == 0 {
        return Err(Error::ResourceLimit("current-reader value budget is zero"));
    }
    if limits.max_work == 0 {
        return Err(Error::ResourceLimit("current-reader work budget is zero"));
    }
    if limits.max_depth == 0 || limits.max_depth > 64 {
        return Err(Error::ResourceLimit("current-reader depth must be 1..64"));
    }
    if limits.max_source_bytes == 0 || limits.max_source_files < 3 {
        return Err(Error::ResourceLimit(
            "current-reader fingerprint budget is too small",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn fingerprint_dir(dir: &Path, limits: CurrentReaderLimits) -> Result<SourceFingerprint> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        if entries.len() == limits.max_source_files {
            return Err(Error::ResourceLimit(
                "current-reader source file budget exceeded",
            ));
        }
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    let mut files = Vec::with_capacity(entries.len());
    let mut total_bytes = 0u64;
    for entry in entries {
        let name = entry.file_name();
        if name.to_str().is_none() {
            return Err(bad("current-reader source has a non-UTF-8 filename"));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file() {
            return Err(bad(
                "current-reader source directory is not flat regular files",
            ));
        }
        let bytes = metadata.len();
        total_bytes = total_bytes.checked_add(bytes).ok_or(Error::ResourceLimit(
            "current-reader source byte budget exceeded",
        ))?;
        if total_bytes > limits.max_source_bytes {
            return Err(Error::ResourceLimit(
                "current-reader source byte budget exceeded",
            ));
        }
        let mut file = File::open(entry.path())?;
        let mut crc = 0u32;
        let mut seen = 0u64;
        let mut buffer = [0u8; 64 << 10];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            seen = seen.checked_add(count as u64).ok_or(Error::ResourceLimit(
                "current-reader source byte budget exceeded",
            ))?;
            if seen > bytes {
                return Err(bad("current-reader source file grew during fingerprint"));
            }
            crc = crc32c::crc32c_append(crc, &buffer[..count]);
        }
        if seen != bytes {
            return Err(bad("current-reader source file changed during fingerprint"));
        }
        files.push(FileFingerprint {
            name,
            bytes,
            crc32c: crc,
            identity: file_identity(&metadata),
        });
    }
    Ok(SourceFingerprint {
        file_count: files.len(),
        total_bytes,
        files,
    })
}

fn committed_root(source: &repair::Source) -> Result<u32> {
    let header = if source.index.contains_key(&0) {
        let mut bytes = [0u8; PAGE];
        source.read_at(&mut bytes, 0)?;
        Header::decode(&bytes, 0)?
    } else {
        disk_header(&*source.data)?.ok_or_else(|| bad("current-reader metadata unavailable"))?
    };
    header.validate_extent(source.pages)?;
    Ok(header.root)
}

impl CurrentSourceReader {
    /// Opens an existing page-WAL database without creating, truncating,
    /// checkpointing, normalizing or publishing any source file.
    pub fn open(dir: &Path, limits: CurrentReaderLimits) -> Result<Self> {
        validate_limits(limits)?;
        let dir = fs::canonicalize(dir)?;
        let writer_lock = File::open(dir.join("writer.lock"))?;
        if !io::try_lock_exclusive(&writer_lock)? {
            return Err(Error::WriterLocked);
        }
        let data: Arc<dyn FileIo> = io::open_recovery_source(&dir.join("data"))?.into();
        let wal: Arc<dyn FileIo> = io::open_recovery_source(&dir.join("wal"))?.into();
        let fingerprint = fingerprint_dir(&dir, limits)?;
        let state = Pager::inspect(&*data, &*wal)?;
        let compact_cells = state.features & COMPACT_CELLS != 0;
        let source = repair::Source {
            data,
            wal,
            index: state.committed,
            pages: state.committed_pages,
        };
        let root = committed_root(&source)?;
        let values = CandidateReader::from_file(Box::new(source.clone()));
        Ok(Self {
            dir,
            source,
            values,
            root,
            compact_cells,
            limits,
            fingerprint,
            _writer_lock: writer_lock,
        })
    }

    pub fn fingerprint(&self) -> &SourceFingerprint {
        &self.fingerprint
    }

    /// Cell codec declared by the CRC-validated committed Page-WAL header.
    pub(crate) fn compact_cells(&self) -> bool {
        self.compact_cells
    }

    /// Re-hashes the source and refuses if its inventory, identities or bytes
    /// changed.  A streaming verifier should call this after its final visit
    /// and before publishing any separately built destination.
    pub fn recheck_source(&self) -> Result<()> {
        if fingerprint_dir(&self.dir, self.limits)? != self.fingerprint {
            return Err(bad("current-reader source changed"));
        }
        Ok(())
    }

    fn page(&self, no: u32, work: &mut Work) -> Result<[u8; PAGE]> {
        work.charge(self.limits, 1)?;
        if no < 2 || no >= self.source.pages {
            return Err(tree_bad(
                no,
                "current-reader child is outside committed extent",
            ));
        }
        let mut bytes = [0u8; PAGE];
        self.source.read_at(&mut bytes, no as u64 * PAGE as u64)?;
        PageRef::open(&bytes, no)?;
        Ok(bytes)
    }

    fn materialize(
        &self,
        page_no: u32,
        generation: u64,
        slot: usize,
        key: &[u8],
        stored_value: &[u8],
        overflow: bool,
        work: &mut Work,
    ) -> Result<Vec<u8>> {
        if overflow {
            if stored_value.len() != 12 {
                return Err(tree_bad(page_no, "current-reader overflow marker size"));
            }
            let total = u32::from_le_bytes(stored_value[..4].try_into().unwrap()) as usize;
            if total > self.limits.max_value_bytes {
                return Err(Error::ResourceLimit("current-reader value budget exceeded"));
            }
            // CandidateReader may inspect one page beyond the encoded bound
            // to prove that an overlong/cyclic chain is invalid.
            let pages = total
                .div_ceil(OVERFLOW_CAP)
                .max(1)
                .checked_add(1)
                .ok_or(Error::TooLarge)?;
            work.charge(
                self.limits,
                u64::try_from(pages).map_err(|_| Error::TooLarge)?,
            )?;
        } else if stored_value.len() > self.limits.max_value_bytes {
            return Err(Error::ResourceLimit("current-reader value budget exceeded"));
        }
        self.values.read_value(
            LeafCandidate {
                page_no,
                generation,
                slot,
                key,
                stored_value,
                overflow,
            },
            self.limits.max_value_bytes,
        )
    }

    /// Point lookup in the latest committed tree.  Every page on the routed
    /// root path is CRC/identity checked and all its cells are validated.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_in(TREE_ID, self.root, key)
    }

    /// The same point lookup in a tree the caller names -- a per-index tree,
    /// reached through the root its descriptor holds. `root == 0` is an empty
    /// tree. Identical page checks: every page must carry the tree id asked
    /// for, so a descriptor root that names a page of some OTHER tree (or a
    /// free page) is refused rather than read.
    pub fn get_in(&self, tree_id: u16, root: u32, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if root == 0 { return Ok(None); }
        let mut work = Work::default();
        let mut page_no = root;
        let mut lower: Option<Vec<u8>> = None;
        let mut upper: Option<Vec<u8>> = None;
        let mut path = Vec::with_capacity(self.limits.max_depth);
        loop {
            if path.contains(&page_no) {
                return Err(tree_bad(page_no, "current-reader tree cycle"));
            }
            if path.len() >= self.limits.max_depth {
                return Err(Error::ResourceLimit("current-reader depth budget exceeded"));
            }
            path.push(page_no);
            let bytes = self.page(page_no, &mut work)?;
            let page = PageRef::open(&bytes, page_no)?;
            if page.tree_id() != tree_id {
                return Err(tree_bad(
                    page_no,
                    "current-reader page belongs to another tree",
                ));
            }
            match page.kind() {
                PageKind::Leaf => {
                    let mut previous: Option<&[u8]> = None;
                    let mut found = None;
                    for slot in 0..page.nentries() {
                        work.charge(self.limits, 1)?;
                        let DecodedRecord::Leaf {
                            key: found_key,
                            value,
                            overflow,
                        } = decode_record(page.slot(slot), page_no, PageKind::Leaf)?
                        else {
                            return Err(tree_bad(page_no, "current-reader leaf record kind"));
                        };
                        if previous.is_some_and(|old| found_key <= old)
                            || lower
                                .as_ref()
                                .is_some_and(|bound| found_key < bound.as_slice())
                            || upper
                                .as_ref()
                                .is_some_and(|bound| found_key >= bound.as_slice())
                        {
                            return Err(tree_bad(page_no, "current-reader leaf key order/bounds"));
                        }
                        previous = Some(found_key);
                        if found_key == key {
                            found = Some((slot, found_key, value, overflow));
                        }
                    }
                    return found
                        .map(|(slot, found_key, value, overflow)| {
                            self.materialize(
                                page_no,
                                page.lsn(),
                                slot,
                                found_key,
                                value,
                                overflow,
                                &mut work,
                            )
                        })
                        .transpose();
                }
                PageKind::Interior => {
                    let mut separators = Vec::with_capacity(page.nentries());
                    let mut previous: Option<&[u8]> = None;
                    for slot in 0..page.nentries() {
                        work.charge(self.limits, 1)?;
                        let DecodedRecord::Interior {
                            key: separator,
                            child,
                        } = decode_record(page.slot(slot), page_no, PageKind::Interior)?
                        else {
                            return Err(tree_bad(page_no, "current-reader interior record kind"));
                        };
                        if previous.is_some_and(|old| separator <= old)
                            || lower
                                .as_ref()
                                .is_some_and(|bound| separator < bound.as_slice())
                            || upper
                                .as_ref()
                                .is_some_and(|bound| separator >= bound.as_slice())
                        {
                            return Err(tree_bad(page_no, "current-reader separator order/bounds"));
                        }
                        if child < 2 || child >= self.source.pages {
                            return Err(tree_bad(
                                page_no,
                                "current-reader child is outside committed extent",
                            ));
                        }
                        previous = Some(separator);
                        separators.push((separator.to_vec(), child));
                    }
                    if page.child0() < 2 || page.child0() >= self.source.pages {
                        return Err(tree_bad(
                            page_no,
                            "current-reader child0 is outside committed extent",
                        ));
                    }
                    let mut chosen = 0usize;
                    for (index, (separator, _)) in separators.iter().enumerate() {
                        if separator.as_slice() <= key {
                            chosen = index + 1;
                        } else {
                            break;
                        }
                    }
                    let next = if chosen == 0 {
                        page.child0()
                    } else {
                        separators[chosen - 1].1
                    };
                    let next_lower = if chosen == 0 {
                        lower.clone()
                    } else {
                        Some(separators[chosen - 1].0.clone())
                    };
                    let next_upper = if chosen == separators.len() {
                        upper.clone()
                    } else {
                        Some(separators[chosen].0.clone())
                    };
                    page_no = next;
                    lower = next_lower;
                    upper = next_upper;
                }
                _ => return Err(tree_bad(page_no, "current-reader reached a non-tree page")),
            }
        }
    }

    /// Streams every current row in `[start, end)`, in key order.  `None` is
    /// an unbounded upper end.  Budget exhaustion or damage returns an error;
    /// it is never reported as a shorter successful result.
    pub fn visit_range(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        visitor: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<u64> {
        self.visit_tree_range(TREE_ID, self.root, start, end, visitor)
    }

    /// The same streamed range over a tree the caller names.
    pub fn visit_tree_range(
        &self,
        tree_id: u16,
        root: u32,
        start: &[u8],
        end: Option<&[u8]>,
        mut visitor: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<u64> {
        if root == 0 { return Ok(0); }
        if end.is_some_and(|bound| bound < start) {
            return Err(Error::ResourceLimit(
                "current-reader range bounds are reversed",
            ));
        }
        if end == Some(start) {
            return Ok(0);
        }
        let mut work = Work::default();
        let mut path = Vec::with_capacity(self.limits.max_depth);
        let mut last = None;
        let mut count = 0u64;
        let mut state = VisitState {
            tree_id,
            start,
            end,
            visitor: &mut visitor,
            work: &mut work,
            path: &mut path,
            last: &mut last,
            count: &mut count,
        };
        self.visit_page(root, None, None, 1, &mut state)?;
        Ok(count)
    }

    pub fn visit_all(&self, visitor: impl FnMut(&[u8], &[u8]) -> Result<()>) -> Result<u64> {
        self.visit_range(&[], None, visitor)
    }

    fn visit_page(
        &self,
        page_no: u32,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        depth: usize,
        state: &mut VisitState<'_, impl FnMut(&[u8], &[u8]) -> Result<()>>,
    ) -> Result<()> {
        if state.path.contains(&page_no) {
            return Err(tree_bad(page_no, "current-reader tree cycle"));
        }
        if depth > self.limits.max_depth {
            return Err(Error::ResourceLimit("current-reader depth budget exceeded"));
        }
        state.path.push(page_no);
        let result = (|| {
            let bytes = self.page(page_no, state.work)?;
            let page = PageRef::open(&bytes, page_no)?;
            if page.tree_id() != state.tree_id {
                return Err(tree_bad(
                    page_no,
                    "current-reader page belongs to another tree",
                ));
            }
            match page.kind() {
                PageKind::Leaf => {
                    let mut previous: Option<&[u8]> = None;
                    for slot in 0..page.nentries() {
                        state.work.charge(self.limits, 1)?;
                        let DecodedRecord::Leaf {
                            key,
                            value,
                            overflow,
                        } = decode_record(page.slot(slot), page_no, PageKind::Leaf)?
                        else {
                            return Err(tree_bad(page_no, "current-reader leaf record kind"));
                        };
                        if previous.is_some_and(|old| key <= old)
                            || lower.is_some_and(|bound| key < bound)
                            || upper.is_some_and(|bound| key >= bound)
                        {
                            return Err(tree_bad(page_no, "current-reader leaf key order/bounds"));
                        }
                        previous = Some(key);
                        if key < state.start || state.end.is_some_and(|bound| key >= bound) {
                            continue;
                        }
                        if state.last.as_ref().is_some_and(|old| key <= old.as_slice()) {
                            return Err(tree_bad(page_no, "current-reader global key order"));
                        }
                        let materialized = self.materialize(
                            page_no,
                            page.lsn(),
                            slot,
                            key,
                            value,
                            overflow,
                            state.work,
                        )?;
                        (state.visitor)(key, &materialized)?;
                        *state.last = Some(key.to_vec());
                        *state.count = state.count.checked_add(1).ok_or(Error::TooLarge)?;
                    }
                    Ok(())
                }
                PageKind::Interior => {
                    let mut separators = Vec::with_capacity(page.nentries());
                    let mut previous: Option<&[u8]> = None;
                    for slot in 0..page.nentries() {
                        state.work.charge(self.limits, 1)?;
                        let DecodedRecord::Interior { key, child } =
                            decode_record(page.slot(slot), page_no, PageKind::Interior)?
                        else {
                            return Err(tree_bad(page_no, "current-reader interior record kind"));
                        };
                        if previous.is_some_and(|old| key <= old)
                            || lower.is_some_and(|bound| key < bound)
                            || upper.is_some_and(|bound| key >= bound)
                        {
                            return Err(tree_bad(page_no, "current-reader separator order/bounds"));
                        }
                        if child < 2 || child >= self.source.pages {
                            return Err(tree_bad(
                                page_no,
                                "current-reader child is outside committed extent",
                            ));
                        }
                        previous = Some(key);
                        separators.push((key.to_vec(), child));
                    }
                    let child0 = page.child0();
                    if child0 < 2 || child0 >= self.source.pages {
                        return Err(tree_bad(
                            page_no,
                            "current-reader child0 is outside committed extent",
                        ));
                    }
                    for index in 0..=separators.len() {
                        let child = if index == 0 {
                            child0
                        } else {
                            separators[index - 1].1
                        };
                        let child_lower = if index == 0 {
                            lower
                        } else {
                            Some(separators[index - 1].0.as_slice())
                        };
                        let child_upper = if index == separators.len() {
                            upper
                        } else {
                            Some(separators[index].0.as_slice())
                        };
                        if child_upper.is_some_and(|bound| bound <= state.start)
                            || state
                                .end
                                .is_some_and(|end| child_lower.is_some_and(|bound| bound >= end))
                        {
                            continue;
                        }
                        self.visit_page(child, child_lower, child_upper, depth + 1, state)?;
                    }
                    Ok(())
                }
                _ => Err(tree_bad(page_no, "current-reader reached a non-tree page")),
            }
        })();
        state.path.pop();
        result
    }
}

struct VisitState<'a, F> {
    tree_id: u16,
    start: &'a [u8],
    end: Option<&'a [u8]>,
    visitor: &'a mut F,
    work: &'a mut Work,
    path: &'a mut Vec<u32>,
    last: &'a mut Option<Vec<u8>>,
    count: &'a mut u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    fn limits() -> CurrentReaderLimits {
        CurrentReaderLimits {
            max_value_bytes: 1 << 20,
            max_work: 100_000,
            max_depth: 64,
            max_source_bytes: 32 << 20,
            max_source_files: 64,
        }
    }

    fn inventory(path: &Path) -> Vec<(OsString, Vec<u8>)> {
        let mut names = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let bytes = fs::read(path.join(&name)).unwrap();
                (name, bytes)
            })
            .collect()
    }

    #[test]
    fn checkpointed_source_is_streamed_without_source_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let mut store = PageWalStore::open(&path, true, 64 << 10).unwrap();
        store.put(b"a", b"one").unwrap();
        store.put(b"c", b"three").unwrap();
        store.commit().unwrap();
        store.checkpoint().unwrap();
        drop(store);
        let before = inventory(&path);

        let reader = CurrentSourceReader::open(&path, limits()).unwrap();
        assert_eq!(
            reader.get(b"a").unwrap().as_deref(),
            Some(b"one".as_slice())
        );
        assert_eq!(reader.get(b"b").unwrap(), None);
        let mut rows = Vec::new();
        assert_eq!(
            reader
                .visit_range(b"a", Some(b"d"), |key, value| {
                    rows.push((key.to_vec(), value.to_vec()));
                    Ok(())
                })
                .unwrap(),
            2
        );
        assert_eq!(
            rows,
            vec![
                (b"a".to_vec(), b"one".to_vec()),
                (b"c".to_vec(), b"three".to_vec())
            ]
        );
        reader.recheck_source().unwrap();
        assert_eq!(inventory(&path), before);
        drop(reader);
        assert_eq!(inventory(&path), before);
    }

    #[test]
    fn committed_wal_overlays_checkpoint_and_deleted_rows_stay_absent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let mut store = PageWalStore::open(&path, true, 64 << 10).unwrap();
        store.put(b"base", b"checkpoint").unwrap();
        store.put(b"deleted", b"historical").unwrap();
        store.commit().unwrap();
        store.checkpoint().unwrap();
        store.delete(b"deleted").unwrap();
        store.put(b"pending", b"committed wal").unwrap();
        store.commit().unwrap();
        drop(store);

        let reader = CurrentSourceReader::open(&path, limits()).unwrap();
        assert_eq!(
            reader.get(b"base").unwrap().as_deref(),
            Some(b"checkpoint".as_slice())
        );
        assert_eq!(
            reader.get(b"pending").unwrap().as_deref(),
            Some(b"committed wal".as_slice())
        );
        assert_eq!(reader.get(b"deleted").unwrap(), None);
        let mut keys = Vec::new();
        reader
            .visit_all(|key, _| {
                keys.push(key.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(keys, vec![b"base".to_vec(), b"pending".to_vec()]);
    }

    #[test]
    fn interior_tree_point_range_and_full_visit_match_independent_map() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let mut store = PageWalStore::open(&path, true, 1 << 20).unwrap();
        let mut expected = BTreeMap::new();
        for index in 0..1500u32 {
            let key = format!("k{index:04}").into_bytes();
            let value = if index == 1499 {
                vec![0x5a; 6000]
            } else {
                format!("value-{index:04}").into_bytes()
            };
            store.put(&key, &value).unwrap();
            expected.insert(key, value);
        }
        store.commit().unwrap();
        store.checkpoint().unwrap();
        let mut deleted = Vec::new();
        for index in (0..1500u32).step_by(7) {
            let key = format!("k{index:04}").into_bytes();
            store.delete(&key).unwrap();
            expected.remove(&key);
            deleted.push(key);
        }
        store
            .put(b"k0500", b"replacement in committed wal")
            .unwrap();
        expected.insert(b"k0500".to_vec(), b"replacement in committed wal".to_vec());
        store.commit().unwrap();
        drop(store);
        let before = inventory(&path);

        let reader = CurrentSourceReader::open(&path, limits()).unwrap();
        let mut work = Work::default();
        let root_bytes = reader.page(reader.root, &mut work).unwrap();
        let root = PageRef::open(&root_bytes, reader.root).unwrap();
        assert_eq!(root.kind(), PageKind::Interior);
        let mut height = 1usize;
        let mut page_no = root.child0();
        loop {
            let bytes = reader.page(page_no, &mut work).unwrap();
            let page = PageRef::open(&bytes, page_no).unwrap();
            height += 1;
            if page.kind() == PageKind::Leaf {
                break;
            }
            assert_eq!(page.kind(), PageKind::Interior);
            page_no = page.child0();
        }
        assert!(height >= 2, "fixture must exercise an interior path");

        for (key, value) in &expected {
            assert_eq!(reader.get(key).unwrap().as_deref(), Some(value.as_slice()));
        }
        for key in deleted.iter().take(16) {
            assert_eq!(reader.get(key).unwrap(), None);
        }

        let expected_all = expected
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let mut actual_all = Vec::new();
        assert_eq!(
            reader
                .visit_all(|key, value| {
                    actual_all.push((key.to_vec(), value.to_vec()));
                    Ok(())
                })
                .unwrap(),
            expected_all.len() as u64
        );
        assert_eq!(actual_all, expected_all);

        let expected_range = expected
            .iter()
            .filter(|(key, _)| key.as_slice() >= b"k0300" && key.as_slice() < b"k0750")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let mut actual_range = Vec::new();
        reader
            .visit_range(b"k0300", Some(b"k0750"), |key, value| {
                actual_range.push((key.to_vec(), value.to_vec()));
                Ok(())
            })
            .unwrap();
        assert_eq!(actual_range, expected_range);
        reader.recheck_source().unwrap();
        drop(reader);
        assert_eq!(inventory(&path), before);
    }

    #[test]
    fn active_writer_is_refused_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let store = PageWalStore::open(&path, true, 64 << 10).unwrap();
        let before = inventory(&path);
        assert!(matches!(
            CurrentSourceReader::open(&path, limits()),
            Err(Error::WriterLocked)
        ));
        assert_eq!(inventory(&path), before);
        drop(store);
    }

    #[test]
    fn corrupt_reachable_page_and_work_or_value_overflow_refuse() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("corrupt");
        let mut store = PageWalStore::open(&path, true, 64 << 10).unwrap();
        store.put(b"key", b"value").unwrap();
        store.commit().unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let data_path = path.join("data");
        let mut data = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&data_path)
            .unwrap();
        let mut meta = [0u8; PAGE];
        data.read_exact(&mut meta).unwrap();
        let root = Header::decode(&meta, 0).unwrap().root;
        data.seek(SeekFrom::Start(root as u64 * PAGE as u64 + 100))
            .unwrap();
        let mut byte = [0u8; 1];
        data.read_exact(&mut byte).unwrap();
        data.seek(SeekFrom::Current(-1)).unwrap();
        byte[0] ^= 0x80;
        data.write_all(&byte).unwrap();
        data.sync_all().unwrap();
        drop(data);
        let before = inventory(&path);
        let reader = CurrentSourceReader::open(&path, limits()).unwrap();
        assert!(matches!(
            reader.visit_all(|_, _| Ok(())),
            Err(Error::Corrupt { .. })
        ));
        drop(reader);
        assert_eq!(inventory(&path), before);

        let path = temp.path().join("budgets");
        let mut store = PageWalStore::open(&path, true, 64 << 10).unwrap();
        store.put(b"large", &vec![7; 5000]).unwrap();
        store.commit().unwrap();
        drop(store);
        let before = inventory(&path);
        let mut tiny = limits();
        tiny.max_value_bytes = 100;
        let reader = CurrentSourceReader::open(&path, tiny).unwrap();
        assert!(matches!(reader.get(b"large"), Err(Error::ResourceLimit(_))));
        drop(reader);
        assert_eq!(inventory(&path), before);
        let mut tiny = limits();
        tiny.max_work = 1;
        let reader = CurrentSourceReader::open(&path, tiny).unwrap();
        assert!(matches!(
            reader.visit_all(|_, _| Ok(())),
            Err(Error::ResourceLimit(_))
        ));
        drop(reader);
        assert_eq!(inventory(&path), before);
    }

    #[test]
    fn crc_valid_reachable_cycle_is_refused_without_source_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let mut store = PageWalStore::open(&path, true, 1 << 20).unwrap();
        for index in 0..600u32 {
            store
                .put(format!("k{index:04}").as_bytes(), b"value")
                .unwrap();
        }
        store.commit().unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let data_path = path.join("data");
        let mut data = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&data_path)
            .unwrap();
        let mut metadata = [0u8; PAGE];
        data.read_exact(&mut metadata).unwrap();
        let root = Header::decode(&metadata, 0).unwrap().root;
        let mut root_bytes = [0u8; PAGE];
        data.seek(SeekFrom::Start(root as u64 * PAGE as u64))
            .unwrap();
        data.read_exact(&mut root_bytes).unwrap();
        let root_page = PageRef::open(&root_bytes, root).unwrap();
        let generation = root_page.lsn();
        assert_eq!(root_page.kind(), PageKind::Interior);
        PageMut::reopen(&mut root_bytes).set_child0(root);
        kernel::page::seal(&mut root_bytes, generation);
        data.seek(SeekFrom::Start(root as u64 * PAGE as u64))
            .unwrap();
        data.write_all(&root_bytes).unwrap();
        data.sync_all().unwrap();
        drop(data);
        let before = inventory(&path);

        let reader = CurrentSourceReader::open(&path, limits()).unwrap();
        assert!(matches!(
            reader.visit_all(|_, _| Ok(())),
            Err(Error::Corrupt {
                why: "current-reader tree cycle",
                ..
            })
        ));
        drop(reader);
        assert_eq!(inventory(&path), before);
    }

    #[test]
    fn fingerprint_recheck_detects_inventory_change() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let store = PageWalStore::open(&path, true, 64 << 10).unwrap();
        drop(store);
        let reader = CurrentSourceReader::open(&path, limits()).unwrap();
        fs::write(path.join("unexpected"), b"external mutation").unwrap();
        assert!(reader.recheck_source().is_err());
    }
}
