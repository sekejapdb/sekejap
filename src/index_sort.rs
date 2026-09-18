//! Bounded external sorter for late index builds.
//!
//! SQLite's CREATE INDEX sorts every (key, rowid) with sqlite3VdbeSorter*:
//! an in-memory run, spill to temp files at a byte budget, then a k-way merge
//! so the btree sees an ascending stream and can append. This is that shape
//! over `(key, value)` byte pairs.
//!
//! RAM is the budget, not the store (Law 1). Spill files are not
//! authoritative — the primary rows are — so a crash discards them and the
//! sort restarts (Law 3). Named sacrifice (Law 4): temp spill space is
//! proportional to the index being built, for the life of the sort.
use super::*;
use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
};

/// Default in-memory run: 8 MiB. Larger corpora spill; RAM stays flat.
pub(super) const DEFAULT_BUDGET: usize = 8 * 1024 * 1024;
const LEN_BYTES: usize = 4;
static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

fn sort_io(err: impl std::fmt::Display) -> Error {
    invalid(format!("index sort: {err}"))
}

fn record_bytes(key: &[u8], value: &[u8]) -> usize {
    LEN_BYTES + key.len() + LEN_BYTES + value.len()
}

fn write_u32<W: Write>(w: &mut W, n: usize) -> Result<()> {
    let n = u32::try_from(n).map_err(|_| invalid("index sort record exceeds 4 GiB"))?;
    w.write_all(&n.to_be_bytes()).map_err(sort_io)
}

fn read_u32<R: Read>(r: &mut R) -> Result<Option<u32>> {
    let mut buf = [0u8; 4];
    match r.read_exact(&mut buf) {
        Ok(()) => Ok(Some(u32::from_be_bytes(buf))),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(sort_io(e)),
    }
}

fn write_record<W: Write>(w: &mut W, key: &[u8], value: &[u8]) -> Result<()> {
    write_u32(w, key.len())?;
    w.write_all(key).map_err(sort_io)?;
    write_u32(w, value.len())?;
    w.write_all(value).map_err(sort_io)?;
    Ok(())
}

fn read_record<R: Read>(r: &mut R) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let Some(klen) = read_u32(r)? else {
        return Ok(None);
    };
    let mut key = vec![0; klen as usize];
    r.read_exact(&mut key).map_err(sort_io)?;
    let vlen = read_u32(r)?.ok_or_else(|| invalid("index sort truncated value length"))?;
    let mut value = vec![0; vlen as usize];
    r.read_exact(&mut value).map_err(sort_io)?;
    Ok(Some((key, value)))
}

/// One sorter. Push entries, then `finish` into a k-way merge.
pub(super) struct ExternalSorter {
    budget: usize,
    current: Vec<(Vec<u8>, Vec<u8>)>,
    current_bytes: usize,
    runs: Vec<PathBuf>,
    dir: Option<PathBuf>,
    spills: usize,
}

