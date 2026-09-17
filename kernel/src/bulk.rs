//! Building a large tree by repeated insertion means one descent and one random
//! write per key. Sorting first and packing bottom-up means one sequential pass.
//!
//! Peak RAM is the arena in phase 1 and `MAX_FANOUT x buffer` in phase 2. Both
//! are chosen numbers, neither is proportional to the input. That is Law 1 by
//! construction rather than by care: without a fanout cap, `iter` would open
//! every run at once and merge memory would be `run_count * buffer`, and
//! `run_count` is `input / arena` -- linear in the input, correct at the test
//! scale and wrong at the target one. `SortedRuns::merge_down` bounds it by
//! merging runs down to at most `MAX_FANOUT` before the final pass.
//!
//! Phase 3, `pack_tree`, is bounded the same way and for the same reason. It
//! used to accumulate one `(first_key, page_no)` pair per page of the level it
//! was building into a `Vec`, which is one entry per page and therefore linear
//! in the rows -- correct at the test scale, wrong at the target one. Measured
//! on a 209M-row / 50.08 GiB load under a 1 GiB cap, that vector was several
//! hundred MiB of the 539 MiB `rss_anon` the process peaked at, against a
//! 113 MiB pool. Each level is now spilled to a scratch file as it is built and
//! streamed back to build the level above it, so peak pack memory is one write
//! buffer plus one read buffer whatever the input.
//!
//! SACRIFICE (Law 4): the input is written to temporary files and read back, so
//! a bulk load costs roughly 3x the data in sequential I/O and needs scratch disk
//! comparable to the input, plus one more read-and-rewrite pass of the whole
//! dataset for every factor of `MAX_FANOUT` the run count exceeds it. Bought:
//! linear build time instead of a curve, with merge memory that stays fixed
//! regardless of how large the input grows.
//!
//! SACRIFICE (Law 4), scratch integrity: every temporary record carries four
//! checksum bytes and is checksummed once when written and once when read.
//! Extra merge passes repeat that work. Bought: a changed scratch byte cannot
//! become a checksummed page and then pass the publication row-count check.
//!
//! SACRIFICE (Law 4), spilled separators: each level's separators are written
//! once and read once instead of being held. That is `16 + key_len` bytes per
//! PAGE of the level, not per row -- for 8-byte keys, 24 bytes per 4096-byte
//! page, so the level-0 file is about 0.59% of the tree it describes and every
//! level above it is another ~1/200th of that. Measured at 0.586% of the tree
//! (265,080 B of scratch against a 45.2 MB tree) by
//! `pack_tree_spill_is_a_small_fraction_of_the_tree` in
//! kernel/tests/pack_shape.rs, which polls the scratch directory while the pack
//! runs rather than deriving the figure from the record layout. Scaled to the
//! 50.08 GiB reference load that is ~300 MiB of extra sequential I/O. Bought: pack memory that does not grow with rows
//! at all, which is the whole premise of indexing 50 GB under a 1 GiB cap.

use crate::page::{PageKind, PageMut, HEADER_LEN, PAGE_SIZE};
use crate::pool::BufferPool;
use crate::io::AlignedRegion;
use crate::{Error, Result};
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Run-file framing. The top bit of vlen carries "this value is an overflow
/// MARKER" -- explicit in the framing, never inside the value bytes, because an
/// in-band tag was tried and ate the first byte of every value on recover's
/// path within the hour. Real vlen is far below 2^31.
const MARKER_BIT: u32 = 1 << 31;
const SCRATCH_HEADER_LEN: usize = 12;

fn invalid_scratch(why: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, why)
}

fn write_item(w: &mut impl Write, k: &[u8], v: &[u8], marker: bool) -> std::io::Result<()> {
    let kl = u32::try_from(k.len()).map_err(|_| invalid_scratch("scratch key is too long"))?;
    let vl = u32::try_from(v.len()).map_err(|_| invalid_scratch("scratch value is too long"))?;
    if vl & MARKER_BIT != 0 {
        return Err(invalid_scratch("scratch value is too long for the marker bit"));
    }
    let raw_vl = vl | if marker { MARKER_BIT } else { 0 };
    let mut header = [0u8; SCRATCH_HEADER_LEN];
    header[0..4].copy_from_slice(&kl.to_le_bytes());
    header[4..8].copy_from_slice(&raw_vl.to_le_bytes());
    let mut crc = crc32c::crc32c(&header[..8]);
    crc = crc32c::crc32c_append(crc, k);
    crc = crc32c::crc32c_append(crc, v);
    header[8..12].copy_from_slice(&crc.to_le_bytes());
    w.write_all(&header)?;
    w.write_all(k)?;
    w.write_all(v)
}

fn read_item(r: &mut impl Read, max_key_len: usize, max_val_len: usize)
    -> std::io::Result<Option<(Vec<u8>, Vec<u8>, bool)>>
{
    let mut h = [0u8; SCRATCH_HEADER_LEN];
    match r.read_exact(&mut h[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    // Once even one header byte exists, every other byte is mandatory.  This
    // is what distinguishes an exact record boundary from a torn header.
    r.read_exact(&mut h[1..])?;
    let kl = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
    let raw = u32::from_le_bytes(h[4..8].try_into().unwrap());
    let marker = raw & MARKER_BIT != 0;
    let vl = (raw & !MARKER_BIT) as usize;
    // These maxima came from the in-memory records handed to the writer, not
    // from this file.  Validate both disk lengths before either controls an
    // allocation; a corrupted u32 must not request gigabytes of RAM.
    if kl > max_key_len || vl > max_val_len {
        return Err(invalid_scratch("scratch record length exceeds its writer's bound"));
    }
    let mut k = Vec::new();
    k.try_reserve_exact(kl)
        .map_err(|_| invalid_scratch("scratch key allocation exceeds address space"))?;
    k.resize(kl, 0);
    r.read_exact(&mut k)?;
    let mut v = Vec::new();
    v.try_reserve_exact(vl)
        .map_err(|_| invalid_scratch("scratch value allocation exceeds address space"))?;
    v.resize(vl, 0);
    r.read_exact(&mut v)?;
    let want_crc = u32::from_le_bytes(h[8..12].try_into().unwrap());
    let mut crc = crc32c::crc32c(&h[..8]);
    crc = crc32c::crc32c_append(crc, &k);
    crc = crc32c::crc32c_append(crc, &v);
    if crc != want_crc {
        return Err(invalid_scratch("scratch record checksum mismatch"));
    }
    Ok(Some((k, v, marker)))
}

const SORT_MANIFEST_MAGIC: &[u8; 8] = b"KSRUN01\0";
const SORT_MANIFEST_MAX: u64 = 16 << 20;

struct SortManifest {
    watermark: u64,
    records: u64,
    framed_bytes: u64,
    max_key_len: usize,
    max_val_len: usize,
    runs: Vec<PathBuf>,
    run_counts: Vec<u64>,
}

fn manifest_u64(bytes: &[u8], pos: &mut usize) -> std::io::Result<u64> {
    let end = pos.checked_add(8).ok_or_else(|| invalid_scratch("sort manifest offset overflow"))?;
    let raw = bytes.get(*pos..end).ok_or_else(|| invalid_scratch("sort manifest is truncated"))?;
    *pos = end;
    Ok(u64::from_le_bytes(raw.try_into().unwrap()))
}

fn write_sort_manifest(sort: &ExternalSort, generation: u64, watermark: u64) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(SORT_MANIFEST_MAGIC);
    for n in [generation, watermark, sort.records, sort.framed_bytes,
              sort.max_key_len as u64, sort.max_val_len as u64, sort.runs.len() as u64] {
        body.extend_from_slice(&n.to_le_bytes());
    }
    for (path, &count) in sort.runs.iter().zip(&sort.run_counts) {
        let name = path.file_name().and_then(|s| s.to_str())
            .ok_or_else(|| Error::Io(invalid_scratch("durable run has no UTF-8 file name")))?;
        let name = name.as_bytes();
        let len = u16::try_from(name.len())
            .map_err(|_| Error::Io(invalid_scratch("durable run file name is too long")))?;
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(name);
        body.extend_from_slice(&count.to_le_bytes());
    }
    let crc = crc32c::crc32c(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    let tmp = sort.dir.join("manifest.tmp");
    let published = sort.dir.join("manifest");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&body)?;
        crate::write_stats::add(
            crate::write_stats::Phase::Manifest,
            body.len() as u64,
        );
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &published)?;
    File::open(&sort.dir)?.sync_all()?;
    Ok(())
}

