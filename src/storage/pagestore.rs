//! A paged file with a free list — the space manager that removes the need for
//! compaction.
//!
//! # Why this exists
//!
//! Sekejap stores records by appending to one undivided file. Space belonging to
//! updated or deleted records is never reclaimed, so periodically the whole store
//! is rewritten to squeeze it out. That rewrite costs `O(store)` no matter how
//! little changed: measured at 7.6 s per million records, so 6 minutes on a
//! 48-million-record store, charged to whichever ordinary write happens to cross
//! the threshold.
//!
//! SQLite, LMDB and DuckDB do not have that operation at all. They divide the file
//! into fixed-size pages and keep a **free list**: a page belonging to a dead
//! record goes on the list and the next allocation takes it back. Space is
//! reclaimed *continuously*, as it is freed, so there is no batch to schedule and
//! nothing to defer. `VACUUM` exists in SQLite only to shrink the file, and most
//! databases never run it.
//!
//! This is that structure.
//!
//! # The free list lives in the free space
//!
//! Rather than storing a list of free pages somewhere — which would itself need
//! space proportional to the number of free pages, and rewriting — each free page
//! holds the number of the next free page in its first eight bytes. The store then
//! needs to remember only the head:
//!
//! ```text
//!   header.free_head ──► page 7 ──► page 3 ──► page 12 ──► 0 (end)
//! ```
//!
//! Allocation pops the head; freeing pushes onto it. Both are `O(1)` and touch one
//! page. SQLite uses a refinement of this (trunk pages holding many free page
//! numbers each); the plain list is the same idea without the batching.
//!
//! # Durability
//!
//! The header is written on [`sync`](PageStore::sync), not on every allocation —
//! one page write per allocation would defeat the point. A crash therefore loses
//! the *free list*, not any data: pages freed since the last sync stay allocated
//! and are simply never reused. That leaks space; it cannot corrupt anything,
//! because a leaked page is one nothing points at. A scan can reclaim them later.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

const MAGIC: [u8; 8] = *b"SKPAGE\0\0";
const VERSION: u32 = 1;

/// Default page size. Matches SQLite's default and the usual OS page size, so a
/// page read is one page fault and never straddles two.
pub(crate) const DEFAULT_PAGE_SIZE: usize = 4096;

/// Page 0 is the header and is never allocated, so 0 doubles as "no page".
const NO_PAGE: u64 = 0;
const HEADER_PAGE: u64 = 0;

/// A file divided into fixed-size pages, with reclaimed pages reused.
pub(crate) struct PageStore {
    file: File,
    page_size: usize,
    /// One past the highest page ever allocated — where the file grows next.
    high_water: u64,
    /// Head of the free-page chain, or `NO_PAGE`.
    free_head: u64,
    /// How many pages are on the free chain. Bookkeeping only.
    free_count: u64,
    /// Set when the header no longer matches what is on disk.
    dirty: bool,
    /// Two words the owner of the store may use. A B+tree keeps its root page and
    /// entry count here, so reopening needs no scan to find where the tree starts.
    user_a: u64,
    user_b: u64,
}

impl PageStore {
    /// Create a new store, replacing anything already at `path`.
    pub(crate) fn create(path: &Path, page_size: usize) -> io::Result<Self> {
        assert!(page_size >= 64 && page_size.is_power_of_two(),
                "page size must be a power of two and at least 64 bytes");
        let file = OpenOptions::new()
            .read(true).write(true).create(true).truncate(true).open(path)?;
        let mut s = Self {
            file,
            page_size,
            high_water: 1, // page 0 is the header
            free_head: NO_PAGE,
            free_count: 0,
            dirty: true,
            user_a: 0,
            user_b: 0,
        };
        s.file.set_len(page_size as u64)?;
        s.sync()?;
        Ok(s)
    }

    /// Open an existing store. `None` if the file is absent or not one of ours.
    pub(crate) fn open(path: &Path) -> io::Result<Option<Self>> {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut hdr = [0u8; 64];
        if read_exact_at(&file, &mut hdr, 0).is_err() {
            return Ok(None);
        }
        if hdr[0..8] != MAGIC {
            return Ok(None);
        }
        let version = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        if version > VERSION {
            return Ok(None); // written by a newer sekejap — refuse rather than guess
        }
        let page_size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
        if page_size < 64 || !page_size.is_power_of_two() {
            return Ok(None);
        }
        Ok(Some(Self {
            file,
            page_size,
            high_water: u64::from_le_bytes(hdr[16..24].try_into().unwrap()),
            free_head: u64::from_le_bytes(hdr[24..32].try_into().unwrap()),
            free_count: u64::from_le_bytes(hdr[32..40].try_into().unwrap()),
            dirty: false,
            user_a: u64::from_le_bytes(hdr[40..48].try_into().unwrap()),
            user_b: u64::from_le_bytes(hdr[48..56].try_into().unwrap()),
        }))
    }

