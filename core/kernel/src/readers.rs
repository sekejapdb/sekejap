//! 2n: the reader table -- how the writer learns the oldest generation any
//! live snapshot reader still needs, so page recycling never pulls a page
//! out from under one.
//!
//! LMDB's shape, flock-based so it cannot go stale: each snapshot reader
//! creates `readers/r-<pid>-<n>` holding its pinned generation and KEEPS an
//! exclusive flock on it for the snapshot's lifetime. Close or crash, the
//! kernel drops the lock with the fd. The writer, when it wants the oldest,
//! walks the directory: a file whose lock it can grab is a dead reader's
//! leavings (unlinked on the spot); a file whose lock is busy is a live
//! reader. Its generation is stored in two identical framed copies, each with
//! magic, version and CRC. Any malformed copy or disagreement means generation
//! zero: ambiguity leaks space but cannot recycle a live snapshot's pages.
//!
//! Failure posture (the 2n risk ladder): an unreadable directory or file
//! means "assume a reader at generation 0" -- recycling stops, the file
//! grows like it always did, nothing can be wrongly reused.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::Result;

const SLOT_MAGIC: [u8; 8] = *b"SEKREAD\0";
const SLOT_VERSION: u16 = 1;
const FRAME_LEN: usize = 24;
const SLOT_LEN: usize = FRAME_LEN * 2;

fn encode_frame(generation: u64) -> [u8; FRAME_LEN] {
    let mut frame = [0u8; FRAME_LEN];
    frame[0..8].copy_from_slice(&SLOT_MAGIC);
    frame[8..10].copy_from_slice(&SLOT_VERSION.to_le_bytes());
    frame[12..20].copy_from_slice(&generation.to_le_bytes());
    let crc = crc32c::crc32c(&frame[..20]);
    frame[20..24].copy_from_slice(&crc.to_le_bytes());
    frame
}

fn decode_slot(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != SLOT_LEN || bytes[..FRAME_LEN] != bytes[FRAME_LEN..] {
        return None;
    }
    let frame = &bytes[..FRAME_LEN];
    if frame[..8] != SLOT_MAGIC
        || u16::from_le_bytes(frame[8..10].try_into().ok()?) != SLOT_VERSION
        || frame[10..12] != [0, 0]
        || u32::from_le_bytes(frame[20..24].try_into().ok()?) != crc32c::crc32c(&frame[..20])
    {
        return None;
    }
    Some(u64::from_le_bytes(frame[12..20].try_into().ok()?))
}

fn readers_dir(db: &Path) -> PathBuf { db.join("readers") }

/// Held by a snapshot reader for its lifetime; the registration disappears
/// (unlink + lock release) on drop, and the LOCK disappears even on crash.
pub struct ReaderSlot {
    path: PathBuf,
    persistent: bool,
    _file: std::fs::File, // holds the flock
}

impl ReaderSlot {
    /// Register a reader pinned at `gen`. Failure to register is returned as
    /// an error: an unregistered reader is exactly the unsafe state.
    pub fn register(db: &Path, gen: u64) -> Result<ReaderSlot> {
        Self::create(db, gen, true)
    }

    /// Install the conservative live lock before snapshot metadata is read.
    /// The bytes need not be directory-durable: while this process lives,
    /// completed writes are visible to the writer; if it crashes, the lock
    /// disappears and the stale file is swept. The final generation update
    /// below supplies the one existing fsync, avoiding another snapshot-open
    /// barrier solely to close the race.
    pub(crate) fn reserve(db: &Path) -> Result<ReaderSlot> {
        Self::create(db, 0, false)
    }

    fn create(db: &Path, gen: u64, sync: bool) -> Result<ReaderSlot> {
        if let Some(limits) = crate::limits::read(db)? {
            return Self::create_fixed(db, gen, limits.readers);
        }
        Self::create_with_after_file(db, gen, sync, || {})
    }