impl ExternalSorter {
    pub(super) fn new(scratch: &Path, budget: usize) -> Result<Self> {
        let budget = budget.max(1);
        let dir = scratch.join(format!(
            "e4-idxsort-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir_all(&dir).map_err(sort_io)?;
        Ok(Self {
            budget,
            current: Vec::new(),
            current_bytes: 0,
            runs: Vec::new(),
            dir: Some(dir),
            spills: 0,
        })
    }

    #[allow(dead_code)]
    pub(super) fn spills(&self) -> usize {
        self.spills
    }

    /// Push borrowed bytes. The sorter still owns a copy -- that is its
    /// storage -- but the caller keeps one scratch buffer instead of building
    /// a fresh `Vec` per entry.
    ///
    /// The scalar and spatial builds push through here: the copy below is the
    /// sorter's own storage and cannot be avoided, but the caller's key can be
    /// one reused buffer instead of a fresh `Vec` a row.
    pub(super) fn push_ref(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let add = record_bytes(key, value);
        if !self.current.is_empty() && self.current_bytes.saturating_add(add) > self.budget {
            self.spill()?;
        }
        self.current_bytes = self.current_bytes.saturating_add(add);
        self.current.push((key.to_vec(), value.to_vec()));
        Ok(())
    }

    pub(super) fn push(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        let add = record_bytes(&key, &value);
        if !self.current.is_empty() && self.current_bytes.saturating_add(add) > self.budget {
            self.spill()?;
        }
        self.current_bytes = self.current_bytes.saturating_add(add);
        self.current.push((key, value));
        Ok(())
    }

    fn spill(&mut self) -> Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        self.current
            .sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let dir = self
            .dir
            .as_ref()
            .ok_or_else(|| invalid("index sort directory gone"))?;
        let path = dir.join(format!("run-{:06}.dat", self.runs.len()));
        let count = self.current.len();
        {
            let file = File::create(&path).map_err(sort_io)?;
            let mut w = BufWriter::new(file);
            for (k, v) in &self.current {
                write_record(&mut w, k, v)?;
            }
            w.flush().map_err(sort_io)?;
            // No fsync: a spill is scratch a crash discards. Law 3 requires
            // write-verify-then-use of *authoritative* data; the re-read below
            // is the verify. F_FULLFSYNC on these files was the 50K text tax.
        }
        // Independently re-read the run before it participates in the merge.
        {
            let file = File::open(&path).map_err(sort_io)?;
            let mut r = BufReader::new(file);
            let mut seen = 0usize;
            while read_record(&mut r)?.is_some() {
                seen += 1;
            }
            if seen != count {
                return Err(invalid(format!(
                    "index sort spill verify: wrote {count} records, read {seen}"
                )));
            }
        }
        self.runs.push(path);
        self.spills += 1;
        self.current.clear();
        self.current_bytes = 0;
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<MergeIter> {
        if !self.current.is_empty() {
            self.current
                .sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        }
        let mut sources = Vec::new();
        for path in &self.runs {
            let file = File::open(path).map_err(sort_io)?;
            let mut reader = BufReader::new(file);
            let head = read_record(&mut reader)?;
            sources.push(Source::File { reader, head });
        }
        if !self.current.is_empty() {
            sources.push(Source::Memory {
                rows: std::mem::take(&mut self.current),
                at: 0,
            });
        }
        let mut heap = BinaryHeap::new();
        for (i, src) in sources.iter_mut().enumerate() {
            if let Some((k, v)) = src.take_head()? {
                heap.push(HeapItem {
                    key: k,
                    value: v,
                    run: i,
                });
            }
        }
        Ok(MergeIter {
            sources,
            heap,
            dir: self.dir.take(),
        })
    }
}

impl Drop for ExternalSorter {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

enum Source {
    Memory {
        rows: Vec<(Vec<u8>, Vec<u8>)>,
        at: usize,
    },
    File {
        reader: BufReader<File>,
        head: Option<(Vec<u8>, Vec<u8>)>,
    },
}

impl Source {
    fn take_head(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        match self {
            Self::Memory { rows, at } => {
                if *at >= rows.len() {
                    Ok(None)
                } else {
                    let row = std::mem::take(&mut rows[*at]);
                    *at += 1;
                    Ok(Some(row))
                }
            }
            Self::File { reader, head } => {
                if head.is_some() {
                    return Ok(head.take());
                }
                read_record(reader)
            }
        }
    }
}

struct HeapItem {
    key: Vec<u8>,
    value: Vec<u8>,
    run: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value && self.run == other.run
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse so pop yields the smallest key.
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.value.cmp(&self.value))
            .then_with(|| other.run.cmp(&self.run))
    }
}

/// k-way merge of spilled runs plus the leftover in-memory run.
pub(super) struct MergeIter {
    sources: Vec<Source>,
    heap: BinaryHeap<HeapItem>,
    dir: Option<PathBuf>,
}

impl MergeIter {
    pub(super) fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(item) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some((k, v)) = self.sources[item.run].take_head()? {
            self.heap.push(HeapItem {
                key: k,
                value: v,
                run: item.run,
            });
        }
        Ok(Some((item.key, item.value)))
    }
}