    pub(crate) fn page_size(&self) -> usize { self.page_size }

    /// Pages that exist in the file, including the header and any on the free list.
    pub(crate) fn page_count(&self) -> u64 { self.high_water }

    /// Pages available for reuse without growing the file.
    pub(crate) fn free_count(&self) -> u64 { self.free_count }

    /// Two words reserved for whatever is built on top of this store.
    pub(crate) fn user_meta(&self) -> (u64, u64) { (self.user_a, self.user_b) }

    pub(crate) fn set_user_meta(&mut self, a: u64, b: u64) {
        self.user_a = a;
        self.user_b = b;
        self.dirty = true;
    }

    /// Take a page — from the free list if there is one, otherwise by growing.
    ///
    /// Reuse is what keeps a steady-state workload from growing the file forever:
    /// under a rolling retention window, the deletions of one day pay for the
    /// insertions of the next.
    pub(crate) fn alloc(&mut self) -> io::Result<u64> {
        if self.free_head != NO_PAGE {
            let page = self.free_head;
            let mut link = [0u8; 8];
            read_exact_at(&self.file, &mut link, page * self.page_size as u64)?;
            self.free_head = u64::from_le_bytes(link);
            self.free_count -= 1;
            self.dirty = true;
            return Ok(page);
        }
        let page = self.high_water;
        self.high_water += 1;
        self.file.set_len(self.high_water * self.page_size as u64)?;
        self.dirty = true;
        Ok(page)
    }

    /// Return a page for reuse. The page's contents become the chain link.
    pub(crate) fn free(&mut self, page: u64) -> io::Result<()> {
        if page == HEADER_PAGE || page >= self.high_water {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("page {page} is not one this store allocated"),
            ));
        }
        write_all_at(&self.file, &self.free_head.to_le_bytes(),
                     page * self.page_size as u64)?;
        self.free_head = page;
        self.free_count += 1;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn read(&self, page: u64, buf: &mut [u8]) -> io::Result<()> {
        if page >= self.high_water || buf.len() > self.page_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "page out of range"));
        }
        read_exact_at(&self.file, buf, page * self.page_size as u64)
    }

    pub(crate) fn write(&mut self, page: u64, buf: &[u8]) -> io::Result<()> {
        if page >= self.high_water || buf.len() > self.page_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "page out of range"));
        }
        write_all_at(&self.file, buf, page * self.page_size as u64)
    }

    /// Persist the header. Until this runs, allocations and frees since the last
    /// call are not durable — see the note on durability at the top of this file.
    pub(crate) fn sync(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut hdr = vec![0u8; self.page_size.min(64)];
        debug_assert!(hdr.len() >= 56, "header must hold the user words");
        hdr[0..8].copy_from_slice(&MAGIC);
        hdr[8..12].copy_from_slice(&VERSION.to_le_bytes());
        hdr[12..16].copy_from_slice(&(self.page_size as u32).to_le_bytes());
        hdr[16..24].copy_from_slice(&self.high_water.to_le_bytes());
        hdr[24..32].copy_from_slice(&self.free_head.to_le_bytes());
        hdr[32..40].copy_from_slice(&self.free_count.to_le_bytes());
        hdr[40..48].copy_from_slice(&self.user_a.to_le_bytes());
        hdr[48..56].copy_from_slice(&self.user_b.to_le_bytes());
        write_all_at(&self.file, &hdr, 0)?;
        self.file.sync_data()?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}