    fn create_with_after_file(db: &Path, gen: u64, sync: bool, after_file: impl FnOnce()) -> Result<ReaderSlot> {
        let dir = readers_dir(db);
        std::fs::create_dir_all(&dir)?;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // Publish only an initialized, already-locked inode. A visible unlocked
        // slot can be swept by a writer before its creator obtains the lock.
        let mut candidate = tempfile::Builder::new().prefix(".reader-pending-").tempfile_in(db)?;
        after_file();
        if !crate::io::try_lock_exclusive(candidate.as_file())? {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock,
                "reader slot file unexpectedly locked").into());
        }
        let frame = encode_frame(gen);
        candidate.write_all(&frame)?;
        candidate.write_all(&frame)?;
        if sync { candidate.as_file().sync_all()?; }
        loop {
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = dir.join(format!("r-{}-{}", std::process::id(), n));
            match candidate.persist_noclobber(&path) {
                Ok(file) => return Ok(ReaderSlot { path, _file: file, persistent: false }),
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => candidate = e.file,
                Err(e) => return Err(e.error.into()),
            }
        }
    }

    fn create_fixed(db: &Path, generation: u64, count: u32) -> Result<Self> {
        let dir = readers_dir(db);
        std::fs::create_dir_all(&dir)?;
        // Never unlink fixed slots: all contenders must lock the same inode.
        // A writer ignores unlocked slots; a locked, incomplete frame pins 0.
        for n in 0..count {
            let path = dir.join(format!("fixed-{n}"));
            let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
            if !crate::io::try_lock_exclusive(&file)? { continue; }
            let mut slot = Self { path, _file: file, persistent: true };
            slot.set_generation(generation)?;
            return Ok(slot);
        }
        Err(crate::Error::ResourceLimit("snapshot reader slots full"))
    }

    /// Replace a conservative generation-zero reservation with the generation
    /// selected from metadata. While the two framed copies are being changed,
    /// a writer sees either 0, the final generation, or a mismatch that also
    /// decodes as 0. Every intermediate state therefore stops recycling.
    pub fn set_generation(&mut self, generation: u64) -> Result<()> {
        self._file.rewind()?;
        let frame = encode_frame(generation);
        self._file.write_all(&frame)?;
        self._file.write_all(&frame)?;
        self._file.set_len(SLOT_LEN as u64)?;
        self._file.sync_all()?;
        Ok(())
    }
}

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        if !self.persistent { let _ = std::fs::remove_file(&self.path); }
        // Released by unlock, not by the close that follows: a child process
        // may hold a copy of this descriptor for an instant (`io::Locked`).
        let _ = crate::io::unlock(&self._file);
    }
}

/// The oldest generation any live reader pins, or `u64::MAX` when none.
/// Dead readers' files are swept as they are met. Any ambiguity -- a file
/// that cannot be opened or parsed while its lock is busy -- reports
/// generation 0: recycling halts rather than guesses.
pub fn oldest_live_reader(db: &Path) -> u64 {
    live_generations(db).map_or(0, |g| g.first().copied().unwrap_or(u64::MAX))
}

/// A complete sorted set is needed to distinguish versions actually visible
/// to readers from intermediate versions. Any ambiguity disables promotion.
pub(crate) fn live_generations(db: &Path) -> Option<Vec<u64>> {
    let dir = readers_dir(db);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None, // unreadable table: assume the oldest possible reader
    };
    let mut generations = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { return None };
        let path = entry.path();
        let f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        match crate::io::try_lock_exclusive(&f) {
            Ok(true) => {
                // lock acquired: the registering process is gone -- stale file
                if !entry.file_name().to_string_lossy().starts_with("fixed-") {
                    let _ = std::fs::remove_file(&path);
                }
                let _ = crate::io::unlock(&f);
                continue;
            }
            Ok(false) => {
                // busy lock: live reader -- read its pinned generation
                let mut f2 = f;
                let mut buf = [0u8; SLOT_LEN];
                if f2.read_exact(&mut buf).is_err() { return None; }
                let mut extra = [0u8; 1];
                match f2.read(&mut extra) {
                    Ok(0) => {}
                    Ok(_) | Err(_) => return None,
                }
                let Some(generation) = decode_slot(&buf) else { return None };
                if generation == 0 { return None; }
                if generations.len() >= 4096 { return None; }
                generations.push(generation);
            }
            Err(_) => return None, // ambiguous lock state: halt recycling
        }
    }
    generations.sort_unstable();
    generations.dedup();
    Some(generations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writer_sweep_during_creation_cannot_hide_a_live_reader() {
        let d = tempfile::TempDir::new().unwrap();
        let _reader = ReaderSlot::create_with_after_file(d.path(), 41, true, || {
            assert_eq!(oldest_live_reader(d.path()), u64::MAX);
        }).unwrap();
        assert_eq!(oldest_live_reader(d.path()), 41,
            "registration returned success but writer cannot see the reader");
    }

    #[test]
    fn a_dropped_slot_disappears_and_a_live_one_pins() {
        let d = tempfile::TempDir::new().unwrap();
        assert_eq!(oldest_live_reader(d.path()), u64::MAX, "empty table: no pin");
        let s1 = ReaderSlot::register(d.path(), 41).unwrap();
        let _s2 = ReaderSlot::register(d.path(), 44).unwrap();
        assert_eq!(oldest_live_reader(d.path()), 41);
        drop(s1);
        assert_eq!(oldest_live_reader(d.path()), 44);
    }

    #[test]
    fn a_crashed_readers_file_is_swept_not_trusted() {
        // simulate the crash leavings: a registration file with NO live lock
        let d = tempfile::TempDir::new().unwrap();
        let dir = readers_dir(d.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("r-99999-0"), 7u64.to_le_bytes()).unwrap();
        assert_eq!(oldest_live_reader(d.path()), u64::MAX,
                   "an unlocked file is a dead reader, not a pin");
        assert!(!dir.join("r-99999-0").exists(), "and it is swept");
    }

    #[test]
    fn a_flipped_byte_in_a_live_slot_pins_generation_zero() {
        let d = tempfile::TempDir::new().unwrap();
        let _slot = ReaderSlot::register(d.path(), 41).unwrap();
        let dir = readers_dir(d.path());
        let path = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(
            oldest_live_reader(d.path()),
            0,
            "an unverifiable live reader must stop recycling, never advance its generation",
        );
    }
}