fn read_sort_manifest(dir: &Path, expected_generation: u64) -> Result<SortManifest> {
    let path = dir.join("manifest");
    let len = std::fs::metadata(&path)?.len();
    if len < (SORT_MANIFEST_MAGIC.len() + 7 * 8 + 4) as u64 || len > SORT_MANIFEST_MAX {
        return Err(Error::Io(invalid_scratch("sort manifest length is invalid")));
    }
    let bytes = std::fs::read(&path)?;
    let split = bytes.len() - 4;
    let want = u32::from_le_bytes(bytes[split..].try_into().unwrap());
    if crc32c::crc32c(&bytes[..split]) != want {
        return Err(Error::Io(invalid_scratch("sort manifest checksum mismatch")));
    }
    if bytes.get(..8) != Some(SORT_MANIFEST_MAGIC) {
        return Err(Error::Io(invalid_scratch("sort manifest magic mismatch")));
    }
    let mut pos = 8;
    let generation = manifest_u64(&bytes[..split], &mut pos)?;
    if generation != expected_generation {
        return Err(Error::Io(invalid_scratch("sort manifest generation mismatch")));
    }
    let watermark = manifest_u64(&bytes[..split], &mut pos)?;
    let records = manifest_u64(&bytes[..split], &mut pos)?;
    let framed_bytes = manifest_u64(&bytes[..split], &mut pos)?;
    let max_key_len = usize::try_from(manifest_u64(&bytes[..split], &mut pos)?)
        .map_err(|_| Error::TooLarge)?;
    let max_val_len = usize::try_from(manifest_u64(&bytes[..split], &mut pos)?)
        .map_err(|_| Error::TooLarge)?;
    let run_count = usize::try_from(manifest_u64(&bytes[..split], &mut pos)?)
        .map_err(|_| Error::TooLarge)?;
    // Every entry needs at least a u16 name length and u64 row count.
    if run_count > (split.saturating_sub(pos)) / 10 {
        return Err(Error::Io(invalid_scratch("sort manifest run count exceeds its bytes")));
    }
    let mut runs = Vec::with_capacity(run_count);
    let mut run_counts = Vec::with_capacity(run_count);
    for _ in 0..run_count {
        let end = pos.checked_add(2).ok_or(Error::TooLarge)?;
        let raw = bytes.get(pos..end)
            .ok_or_else(|| Error::Io(invalid_scratch("sort manifest run name is truncated")))?;
        pos = end;
        let name_len = u16::from_le_bytes(raw.try_into().unwrap()) as usize;
        let end = pos.checked_add(name_len).ok_or(Error::TooLarge)?;
        let raw_name = bytes.get(pos..end)
            .ok_or_else(|| Error::Io(invalid_scratch("sort manifest run name is truncated")))?;
        pos = end;
        let name = std::str::from_utf8(raw_name)
            .map_err(|_| Error::Io(invalid_scratch("sort manifest run name is not UTF-8")))?;
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err(Error::Io(invalid_scratch("sort manifest run name escapes its directory")));
        }
        let path = dir.join(name);
        if !path.is_file() {
            return Err(Error::Io(invalid_scratch("sort manifest names a missing run")));
        }
        runs.push(path);
        run_counts.push(manifest_u64(&bytes[..split], &mut pos)?);
    }
    if pos != split {
        return Err(Error::Io(invalid_scratch("sort manifest has trailing bytes")));
    }
    if run_counts.iter().try_fold(0u64, |sum, &n| sum.checked_add(n)) != Some(records) {
        return Err(Error::Io(invalid_scratch("sort manifest row counts disagree")));
    }
    Ok(SortManifest { watermark, records, framed_bytes, max_key_len, max_val_len,
        runs, run_counts })
}

/// Removes its scratch directory if it is dropped without `finish()`.
///
/// `push` can fail, and `Store::bulk_load` returns through `?` when it does
/// -- before `finish()` has produced the `SortedRuns` whose own `Drop` would
/// have cleaned up. Without this, every failed bulk load leaves a temp
/// directory behind for the life of the machine.
pub struct ExternalSort {
    finished: bool,
    dir: PathBuf,
    arena: Vec<(Vec<u8>, Vec<u8>, bool)>,
    arena_bytes: usize,
    used: usize,
    runs: Vec<PathBuf>,
    run_counts: Vec<u64>,
    max_key_len: usize,
    max_val_len: usize,
    records: u64,
    framed_bytes: u64,
    /// A durable build generation retains runs on Drop and publishes a
    /// reopenable manifest after each explicit watermark checkpoint.
    durable_generation: Option<u64>,
}

impl ExternalSort {
    pub fn new(dir: &Path, arena_bytes: usize) -> Result<Self> {
        Self::new_inner(dir, arena_bytes, None)
    }

    /// Create a sorter whose runs survive process death. `generation` is the
    /// build generation, not the store page generation; reopen refuses a
    /// manifest from a different build so stale scratch cannot be attached to
    /// a later CREATE INDEX using the same directory.
    pub fn new_durable(dir: &Path, arena_bytes: usize, generation: u64) -> Result<Self> {
        Self::new_inner(dir, arena_bytes, Some(generation))
    }