#[cfg(unix)]
fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}
#[cfg(not(unix))]
fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = f;
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)
}
#[cfg(not(unix))]
fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = f;
    f.seek(SeekFrom::Start(off))?;
    f.write_all(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> PageStore {
        PageStore::create(&dir.path().join("pages.bin"), DEFAULT_PAGE_SIZE).unwrap()
    }

    #[test]
    fn fresh_allocations_grow_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        assert_eq!(s.page_count(), 1, "page 0 is the header");
        let pages: Vec<u64> = (0..5).map(|_| s.alloc().unwrap()).collect();
        assert_eq!(pages, vec![1, 2, 3, 4, 5], "fresh pages come in order");
        assert_eq!(s.free_count(), 0);
        assert_eq!(s.page_count(), 6);
    }

    /// The property the whole design rests on: freed space comes back without the
    /// file growing. This is what replaces compaction.
    #[test]
    fn freed_pages_are_reused_instead_of_growing_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let pages: Vec<u64> = (0..10).map(|_| s.alloc().unwrap()).collect();
        let high_water = s.page_count();

        for &p in &pages {
            s.free(p).unwrap();
        }
        assert_eq!(s.free_count(), 10);

        // Ten allocations must all be satisfied from the free list.
        let reused: Vec<u64> = (0..10).map(|_| s.alloc().unwrap()).collect();
        assert_eq!(s.page_count(), high_water, "the file grew despite free pages");
        assert_eq!(s.free_count(), 0);

        let mut a = pages.clone();
        let mut b = reused.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b, "reused pages are exactly the ones freed");
    }

    #[test]
    fn a_steady_workload_does_not_grow_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let live: Vec<u64> = (0..100).map(|_| s.alloc().unwrap()).collect();
        let settled = s.page_count();

        // Churn: free one, allocate one, a thousand times over.
        let mut live = live;
        for i in 0..1000 {
            let victim = live.remove(i % live.len());
            s.free(victim).unwrap();
            live.push(s.alloc().unwrap());
        }
        assert_eq!(s.page_count(), settled,
                   "a workload that frees as fast as it allocates must not grow the file");
    }

    /// The rolling-retention case, in miniature: insert a day, expire a day,
    /// repeat. The file must reach a plateau and stay there — that plateau is what
    /// a database with no compaction looks like under sustained churn.
    #[test]
    fn a_rolling_retention_window_reaches_a_plateau() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let per_day = 2_000usize;
        let window = 7usize;

        let mut days: Vec<Vec<u64>> = Vec::new();
        let mut plateau = None;
        for day in 0..30 {
            let today: Vec<u64> = (0..per_day).map(|_| s.alloc().unwrap()).collect();
            days.push(today);
            if days.len() > window {
                for p in days.remove(0) {
                    s.free(p).unwrap();
                }
            }
            // Once the window is full the file must stop growing entirely.
            if day == window {
                plateau = Some(s.page_count());
            } else if let Some(p) = plateau {
                assert_eq!(s.page_count(), p,
                           "file grew on day {day} — expiring data is not paying for new data");
            }
        }
        let live = window * per_day;
        assert!(s.page_count() as usize <= live + per_day + 2,
                "file holds {} pages for {live} live records", s.page_count());
    }

    #[test]
    fn page_contents_survive_a_reopen_and_so_does_the_free_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pages.bin");
        let (kept, freed);
        {
            let mut s = PageStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            kept = s.alloc().unwrap();
            freed = s.alloc().unwrap();
            let mut buf = vec![0u8; DEFAULT_PAGE_SIZE];
            buf[..5].copy_from_slice(b"hello");
            s.write(kept, &buf).unwrap();
            s.free(freed).unwrap();
            s.sync().unwrap();
        }
        let mut s = PageStore::open(&path).unwrap().expect("store should reopen");
        assert_eq!(s.free_count(), 1, "the free list did not survive");

        let mut buf = vec![0u8; DEFAULT_PAGE_SIZE];
        s.read(kept, &mut buf).unwrap();
        assert_eq!(&buf[..5], b"hello", "page contents did not survive");

        assert_eq!(s.alloc().unwrap(), freed, "the freed page was not reused after reopen");
    }

    #[test]
    fn the_header_page_can_never_be_freed() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        assert!(s.free(HEADER_PAGE).is_err(), "freeing the header must be refused");
        assert!(s.free(9999).is_err(), "freeing an unallocated page must be refused");
    }

    #[test]
    fn a_foreign_file_is_declined_rather_than_misread() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("not-ours.bin");
        std::fs::write(&path, vec![0xABu8; 4096]).unwrap();
        assert!(PageStore::open(&path).unwrap().is_none());
        assert!(PageStore::open(&dir.path().join("absent.bin")).unwrap().is_none());
    }
}