impl Drop for MergeIter {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(mut m: MergeIter) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(row) = m.next_entry().unwrap() {
            out.push(row);
        }
        out
    }

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn empty_sorter_yields_nothing() {
        let t = scratch();
        let s = ExternalSorter::new(t.path(), 64).unwrap();
        assert_eq!(s.spills(), 0);
        assert!(collect(s.finish().unwrap()).is_empty());
    }

    #[test]
    fn single_run_sorts_in_memory() {
        let t = scratch();
        let mut s = ExternalSorter::new(t.path(), 1 << 20).unwrap();
        s.push(b"c".to_vec(), b"3".to_vec()).unwrap();
        s.push(b"a".to_vec(), b"1".to_vec()).unwrap();
        s.push(b"b".to_vec(), b"2".to_vec()).unwrap();
        assert_eq!(s.spills(), 0);
        assert_eq!(
            collect(s.finish().unwrap()),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ]
        );
    }

    #[test]
    fn multi_run_merge_equals_full_in_memory_sort() {
        let t = scratch();
        // Tiny budget forces a spill every few entries.
        let mut s = ExternalSorter::new(t.path(), 24).unwrap();
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut expect = Vec::new();
        for i in 0..200u32 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = seed.to_be_bytes().to_vec();
            let value = i.to_be_bytes().to_vec();
            expect.push((key.clone(), value.clone()));
            s.push(key, value).unwrap();
        }
        let spills = s.spills();
        assert!(spills >= 2, "expected multiple spills, got {spills}");
        expect.sort();
        assert_eq!(collect(s.finish().unwrap()), expect);
    }

    #[test]
    fn byte_budget_counts_spills() {
        let t = scratch();
        let mut s = ExternalSorter::new(t.path(), 16).unwrap();
        for i in 0..10u8 {
            s.push(vec![i], vec![i]).unwrap();
        }
        assert!(
            s.spills() >= 3,
            "budget 16 over 10 records must spill, got {}",
            s.spills()
        );
        let got = collect(s.finish().unwrap());
        assert_eq!(got.len(), 10);
        for i in 1..got.len() {
            assert!(got[i - 1].0 <= got[i].0);
        }
    }

    #[test]
    fn duplicate_keys_are_yielded_adjacent() {
        let t = scratch();
        let mut s = ExternalSorter::new(t.path(), 20).unwrap();
        s.push(b"k".to_vec(), b"a".to_vec()).unwrap();
        s.push(b"k".to_vec(), b"c".to_vec()).unwrap();
        s.push(b"j".to_vec(), b"x".to_vec()).unwrap();
        s.push(b"k".to_vec(), b"b".to_vec()).unwrap();
        s.push(b"m".to_vec(), b"z".to_vec()).unwrap();
        let got = collect(s.finish().unwrap());
        let keys: Vec<_> = got.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, [b"j".as_slice(), b"k", b"k", b"k", b"m"]);
        let k_values: Vec<_> = got
            .iter()
            .filter(|(k, _)| k.as_slice() == b"k")
            .map(|(_, v)| v.as_slice())
            .collect();
        assert_eq!(k_values.len(), 3);
        assert!(k_values.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn spill_directory_is_removed_on_finish_and_on_drop() {
        let t = scratch();
        let parent = t.path();
        let empty = || parent.read_dir().unwrap().count();
        assert_eq!(empty(), 0);
        {
            let mut s = ExternalSorter::new(parent, 16).unwrap();
            for i in 0..20u8 {
                s.push(vec![i], vec![i]).unwrap();
            }
            assert!(s.spills() >= 1);
            assert!(empty() >= 1);
            drop(s);
        }
        assert_eq!(empty(), 0, "drop without finish must remove the spill dir");
        {
            let mut s = ExternalSorter::new(parent, 16).unwrap();
            for i in 0..20u8 {
                s.push(vec![i], vec![i]).unwrap();
            }
            assert!(s.spills() >= 1);
            let rows = collect(s.finish().unwrap());
            assert_eq!(rows.len(), 20);
        }
        assert_eq!(empty(), 0, "finish must remove the spill dir");
    }
}