    fn new_inner(dir: &Path, arena_bytes: usize, durable_generation: Option<u64>) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(ExternalSort {
            finished: false, dir: dir.to_path_buf(), arena: Vec::new(), arena_bytes, used: 0,
            runs: Vec::new(), run_counts: Vec::new(), max_key_len: 0, max_val_len: 0,
            records: 0, framed_bytes: 0, durable_generation,
        })
    }

    /// Push with the overflow-marker flag carried explicitly through the run
    /// files. `val` is whatever the caller's pipeline stores (recover tags it
    /// with a page number); the flag survives sort and merge untouched.
    pub fn push_flagged(&mut self, key: Vec<u8>, val: Vec<u8>, marker: bool) -> Result<()> {
        self.push_inner(key, val, marker)
    }

    pub fn push(&mut self, key: Vec<u8>, val: Vec<u8>) -> Result<()> {
        self.push_inner(key, val, false)
    }

    fn push_inner(&mut self, key: Vec<u8>, val: Vec<u8>, marker: bool) -> Result<()> {
        self.records = self.records.checked_add(1).ok_or(Error::TooLarge)?;
        self.framed_bytes = self.framed_bytes
            .checked_add((SCRATCH_HEADER_LEN + key.len() + val.len()) as u64)
            .ok_or(Error::TooLarge)?;
        self.max_key_len = self.max_key_len.max(key.len());
        self.max_val_len = self.max_val_len.max(val.len());
        self.used += key.len() + val.len() + 48;   // 48 = two Vec headers, approx
        self.arena.push((key, val, marker));
        if self.used >= self.arena_bytes { self.spill()?; }
        Ok(())
    }

    fn spill(&mut self) -> Result<()> {
        if self.arena.is_empty() { return Ok(()); }
        self.arena.sort_by(|a, b| a.0.cmp(&b.0));
        let path = match self.durable_generation {
            Some(generation) => self.dir.join(format!("g{generation:016x}-run-{:05}.dat", self.runs.len())),
            None => self.dir.join(format!("run-{:05}.tmp", self.runs.len())),
        };
        let mut w = BufWriter::new(File::create(&path)?);
        let count = u64::try_from(self.arena.len()).map_err(|_| Error::TooLarge)?;
        let mut written = 0u64;
        for (k, v, m) in self.arena.drain(..) {
            written = written.checked_add(
                (SCRATCH_HEADER_LEN + k.len() + v.len()) as u64,
            ).ok_or(Error::TooLarge)?;
            write_item(&mut w, &k, &v, m)?;
        }
        w.flush()?;
        crate::write_stats::add(crate::write_stats::Phase::SortScratch, written);
        if self.durable_generation.is_some() { w.get_ref().sync_all()?; }
        self.runs.push(path);
        self.run_counts.push(count);
        self.used = 0;
        Ok(())
    }

    /// Seal the current arena as one checksummed run without finishing the
    /// sorter. Chunked SQL builders call this at their scan watermark so peak
    /// live heap is bounded by the fixed chunk even when the configured arena
    /// is larger (the arena remains the hard upper ceiling).
    pub fn flush_run(&mut self) -> Result<()> { self.spill() }

    /// Mechanism counters for load profiling.  `framed_bytes` is the exact
    /// first-pass scratch payload (including each record header), excluding
    /// any extra merge-down pass.  The pending arena counts as one future run.
    pub fn profile(&self) -> (u64, u64, usize) {
        (self.records, self.framed_bytes,
         self.runs.len() + usize::from(!self.arena.is_empty()))
    }

    /// Flush one scan watermark and atomically publish the complete run list.
    /// The returned cost includes the run fsync, manifest fsync, rename and
    /// directory fsync: exactly the durability tax paid at this interval.
    pub fn checkpoint(&mut self, watermark: u64) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        let generation = self.durable_generation.ok_or_else(|| Error::Io(
            invalid_scratch("checkpoint requested for a non-durable sorter")))?;
        self.spill()?;
        write_sort_manifest(self, generation, watermark)?;
        Ok(started.elapsed())
    }

    /// Reopen a durable sorter at its last completely published watermark.
    /// Torn `.tmp` manifests are ignored; every listed run is later checked by
    /// the ordinary framed CRC and exact record-count reader.
    pub fn reopen_durable(dir: &Path, arena_bytes: usize, expected_generation: u64)
        -> Result<(Self, u64)>
    {
        let manifest = read_sort_manifest(dir, expected_generation)?;
        let mut sort = Self::new_inner(dir, arena_bytes, Some(expected_generation))?;
        sort.runs = manifest.runs;
        sort.run_counts = manifest.run_counts;
        sort.max_key_len = manifest.max_key_len;
        sort.max_val_len = manifest.max_val_len;
        sort.records = manifest.records;
        sort.framed_bytes = manifest.framed_bytes;
        Ok((sort, manifest.watermark))
    }

    /// Successful publication owns the cleanup decision. Until this is
    /// called, dropping the sorter deliberately leaves its checkpoint intact.
    pub fn discard_durable(mut self) -> Result<()> {
        self.finished = true;
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<SortedRuns> {
        self.spill()?;
        self.finished = true;
        Ok(SortedRuns {
            dir: std::mem::take(&mut self.dir), runs: std::mem::take(&mut self.runs),
            owns_dir: self.durable_generation.is_none(),
            run_counts: std::mem::take(&mut self.run_counts),
            max_key_len: self.max_key_len, max_val_len: self.max_val_len,
        })
    }
}

impl Drop for ExternalSort {
    fn drop(&mut self) {
        if !self.finished && self.durable_generation.is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

pub struct SortedRuns {
    dir: PathBuf,
    runs: Vec<PathBuf>,
    /// Trusted record count beside each run, so truncation exactly at a
    /// record boundary cannot masquerade as the run ending normally.
    run_counts: Vec<u64>,
    owns_dir: bool,
    /// Trusted allocation bounds retained from the caller's in-memory input.
    max_key_len: usize,
    max_val_len: usize,
}

impl SortedRuns {
    pub fn run_count(&self) -> usize { self.runs.len() }

    /// Remove a durable sort checkpoint after its graft and final metadata
    /// publication are known durable. Ordinary temporary runs already clean
    /// themselves on Drop; accepting both shapes keeps caller cleanup simple.
    pub fn discard(mut self) -> Result<()> {
        self.owns_dir = false;
        if self.dir.exists() { std::fs::remove_dir_all(&self.dir)?; }
        Ok(())
    }

    /// The most runs `iter` will hold open at once.
    ///
    /// Without a cap, `iter` opens EVERY run with a read buffer each, so merge
    /// memory is `run_count * buffer` -- and `run_count` is `input / arena`,
    /// which makes it linear in the input. Law 1 forbids that: it happens to
    /// fit at moderate scale and stops fitting above it, which is precisely
    /// the shape this project exists to avoid -- correct at the test scale,
    /// wrong at the target one, invisible until someone's data grows.
    pub const MAX_FANOUT: usize = 64;

    /// Merge groups of runs into intermediate runs until at most `MAX_FANOUT`
    /// remain, so the final merge's memory is bounded by a chosen number
    /// rather than by the input.
    ///
    /// SACRIFICE (Law 4): each extra pass reads and rewrites the whole
    /// dataset once more. Bought: merge memory that does not grow with the
    /// input at all.
    fn merge_down(&mut self) -> Result<()> {
        // A monotonic counter across the WHOLE call, not just within one
        // pass. `next.len()`/`gi` alone repeat every pass (`next` always
        // starts at 0), so a name built only from them collides with a
        // SURVIVOR of the previous pass sitting at the same position in
        // `self.runs` -- concretely, pass 2's group 0 output and pass 1's
        // group 0 output are both named `pass-0-00000.tmp`, and pass 1's
        // output is typically pass 2's group 0's OWN first input. Creating
        // `out` then truncates that input before it is read (silently
        // losing every record it held), and the later cleanup below deletes
        // the file `out` itself, since it was also listed as one of
        // `group`'s members -- so the very output just written vanishes and
        // the next `File::open` on it panics with "No such file or
        // directory". Reachable on any input needing more than one
        // `merge_down` pass (run_count > MAX_FANOUT^2), which is exactly the
        // scale this bound exists to make safe -- found by forcing a
        // two-pass `merge_down` in a scratch test, not by inspection alone.
        let mut pass_no = 0usize;
        while self.runs.len() > Self::MAX_FANOUT {
            let mut next: Vec<PathBuf> = Vec::new();
            let mut next_counts: Vec<u64> = Vec::new();
            for (gi, start) in (0..self.runs.len()).step_by(Self::MAX_FANOUT).enumerate() {
                let end = (start + Self::MAX_FANOUT).min(self.runs.len());
                let group = &self.runs[start..end];
                let group_count = self.run_counts[start..end].iter().try_fold(0u64, |total, &n| {
                    total.checked_add(n).ok_or(Error::TooLarge)
                })?;
                if group.len() == 1 {
                    next.push(group[0].clone());
                    next_counts.push(group_count);
                    continue;
                }
                let out = self.dir.join(format!("pass-{}-{}-{:05}.tmp", pass_no, next.len(), gi));
                let mut w = BufWriter::new(File::create(&out)?);
                // `owns_dir: false` -- this SortedRuns shares `self.dir` with
                // its parent. If its Drop removed that directory the way the
                // owning one does, finishing this group's merge would delete
                // run files sibling groups (and the parent) still need. Same
                // shape as the temp-directory race `Store::bulk_load` guards
                // against with its per-call sequence number, one level in:
                // here the collision is between a SortedRuns and the parent
                // it was carved out of, not between two unrelated calls.
                let mut part = SortedRuns {
                    dir: self.dir.clone(), runs: group.to_vec(), owns_dir: false,
                    run_counts: self.run_counts[start..end].to_vec(),
                    max_key_len: self.max_key_len, max_val_len: self.max_val_len,
                };
                let mut written = 0u64;
                for item in part.iter_unbounded()? {
                    let (k, v, m) = item?;
                    write_item(&mut w, &k, &v, m)?;
                    written = written.checked_add(
                        (SCRATCH_HEADER_LEN + k.len() + v.len()) as u64,
                    ).ok_or(Error::TooLarge)?;
                }
                w.flush()?;
                crate::write_stats::add(crate::write_stats::Phase::SortScratch, written);
                for p in group { let _ = std::fs::remove_file(p); }
                next.push(out);
                next_counts.push(group_count);
            }
            self.runs = next;
            self.run_counts = next_counts;
            pass_no += 1;
        }
        Ok(())
    }

    /// K-way merge over at most `MAX_FANOUT` runs. RAM is one buffered
    /// reader per open run, a chosen bound rather than an input-shaped one.
    pub fn iter(&mut self) -> Result<MergeIter> {
        self.merge_down()?;
        self.iter_unbounded()
    }

    /// Open every remaining run and build the merge over them.
    ///
    /// Returns a `Result` because opening a file can fail for reasons that
    /// have nothing to do with the caller: a permission change, a full disk,
    /// a future regression that reintroduces a name collision. An earlier
    /// version used `File::open(p).unwrap()` -- the filename-collision fix
    /// removed the one *trigger* this project had found for that panic and
    /// left the panic itself standing for every other cause, contradicting
    /// the `pending_err`/`done` machinery two paragraphs below, which exists
    /// specifically because nothing else in this crate aborts on a
    /// recoverable I/O error. `iter`, and `merge_down`'s own internal use of
    /// `iter_unbounded`, both propagate this with `?` rather than unwrap it.
    ///
    /// The actual k-way merge over whatever is currently in `self.runs`, with
    /// no fanout bound of its own -- callers (`iter`, after `merge_down`, and
    /// `merge_down` itself over one `<= MAX_FANOUT` group) are what keep the
    /// run count this opens bounded.
    fn iter_unbounded(&mut self) -> Result<MergeIter> {
        let mut readers = Vec::with_capacity(self.runs.len());
        // One fixed aggregate read arena, divided across the active fanout.
        // A buffer per run with a fixed per-buffer size makes live heap grow
        // with the number of corpus runs until MAX_FANOUT; bounded is not the
        // same as independent of corpus size. Four KiB is one page at the
        // maximum 64-way fanout, 256 KiB for a single-run replay.
        let per_reader = (256 * 1024 / self.runs.len().max(1)).max(4096);
        for p in &self.runs {
            readers.push(BufReader::with_capacity(per_reader, File::open(p)?));
        }
        let mut m = MergeIter {
            readers, heap: BinaryHeap::new(), pending_err: None, done: false,
            max_key_len: self.max_key_len, max_val_len: self.max_val_len,
            expected: self.run_counts.iter().try_fold(0u64, |total, &n| {
                total.checked_add(n).ok_or(Error::TooLarge)
            })?,
            yielded: 0,
        };
        for i in 0..m.readers.len() {
            // An I/O error this early (priming a run's first record) must
            // not be swallowed as if the run were simply empty -- that would
            // drop every key still sitting in it with no signal to the
            // caller. Keep priming the rest so their handles stay primed,
            // and hand back the first error once iteration starts.
            if let Err(e) = m.pull(i) {
                if m.pending_err.is_none() { m.pending_err = Some(e); }
            }
        }
        Ok(m)
    }
}

impl Drop for SortedRuns {
    fn drop(&mut self) {
        // Only the owner removes the directory. A temporary SortedRuns built
        // over a subset of runs during `merge_down` must not delete the
        // scratch its parent (or sibling groups) is still using.
        if self.owns_dir { let _ = std::fs::remove_dir_all(&self.dir); }
    }
}

/// Reversed ordering so BinaryHeap (a max-heap) yields the smallest key.
struct Head { key: Vec<u8>, val: Vec<u8>, marker: bool, from: usize }
impl PartialEq for Head { fn eq(&self, o: &Self) -> bool { self.key == o.key } }
impl Eq for Head {}
impl Ord for Head {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.key.cmp(&self.key).then(o.from.cmp(&self.from)) }
}
impl PartialOrd for Head { fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) } }

pub struct MergeIter {
    readers: Vec<BufReader<File>>,
    heap: BinaryHeap<Head>,
    /// An I/O error observed while refilling from a run. `read_item` cannot
    /// tell corruption/truncation apart from "the run legitimately ended" by
    /// itself, so `pull` must not collapse a real error into `None` the way
    /// the reference sketch did -- that reads as the run finishing early and
    /// silently drops every key still sitting in it, with the merge and its
    /// caller both reporting success. Carried here because `pull` runs both
    /// from `next` (which can return it immediately) and from `SortedRuns::iter`
    /// while priming (which cannot: the `Iterator` isn't constructed yet).
    pending_err: Option<Error>,
    /// Set once `pending_err` has been handed back. Same discipline as
    /// `RangeIter::fail` in btree.rs: after an error, stop for good rather
    /// than keep popping the heap and serving items from runs the error
    /// didn't touch, which would look like the merge completed when an
    /// unknown number of keys from the broken run were never emitted.
    done: bool,
    max_key_len: usize,
    max_val_len: usize,
    expected: u64,
    yielded: u64,
}

impl MergeIter {
    fn pull(&mut self, i: usize) -> Result<()> {
        match read_item(&mut self.readers[i], self.max_key_len, self.max_val_len) {
            Ok(Some((k, v, m))) => { self.heap.push(Head { key: k, val: v, marker: m, from: i }); Ok(()) }
            Ok(None) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

impl Iterator for MergeIter {
    type Item = Result<(Vec<u8>, Vec<u8>, bool)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done { return None; }
        if let Some(e) = self.pending_err.take() {
            self.done = true;
            return Some(Err(e));
        }
        let Some(h) = self.heap.pop() else {
            self.done = true;
            if self.yielded == self.expected { return None; }
            return Some(Err(Error::Io(invalid_scratch(
                "scratch run ended before its recorded item count",
            ))));
        };
        if self.yielded >= self.expected {
            self.done = true;
            return Some(Err(Error::Io(invalid_scratch(
                "scratch run exceeded its recorded item count",
            ))));
        }
        self.yielded += 1;
        if let Err(e) = self.pull(h.from) { self.pending_err = Some(e); }
        Some(Ok((h.key, h.val, h.marker)))
    }
}

fn enc_leaf(key: &[u8], val: &[u8], compact: bool) -> Vec<u8> {
    crate::btree::enc_leaf(key,val,compact)
}

fn enc_interior(key: &[u8], child: u32) -> Vec<u8> {
    let mut r = Vec::with_capacity(6 + key.len());
    r.extend_from_slice(&(key.len() as u16).to_le_bytes());
    r.extend_from_slice(key);
    r.extend_from_slice(&child.to_le_bytes());
    r
}

/// Build one page's full contents in a scratch buffer, then return it whole.
///
/// Mirrors `btree.rs`'s `build_page`: writing straight into a live pinned
/// frame and then hitting a fallible `insert_slot` would leave that frame
/// wiped, dirty and never finalised -- a stale checksum over a blank page,
/// which the next read refuses, losing every key that was meant to land on
/// it. Building in scratch first removes the class rather than relying on
/// the packing arithmetic below never being wrong.
fn build_scratch(kind: PageKind, tree_id: u16, page_no: u32, recs: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut scratch = vec![0u8; PAGE_SIZE];
    {
        let mut p = PageMut::init(&mut scratch, kind, tree_id, page_no);
        for r in recs {
            let at = p.nentries_pub();
            p.insert_slot(at, r)?;
        }
        p.finalise(0);
    }
    Ok(scratch)
}

/// Fixed 256 KiB write-behind used only for unreachable packed candidates.
/// Page numbers from the append allocator are contiguous in the common case;
/// a freelist discontinuity flushes the current run and starts another.
struct PackedPageWriter<'a> {
    pool: &'a BufferPool,
    region: AlignedRegion,
    first: Option<u32>,
    pages: usize,
}

impl<'a> PackedPageWriter<'a> {
    // The 64-page write aggregation did not move the 250k end-to-end wall
    // (9.21s baseline, 9.20s one-page ablation) and the combined server 1M
    // result remained flat. Keep the uncached candidate-page path, which
    // avoids polluting the shared pool, but omit the unearned aggregation.
    const CAPACITY: usize = 1;

    fn new(pool: &'a BufferPool) -> Result<Self> {
        Ok(Self {
            pool,
            region: AlignedRegion::new(Self::CAPACITY * PAGE_SIZE)?,
            first: None,
            pages: 0,
        })
    }

    fn push(&mut self, page_no: u32, scratch: &[u8]) -> Result<()> {
        if scratch.len() != PAGE_SIZE { return Err(Error::TooLarge); }
        if self.pages == Self::CAPACITY
            || self.first.is_some_and(|first| first + self.pages as u32 != page_no)
        {
            self.flush()?;
        }
        if self.first.is_none() { self.first = Some(page_no); }
        // SAFETY: this writer exclusively owns the region and no slice escapes.
        let page = unsafe { self.region.page_mut(self.pages) };
        page.copy_from_slice(scratch);
        crate::page::seal(page, self.pool.stamp_generation());
        self.pages += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.pages == 0 { return Ok(()); }
        let len = self.pages * PAGE_SIZE;
        // SAFETY: no mutable page slice is live; the write completes before
        // the region can be reused.
        let bytes = unsafe { self.region.prefix(len) };
        self.pool.write_unpooled_run(self.first.unwrap(), bytes)?;
        self.first = None;
        self.pages = 0;
        Ok(())
    }
}

/// Where a packed page goes, and therefore which durability rule it obeys.
///
/// `Direct` reserves an unpooled page number and writes the finished, sealed
/// page straight into the data file. Those pages are unreachable until the
/// root swap publishes them, which is what lets `Store::graft_range` skip the
/// record WAL for them entirely (D11).
///
/// `Pooled` allocates ordinary pool pages instead. A page-WAL database then
/// logs every packed page as a normal frame and publishes the whole graft
/// with the ordinary commit, so there is no root swap, no skipped log and no
/// second durability rule to reason about: Law 3 and Law 5 hold exactly as
/// they do for a single `insert`. The price is one WAL frame per packed page
/// -- the same frame an ordinary insert would have paid for that leaf anyway,
/// but paid once instead of once per commit the leaf is dirty in.
///
/// A page is RESERVED before it is filled, because a leaf's `next_leaf`
/// pointer is only known once the following leaf has a number. The pooled
/// sink therefore holds the write guard between `reserve` and `push` (at most
/// two at a time: the leaf being finished and the leaf that follows it),
/// which keeps every packed page a single pooled write with no read-back.
pub(crate) enum PageSink<'a> {
    Direct(PackedPageWriter<'a>),
    Pooled { pool: &'a BufferPool, held: Vec<crate::pool::PinnedWrite<'a>> },
}

impl<'a> PageSink<'a> {
    pub(crate) fn direct(pool: &'a BufferPool) -> Result<Self> {
        Ok(PageSink::Direct(PackedPageWriter::new(pool)?))
    }

    pub(crate) fn pooled(pool: &'a BufferPool) -> Self {
        PageSink::Pooled { pool, held: Vec::new() }
    }

    fn reserve(&mut self) -> Result<u32> {
        match self {
            PageSink::Direct(writer) => writer.pool.allocate_unpooled(),
            PageSink::Pooled { pool, held } => {
                let guard = pool.allocate()?;
                let page_no = guard.page_no();
                held.push(guard);
                Ok(page_no)
            }
        }
    }

    fn push(&mut self, page_no: u32, scratch: &[u8]) -> Result<()> {
        match self {
            PageSink::Direct(writer) => writer.push(page_no, scratch),
            PageSink::Pooled { held, .. } => {
                if scratch.len() != PAGE_SIZE { return Err(Error::TooLarge); }
                // A push for a number this sink never reserved would write a
                // page the allocator still considers free: refuse instead of
                // guessing, the same way the direct writer refuses a run that
                // exceeds its allocation.
                let at = held.iter().position(|guard| guard.page_no() == page_no)
                    .ok_or(Error::Corrupt { page_no, why: "packed page was never reserved" })?;
                let mut guard = held.remove(at);
                guard.bytes_mut().copy_from_slice(scratch);
                Ok(())
            }
        }
    }

    /// No page may stay pinned past the pack: `flush_all` asserts that a
    /// dirty frame has no live guard, so a guard held across the caller's
    /// commit would be a panic, not a slow path. Guards still held here
    /// belong to reserved-but-unfilled pages on an error path; dropping them
    /// leaves each as the valid empty Free page `allocate` initialised.
    fn finish(&mut self) -> Result<()> {
        match self {
            PageSink::Direct(writer) => writer.flush(),
            PageSink::Pooled { held, .. } => { held.clear(); Ok(()) }
        }
    }
}

/// One level's separators, on disk instead of in RAM.
///
/// `pack_tree` builds a level of pages and needs, for each page, the pair
/// `(first_key, page_no)` to build the level above it. Held in a `Vec`, that is
/// one entry per page and so linear in the rows -- the same shape
/// `MAX_FANOUT` above exists to forbid, one phase later. This holds the pairs
/// in a file and hands back a reader, so the pack's memory is a write buffer
/// plus a read buffer regardless of how many pages the level has.
///
/// `Drop` closes the writer and unlinks the file unconditionally. That is what
/// makes the scratch directory empty after a FAILED pack as well as a
/// successful one: every early return in `pack_tree` -- a duplicate key, an
/// oversized record, a pool that cannot allocate -- drops the live
/// `Separators` on its way out.
struct Separators {
    path: PathBuf,
    /// `None` once `seal` has flushed and closed it.
    w: Option<BufWriter<File>>,
    /// Entries written. The pack needs only "is this level one page yet?",
    /// which is `count == 1`, and "was there any input at all?", `count == 0`.
    count: u64,
    /// Trusted upper bound for a key length read back from this file.
    max_key_len: usize,
    framed_bytes: u64,
}

impl Separators {
    /// `seq` is a counter that runs across the WHOLE pack and is never reset
    /// per level, and `pack` is a per-call number from a process-wide atomic.
    ///
    /// A name built from the level number alone would happen to be unique
    /// today, and that is exactly the reasoning that produced the
    /// `merge_down` collision documented above: a name derived only from
    /// position repeats the moment the loop producing it runs more than once,
    /// and the file it collides with is typically one still being read, so
    /// `File::create` truncates live data with no error anywhere. This
    /// creates temp files across LEVELS, which is structurally the same
    /// hazard, so the counter is monotonic across the whole call. `pack` and
    /// the pid do for concurrent packs sharing a scratch directory what
    /// `Store::bulk_load`'s per-call sequence number does for the sort, and
    /// keep these names disjoint from `run-*.tmp` and `pass-*.tmp` besides.
    fn create(dir: &Path, pack: u64, seq: &mut u64) -> Result<Self> {
        let n = *seq;
        *seq += 1;
        let path = dir.join(format!("sep-{}-{}-{:05}.tmp", std::process::id(), pack, n));
        let w = BufWriter::with_capacity(256 * 1024, File::create(&path)?);
        Ok(Separators {
            path,
            w: Some(w),
            count: 0,
            max_key_len: 0,
            framed_bytes: 0,
        })
    }

    fn push(&mut self, key: &[u8], page_no: u32) -> Result<()> {
        let w = self.w.as_mut().expect("separators pushed after seal");
        write_item(w, key, &page_no.to_le_bytes(), false)?;
        self.max_key_len = self.max_key_len.max(key.len());
        self.count += 1;
        self.framed_bytes = self.framed_bytes
            .checked_add((SCRATCH_HEADER_LEN + key.len() + 4) as u64)
            .ok_or(Error::TooLarge)?;
        Ok(())
    }

    /// Flush and close. Explicit rather than left to `Drop`, because a
    /// `BufWriter` dropped with bytes still buffered discards the write error
    /// and the level would simply be short -- pages silently absent from the
    /// tree, which is the failure mode this crate refuses everywhere else.
    fn seal(&mut self) -> Result<()> {
        if let Some(mut w) = self.w.take() {
            w.flush()?;
            crate::write_stats::add(
                crate::write_stats::Phase::PackScratch,
                self.framed_bytes,
            );
        }
        Ok(())
    }

    fn reader(&self) -> Result<BufReader<File>> {
        // `push` after `seal` is already a hard error; this is the mirror.
        // Reading before `seal` would open the path and get whatever had
        // happened to reach the filesystem -- silently short by up to a whole
        // write buffer, which is the exact failure `short_read` below exists
        // to catch and the exact reason `seal` is explicit at all. No current
        // path does it; the seal discipline is load-bearing enough to say so.
        debug_assert!(self.w.is_none(), "separators read before seal");
        Ok(BufReader::with_capacity(256 * 1024, File::open(&self.path)?))
    }
}

impl Drop for Separators {
    fn drop(&mut self) {
        // Writer first, then unlink -- and unlink even when `seal` already
        // ran, since the file is scratch either way.
        self.w = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A level file yielded fewer separators than were written to it.
///
/// `write_item` uses `write_all` into a `BufWriter`, `count` is incremented
/// only after that returns `Ok`, and `seal` flushes explicitly so a buffered
/// write error surfaces rather than leaving the file short -- so the write
/// side is defended. The READ side was not, and `read_item` cannot tell the
/// difference by itself: EOF at a record boundary reads as "the level ended"
/// and EOF mid-record as an error, so a file short by a whole number of
/// records is indistinguishable from a complete one.
///
/// What that costs is not a crash. Each missing separator is a page the level
/// above never points at, so an entire subtree becomes unreachable by descent
/// while the leaf sibling chain -- stitched independently of this file --
/// still walks every leaf. `get` misses keys that `range` returns: exactly the
/// point-lookup/scan divergence `Error::DuplicateKey` refuses inputs to
/// prevent, arriving silently, with `bulk_load` returning `Ok`. Verified by
/// injection: truncating the level-0 file by one whole record left 144 of
/// 50,000 keys unreachable by `get` while `scan` still counted all 50,000.
/// A full-scan row count -- the check the 209M-row load was verified with --
/// cannot see it.
///
/// This file's own recorded history is a temp-name collision that truncated a
/// file still being read, so "nothing can truncate it" is not a claim this
/// code gets to make about itself. One `u64` and one comparison per level
/// converts the whole class from silent to loud.
fn short_level(seen: u64, want: u64) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("a separator level came back with {seen} of {want} entries"),
    ))
}

/// Read one `(first_key, page_no)` back off a level file.
fn read_sep(r: &mut BufReader<File>, max_key_len: usize) -> Result<Option<(Vec<u8>, u32)>> {
    match read_item(r, max_key_len, 4)? {
        None => Ok(None),
        Some((k, v, _)) => {
            // A value that is not exactly four bytes means the level file was
            // truncated or scribbled on. Taking a page number out of the
            // wrong bytes would build interior pages pointing at arbitrary
            // pages -- a structurally valid tree serving wrong answers -- so
            // refuse instead. Not `Corrupt`: that carries a page number, and
            // page 0 is the superblock; a scratch file has no page at all, so
            // naming one would read as damage to the store itself.
            if v.len() != 4 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "separator spill record is not a 4-byte page number",
                )));
            }
            Ok(Some((k, u32::from_le_bytes(v[..4].try_into().unwrap()))))
        }
    }
}

/// The physical result of packing one sorted key range.  `first_leaf` and
/// `last_leaf` let a caller stitch the range into an existing leaf sequence
/// without walking the packed tree or retaining one separator per page.
#[derive(Debug, Clone)]
pub(crate) struct PackedRange {
    pub(crate) root: u32,
    pub(crate) first_leaf: u32,
    pub(crate) last_leaf: u32,
    pub(crate) rows: u64,
    pub(crate) min: Option<Vec<u8>>,
    pub(crate) max: Option<Vec<u8>>,
}

/// Pack a sorted stream into a fresh tree, bottom up.
///
/// `last_next` is the already-known leaf immediately to the right of this
/// range.  Whole-tree builds pass zero.  A graft passes the copied boundary
/// leaf (or the standing right sibling), which means the final packed page is
/// complete before its first and only write to disk.  No post-verification
/// pointer patch is needed.
///
/// `scratch_dir` is where each level's separators are spilled. Callers pass
/// the directory they already made for the sort (`Store::bulk_load`,
/// `recover`), rather than this inventing a second temp-file scheme with its
/// own lifetime and its own collision rules; the files created here are
/// removed as each level is consumed, and on every error path.
pub(crate) fn pack_range<I>(
    pool: &BufferPool,
    tree_id: u16,
    sorted: I,
    fill: f32,
    scratch_dir: &Path,
    last_next: u32,
) -> Result<PackedRange>
where I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    let mut sink = PageSink::direct(pool)?;
    pack_range_into(pool, tree_id, sorted, fill, scratch_dir, last_next, &mut sink)
}

/// The same pack, with every page allocated and written through the ordinary
/// buffer pool. A page-WAL store uses this so a graft is an ordinary logged
/// transaction; see [`PageSink`].
pub(crate) fn pack_range_pooled<I>(
    pool: &BufferPool,
    tree_id: u16,
    sorted: I,
    fill: f32,
    scratch_dir: &Path,
    last_next: u32,
) -> Result<PackedRange>
where I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    let mut sink = PageSink::pooled(pool);
    let packed = pack_range_into(pool, tree_id, sorted, fill, scratch_dir, last_next, &mut sink);
    // Drop any guard a failed pack still holds before the error reaches a
    // caller that may roll back or commit.
    sink.finish()?;
    packed
}

fn pack_range_into<I>(
    pool: &BufferPool,
    tree_id: u16,
    sorted: I,
    fill: f32,
    scratch_dir: &Path,
    last_next: u32,
    sink: &mut PageSink<'_>,
) -> Result<PackedRange>
where I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    let usable = ((PAGE_SIZE - HEADER_LEN) as f32 * fill) as usize;
    let capacity = PAGE_SIZE - HEADER_LEN;

    std::fs::create_dir_all(scratch_dir)?;
    // One number per call, so two packs sharing a scratch directory cannot
    // name the same file. Same discipline as `Store::bulk_load`'s per-call
    // sequence number for the sort directory.
    static PACK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pack = PACK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut seq = 0u64;

    // Level 0: leaves. Their (first_key, page_no) pairs are spilled, not held.
    let mut level = Separators::create(scratch_dir, pack, &mut seq)?;
    let mut cur: Option<(u32, Vec<Vec<u8>>, Vec<u8>, usize)> = None; // (page, recs, first_key, used)
    let mut first_leaf: Option<u32> = None;
    let mut last_leaf: Option<u32> = None;
    let mut rows = 0u64;
    let mut range_min: Option<Vec<u8>> = None;
    let mut range_max: Option<Vec<u8>> = None;

    let flush_leaf = |cur: &mut Option<(u32, Vec<Vec<u8>>, Vec<u8>, usize)>,
                          level: &mut Separators,
                          next_leaf: u32,
                          sink: &mut PageSink<'_>| -> Result<()> {
        if let Some((no, recs, first, _)) = cur.take() {
            let mut scratch = build_scratch(PageKind::Leaf, tree_id, no, &recs)?;
            {
                let mut page = PageMut::reopen(&mut scratch);
                page.set_next_leaf(next_leaf);
                page.finalise(0);
            }
            sink.push(no, &scratch)?;
            level.push(&first, no)?;
        }
        Ok(())
    };

    let mut prev_key: Option<Vec<u8>> = None;
    for item in sorted {
        let (k, v, is_marker) = item?;
        // Refuse duplicates rather than pack them.
        //
        // Two entries with the same key can straddle a leaf boundary, and
        // the separator promoted into the parent then equals a key in the
        // leaf to its left -- so a descent routes past one of them and `get`
        // cannot reach it, while `range` still returns it. A divergence
        // between point lookup and scan is the worst kind of wrong answer:
        // both paths look correct in isolation. Refusing is honest, and a
        // caller who wants last-wins can deduplicate before calling.
        if prev_key.as_deref() == Some(k.as_slice()) {
            // Not `Corrupt { page_no: 0 }` -- page 0 is the superblock, so
            // that would name a real, meaningful page for a condition that
            // has nothing to do with one, and read as structural damage in
            // a log when it is really just an input the caller must
            // deduplicate.
            return Err(Error::DuplicateKey);
        }
        rows = rows.checked_add(1).ok_or(Error::TooLarge)?;
        if range_min.is_none() { range_min = Some(k.clone()); }
        range_max = Some(k.clone());
        prev_key = Some(k.clone());
        let rec = if is_marker {
            let m: [u8; 12] = v.as_slice().try_into().map_err(|_| {
                Error::Io(invalid_scratch("overflow marker scratch value is not exactly 12 bytes"))
            })?;
            crate::btree::enc_leaf_marker(&k, &m)
        } else {
            enc_leaf(&k, &v, pool.compact_cells())
        };
        let need = rec.len() + 4;
        // A record that cannot fit an empty page at all can never be packed,
        // bulk or otherwise -- `BTree::insert` refuses it before it ever
        // touches a page (`kernel/src/btree.rs`); refuse it here too, before
        // any page is allocated for it, rather than letting `insert_slot`
        // discover it deep inside `build_scratch`.
        if need > capacity {
            return Err(Error::TooLarge);
        }
        let fits = matches!(&cur, Some((_, _, _, used)) if used + need <= usable);
        if !fits {
            let no = sink.reserve()?;
            flush_leaf(&mut cur, &mut level, no, sink)?;
            if first_leaf.is_none() { first_leaf = Some(no); }
            last_leaf = Some(no);
            cur = Some((no, Vec::new(), k.clone(), 0));
        }
        if let Some((_, recs, _, used)) = cur.as_mut() { recs.push(rec); *used += need; }
    }
    flush_leaf(&mut cur, &mut level, last_next, sink)?;
    level.seal()?;

    if level.count == 0 {
        let no = sink.reserve()?;
        let scratch = build_scratch(PageKind::Leaf, tree_id, no, &[])?;
        sink.push(no, &scratch)?;
        sink.finish()?;
        return Ok(PackedRange {
            root: no,
            first_leaf: no,
            last_leaf: no,
            rows: 0,
            min: None,
            max: None,
        });
    }

    // Build interior levels the same way until one page remains. Same
    // convention as btree.rs: leftmost child in the header, slot array holds
    // strictly sorted (min_key, child) pairs.
    //
    // The old loop indexed `level[i]` and looked one entry ahead to decide
    // where a page ends. Streaming has no index, so the lookahead becomes an
    // explicit `pending`: the separator that did not fit the page just
    // finished is the first child of the next one. The page boundaries this
    // produces are identical to the indexed version's, which is what keeps
    // the resulting tree byte-for-byte the same.
    while level.count > 1 {
        let mut up = Separators::create(scratch_dir, pack, &mut seq)?;
        {
            let mut r = level.reader()?;
            // Counted against `level.count` once the file is consumed. See
            // `short_level`.
            let mut seen = 0u64;
            let mut pending = std::collections::VecDeque::new();
            if let Some(first) = read_sep(&mut r, level.max_key_len)? {
                seen += 1;
                pending.push_back(first);
            }
            while let Some((first, child0)) = pending.pop_front() {
                let no = sink.reserve()?;
                // Keep the separator beside its encoded form until this page
                // is sealed. If the very last child would strand itself on a
                // one-child page, we may have to move this page's final
                // separator over to become that page's child0.
                let mut recs: Vec<(Vec<u8>, u32, Vec<u8>)> = Vec::new();
                let mut used = 0usize;
                loop {
                    let next = if let Some(queued) = pending.pop_front() {
                        Some(queued)
                    } else {
                        let read = read_sep(&mut r, level.max_key_len)?;
                        if read.is_some() { seen += 1; }
                        read
                    };
                    let Some((k, child)) = next else { break };
                    // Counted where it is READ, not where it is used: the
                    // entry that does not fit is handed to the next page
                    // through `pending` and must not be counted twice.
                    let rec = enc_interior(&k, child);
                    let need = rec.len() + 4;
                    if need > capacity { return Err(Error::TooLarge); }
                    if used + need > usable && !recs.is_empty() {
                        // Never strand the final child in a one-child
                        // interior page. Let the penultimate page consume the
                        // last separator up to physical capacity; its slight
                        // overfill is bounded by the normal 10% reserve.
                        if seen == level.count && used + need <= capacity {
                            used += need;
                            recs.push((k, child, rec));
                            continue;
                        }
                        if seen == level.count {
                            // The reserve is not large enough for the final
                            // separator. Rebalance one separator from this
                            // page: it becomes child0 of the final page, and
                            // the separator just read becomes that page's one
                            // slot. Both pages therefore retain at least two
                            // children. With only one existing slot there is
                            // no valid two-page partition at this level.
                            if recs.len() == 1 { return Err(Error::TooLarge); }
                            let (moved_key, moved_child, _) = recs.pop().unwrap();
                            pending.push_back((moved_key, moved_child));
                            pending.push_back((k, child));
                            break;
                        }
                        pending.push_back((k, child));
                        break;
                    }
                    used += need;
                    recs.push((k, child, rec));
                }
                let encoded: Vec<Vec<u8>> = recs.into_iter().map(|(_, _, rec)| rec).collect();
                let mut scratch = build_scratch(PageKind::Interior, tree_id, no, &encoded)?;
                // `set_child0` and `set_next_leaf` are the same header field
                // (page.rs); set it in the scratch buffer and re-finalise so the
                // page reaches disk complete in its one batched write.
                {
                    let mut p = PageMut::reopen(&mut scratch);
                    p.set_child0(child0);
                    p.finalise(0);
                }
                sink.push(no, &scratch)?;
                up.push(&first, no)?;
            }
            // The outer loop only ends on `pending == None`, which only
            // happens at EOF, so the whole file has been consumed here.
            if seen != level.count { return Err(short_level(seen, level.count)); }
        }
        up.seal()?;
        // Assigning drops the level just consumed, which unlinks its file --
        // the cleanup happens level by level, so a deep tree never has more
        // than two separator files on disk at once.
        level = up;
    }

    // Exactly one entry left: that page is the root. Checked both ways --
    // an empty file, and a file with anything after the root -- so the last
    // level gets the same treatment as every level below it.
    let mut r = level.reader()?;
    let root = match read_sep(&mut r, level.max_key_len)? {
        Some((_, no)) => no,
        None => return Err(short_level(0, level.count)),
    };
    if read_sep(&mut r, level.max_key_len)?.is_some() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the final separator level held more than the one entry it counted",
        )));
    }
    // The root is about to become readable through the ordinary pool and,
    // after graft publication, reachable from committed metadata. Do not let
    // either happen while any candidate pages remain only in this buffer.
    sink.finish()?;
    Ok(PackedRange {
        root,
        first_leaf: first_leaf.expect("a non-empty level has a first leaf"),
        last_leaf: last_leaf.expect("a non-empty level has a last leaf"),
        rows,
        min: range_min,
        max: range_max,
    })
}

/// Pack a complete sorted tree.  Kept as the stable whole-tree primitive used
/// by bulk load and recovery; range grafting extends it through `pack_range`
/// instead of duplicating its page builder.
pub fn pack_tree<I>(pool: &BufferPool, tree_id: u16, sorted: I, fill: f32, scratch_dir: &Path) -> Result<u32>
where I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    Ok(pack_range(pool, tree_id, sorted, fill, scratch_dir, 0)?.root)
}

/// Pack a complete sorted tree through the ORDINARY buffer pool.
///
/// Same bottom-up pack as [`pack_tree`], but every page is an ordinary pooled
/// page, so a page-WAL database logs each one as a normal frame and publishes
/// the whole tree with the caller's commit (`PageSink::Pooled`). There is no
/// root swap, no skipped log and no second durability rule: Law 3 and Law 5
/// hold exactly as they do for a single `insert`, and a crash before the
/// caller's commit leaves nothing reachable.
///
/// This is what a per-index tree is built with: the tree is EMPTY, so there is
/// no standing content to graft beside and no boundary to plan -- the packed
/// root simply becomes the index descriptor's root in the same transaction.
/// Returns `(root, rows)`; an empty stream returns `(0, 0)`, the descriptor's
/// encoding of an empty tree.
pub fn pack_tree_pooled<I>(pool: &BufferPool, tree_id: u16, sorted: I, fill: f32, scratch_dir: &Path)
    -> Result<(u32, u64)>
where I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
    let mut peek = sorted.peekable();
    if peek.peek().is_none() { return Ok((0, 0)); }
    let packed = pack_range_pooled(pool, tree_id, peek, fill, scratch_dir, 0)?;
    Ok((packed.root, packed.rows))
}
