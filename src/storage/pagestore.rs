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
    /// A read-only mapping of the pages written so far.
    ///
    /// Reads came off the descriptor with `pread`, one syscall per page. That is
    /// fine in isolation and ruinous in aggregate: the structures built on this
    /// store answer a lookup in three or four page reads, so a query touching
    /// 200 000 nodes made close to a million syscalls where the mmap'd layout it
    /// replaces made none. The mapping turns each of those into a memory read.
    ///
    /// `MAP_SHARED`, because this process writes the same file — under a private
    /// mapping a write through the descriptor may or may not be visible, and
    /// "may or may not" means serving a stale page with nothing to show for it.
    ///
    /// Covers `mapped_pages` from the start of the file; anything allocated since
    /// the last remap falls back to reading from the descriptor, which is always
    /// correct and only slower.
    map: Option<super::mmap::MmapView>,
    mapped_pages: u64,
}

impl PageStore {
    /// Create a new store at `path`, replacing an empty file or one of ours.
    ///
    /// **Refuses to replace a file it does not recognise.** This used to truncate
    /// whatever was there, and the pattern `open(path)? else create(path)?` is the
    /// only way anything opens a store — so pointing it at a file written in some
    /// other format silently emptied it. Turning on `paged_payloads` for a database
    /// whose `payloads.bin` was written flat took it from 23 216 bytes to 4 096
    /// before a single query ran, and every row in the database with it.
    ///
    /// A store cannot always tell what it is being handed. It can always tell that
    /// it was handed something, and refuse: nothing that can be wrong about what
    /// exists may delete it.
    pub(crate) fn create(path: &Path, page_size: usize) -> io::Result<Self> {
        assert!(page_size >= 64 && page_size.is_power_of_two(),
                "page size must be a power of two and at least 64 bytes");
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > 0 && !Self::has_our_magic(path) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "sekejap: {} already holds {} bytes that were not written as \
                         a page store, so creating one here would destroy them. This \
                         usually means a paged storage mode was switched on for a \
                         database written without it, which needs a migration rather \
                         than a reopen.",
                        path.display(), meta.len(),
                    ),
                ));
            }
        }
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
            map: None,
            mapped_pages: 0,
        };
        s.file.set_len(page_size as u64)?;
        s.sync()?;
        Ok(s)
    }

    /// Whether `path` begins with this format's magic — i.e. we wrote it.
    fn has_our_magic(path: &Path) -> bool {
        let Ok(file) = OpenOptions::new().read(true).open(path) else { return false };
        let mut magic = [0u8; 8];
        read_exact_at(&file, &mut magic, 0).is_ok() && magic == MAGIC
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
        let mut store = Self {
            file,
            page_size,
            high_water: u64::from_le_bytes(hdr[16..24].try_into().unwrap()),
            free_head: u64::from_le_bytes(hdr[24..32].try_into().unwrap()),
            free_count: u64::from_le_bytes(hdr[32..40].try_into().unwrap()),
            dirty: false,
            user_a: u64::from_le_bytes(hdr[40..48].try_into().unwrap()),
            user_b: u64::from_le_bytes(hdr[48..56].try_into().unwrap()),
            map: None,
            mapped_pages: 0,
        };
        store.remap();
        Ok(Some(store))
    }

    /// Point the read mapping at everything currently in the file.
    ///
    /// Cheap — one `mmap` call, no I/O, since pages load lazily on first touch. It
    /// runs when the store is opened and whenever it is synced, so a store that is
    /// being written falls back to the descriptor only for pages allocated since
    /// the last sync.
    ///
    /// A failure is not an error: the mapping is an optimisation, and reading from
    /// the descriptor is always correct.
    fn remap(&mut self) {
        // **Only as far as the file actually goes.** The header says how many pages
        // were allocated; it does not say how many the file still contains. Mapping
        // past the end of a file is permitted and touching what you mapped is
        // SIGBUS — not an error a caller can handle, not a `None` to fall back on,
        // but the process dying. A file truncated by damage, a full disk or a
        // half-finished copy is exactly the case where the store most needs to
        // return an error rather than take the process with it.
        //
        // Clamping means pages past the real end fall to the descriptor, which
        // reports a short read as the error it is.
        let want = self.high_water * self.page_size as u64;
        let on_disk = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        let bytes = want.min(on_disk);
        self.map = usize::try_from(bytes).ok()
            .and_then(|n| super::mmap::MmapView::try_new_shared(&self.file, n));
        self.mapped_pages = match &self.map {
            Some(_) => bytes / self.page_size as u64,
            None => 0,
        };
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
            // The free list lives *in* the free pages, so a damaged free page is a
            // damaged list, and following one is worse than losing it:
            //
            // - a page that points at itself is a cycle, and allocation hands the
            //   same page out over and over — two records both owning it, which is
            //   silent corruption rather than an error
            // - a page past the end of the file makes every allocation fail from
            //   then on, because the head never advances
            // - page 0 is the header, and handing it out lets a record overwrite
            //   the store's own metadata
            //
            // So the list is abandoned rather than followed. That leaks the pages
            // still on it, which is precisely the failure this design accepts —
            // a leaked page is one nothing points at.
            if page == HEADER_PAGE || page >= self.high_water {
                self.free_head = NO_PAGE;
                self.free_count = 0;
                self.dirty = true;
            } else {
                let mut link = [0u8; 8];
                let next = match read_exact_at(&self.file, &mut link,
                                               page * self.page_size as u64) {
                    Ok(()) => u64::from_le_bytes(link),
                    // An unreadable free page ends the list; the pages behind it
                    // are unreachable either way.
                    Err(_) => NO_PAGE,
                };
                // `NO_PAGE` is the ordinary terminator. Anything else that is not a
                // page of this store, or is this page again, is damage.
                self.free_head = if next == NO_PAGE || next >= self.high_water || next == page {
                    NO_PAGE
                } else {
                    next
                };
                self.free_count = self.free_count.saturating_sub(1);
                self.dirty = true;
                return Ok(page);
            }
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

    /// A page borrowed straight out of the mapping, without copying it.
    ///
    /// `read` copies a page into a caller's buffer, and every structure built on
    /// this store was allocating that buffer per read — three heap allocations and
    /// three 4 KB copies for one B+tree descent, six for a graph hop, on the hot
    /// path of every paged operation.
    ///
    /// `None` when the page is not in the mapping: past what was mapped at the last
    /// remap, or the mapping failed. Callers fall back to `read`, which is always
    /// correct and only slower.
    pub(crate) fn page_slice(&self, page: u64) -> Option<&[u8]> {
        if page >= self.high_water || page >= self.mapped_pages { return None }
        let at = usize::try_from(page * self.page_size as u64).ok()?;
        self.map.as_ref()?.slice(at, self.page_size)
    }

    pub(crate) fn read(&self, page: u64, buf: &mut [u8]) -> io::Result<()> {
        if page >= self.high_water || buf.len() > self.page_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "page out of range"));
        }
        let at = page * self.page_size as u64;
        // From the mapping when it reaches this page: a copy out of the page cache
        // instead of a syscall into it. Pages allocated since the last remap are
        // not covered, so those take the descriptor — correct either way.
        if page < self.mapped_pages {
            if let Some(src) = self.map.as_ref()
                .and_then(|m| usize::try_from(at).ok().and_then(|o| m.slice(o, buf.len())))
            {
                buf.copy_from_slice(src);
                return Ok(());
            }
        }
        read_exact_at(&self.file, buf, at)
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
        // Extend the read mapping over everything allocated since the last sync,
        // so those pages stop taking the descriptor.
        if self.mapped_pages != self.high_water {
            self.remap();
        }
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

    /// Reads come from a mapping and writes go through the descriptor, so the two
    /// have to agree — and when they do not, nothing says so. A stale page is
    /// returned as if it were current.
    ///
    /// This is why the mapping is `MAP_SHARED`. Under `MAP_PRIVATE` the visibility
    /// of a write through the descriptor is unspecified by POSIX, and "unspecified"
    /// here means a store that silently serves data it has already overwritten.
    #[test]
    fn a_write_is_visible_through_the_read_mapping() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pages.bin");
        let mut s = PageStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();

        let pages: Vec<u64> = (0..8).map(|_| s.alloc().unwrap()).collect();
        for (i, &p) in pages.iter().enumerate() {
            s.write(p, &vec![i as u8; DEFAULT_PAGE_SIZE]).unwrap();
        }
        // Sync so the mapping covers them, then read once so the pages are resident
        // in it — a mapping that had never been touched could hide the problem.
        s.sync().unwrap();
        let mut buf = vec![0u8; DEFAULT_PAGE_SIZE];
        for (i, &p) in pages.iter().enumerate() {
            s.read(p, &mut buf).unwrap();
            assert_eq!(buf[0], i as u8, "page {p} came back wrong before any overwrite");
        }

        // Now overwrite through the descriptor and read straight back through the
        // mapping, with no sync in between.
        for (i, &p) in pages.iter().enumerate() {
            s.write(p, &vec![0xF0 | i as u8; DEFAULT_PAGE_SIZE]).unwrap();
        }
        for (i, &p) in pages.iter().enumerate() {
            s.read(p, &mut buf).unwrap();
            assert_eq!(buf[0], 0xF0 | i as u8,
                       "page {p} read back its old contents — the mapping is not \
                        coherent with writes, so this store serves stale data");
            assert!(buf.iter().all(|&b| b == 0xF0 | i as u8), "page {p} partially stale");
        }

        // And a page allocated after the last remap, which the mapping does not
        // cover, must still read correctly from the descriptor.
        let fresh = s.alloc().unwrap();
        s.write(fresh, &vec![0x5A; DEFAULT_PAGE_SIZE]).unwrap();
        s.read(fresh, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x5A),
                "a page beyond the mapping did not fall back to the descriptor");
    }

    /// **The Law 3 test.** Creating a store must never destroy a file it did not
    /// write.
    ///
    /// `open(path)? else create(path)?` is how every store in this codebase is
    /// opened, and `create` truncated unconditionally. So pointing any of them at a
    /// file in another format emptied it — measured on a real database, turning on
    /// `paged_payloads` took `payloads.bin` from 23 216 bytes to 4 096 before a
    /// single query ran, taking every row with it. Nothing reported anything; the
    /// first symptom was an out-of-bounds read some operations later.
    #[test]
    fn creating_a_store_refuses_to_destroy_a_file_it_did_not_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("someone_elses.bin");
        let contents = b"this is not a page store, it is somebody's data".to_vec();
        std::fs::write(&path, &contents).unwrap();

        let Err(err) = PageStore::create(&path, DEFAULT_PAGE_SIZE) else {
            panic!("creating a store over a foreign file was allowed");
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), contents,
                   "the file was modified even though the call failed");

        // An empty file is not somebody's data — creating over it is the ordinary
        // path and must still work.
        let fresh = dir.path().join("fresh.bin");
        std::fs::write(&fresh, b"").unwrap();
        assert!(PageStore::create(&fresh, DEFAULT_PAGE_SIZE).is_ok(),
                "creating a store over an empty file was refused");

        // And one of ours may be replaced: that is what create is for.
        let ours = dir.path().join("ours.bin");
        {
            let mut s = PageStore::create(&ours, DEFAULT_PAGE_SIZE).unwrap();
            for _ in 0..4 { s.alloc().unwrap(); }
            s.sync().unwrap();
        }
        let Ok(s) = PageStore::create(&ours, DEFAULT_PAGE_SIZE) else {
            panic!("creating over a store we wrote was refused");
        };
        assert_eq!(s.page_count(), 1, "recreating did not start from empty");
    }

    /// **The SIGBUS test.** A file shorter than its header claims must not take the
    /// process down.
    ///
    /// `remap` mapped `high_water * page_size` bytes, which is what the header says
    /// was allocated, not what the file still holds. Mapping past the end of a file
    /// is allowed; *reading* what you mapped there raises SIGBUS, which is not an
    /// error a caller can catch, not a `None` to fall back on — the process dies.
    /// A file truncated by damage, a full disk, or a half-finished copy is exactly
    /// where a store most needs to return an error instead.
    #[test]
    fn a_file_shorter_than_its_header_claims_errors_rather_than_dying() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pages.bin");
        let pages: Vec<u64>;
        {
            let mut s = PageStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            pages = (0..12).map(|_| s.alloc().unwrap()).collect();
            for (i, &p) in pages.iter().enumerate() {
                s.write(p, &vec![i as u8 + 1; DEFAULT_PAGE_SIZE]).unwrap();
            }
            s.sync().unwrap();
        }

        // Cut the file to a third. The header still says thirteen pages exist.
        let full = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap()
            .set_len(full / 3).unwrap();

        let s = PageStore::open(&path).unwrap().expect("a truncated store should still open");
        assert_eq!(s.page_count(), 13, "the header should still claim every page");

        // Every page: the ones that survived read correctly, the ones that did not
        // return an error. Neither may kill the process.
        let mut buf = vec![0u8; DEFAULT_PAGE_SIZE];
        let mut ok = 0;
        for (i, &p) in pages.iter().enumerate() {
            match s.read(p, &mut buf) {
                Ok(()) => {
                    assert_eq!(buf[0], i as u8 + 1,
                               "page {p} survived truncation but came back as another page");
                    ok += 1;
                }
                Err(_) => {} // past the end of what is left — the right answer
            }
        }
        assert!(ok > 0, "a store cut to a third returned nothing at all");
        assert!(ok < pages.len(), "a store cut to a third returned every page, which cannot be");
    }

    /// A free list that points somewhere impossible must not be followed there.
    ///
    /// The list lives *in* the free pages — each holds the next one's number in its
    /// first eight bytes — so a damaged free page is a damaged list. Three shapes
    /// matter: a page that points at itself (a cycle, so allocation never ends), a
    /// page that points past the end of the file, and one that points at the header
    /// page, which would hand out page 0 and let a record overwrite the store's own
    /// metadata.
    #[test]
    fn a_corrupt_free_list_is_not_followed_off_the_end() {
        use std::io::{Seek, SeekFrom, Write};
        for (name, bad) in [
            ("points at itself", None),       // filled in below, needs the page number
            ("points past the end", Some(9_999_999u64)),
            ("points at the header", Some(0u64)),
            ("points at a huge number", Some(u64::MAX)),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("pages.bin");
            let mut s = PageStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
            let pages: Vec<u64> = (0..8).map(|_| s.alloc().unwrap()).collect();
            for &p in &pages { s.write(p, &vec![7u8; DEFAULT_PAGE_SIZE]).unwrap(); }
            // Free three, so there is a list to corrupt.
            let freed = [pages[1], pages[3], pages[5]];
            for &p in &freed { s.free(p).unwrap() }
            s.sync().unwrap();
            let head = freed[2]; // most recently freed is the head
            drop(s);

            let target = bad.unwrap_or(head); // "points at itself"
            {
                let mut fh = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                fh.seek(SeekFrom::Start(head * DEFAULT_PAGE_SIZE as u64)).unwrap();
                fh.write_all(&target.to_le_bytes()).unwrap();
            }

            let mut s = PageStore::open(&path).unwrap().expect("should still open");
            // Allocating repeatedly must terminate, and must never hand out the
            // header page or one past the end of the file.
            for i in 0..20 {
                let p = s.alloc().unwrap();
                assert_ne!(p, HEADER_PAGE,
                           "{name}: allocation {i} handed out the header page, which a \
                            record would then overwrite");
                assert!(p < s.page_count(),
                        "{name}: allocation {i} handed out page {p} with only {} pages",
                        s.page_count());
                // And it must be writable, i.e. really part of the file.
                s.write(p, &vec![1u8; DEFAULT_PAGE_SIZE]).unwrap();
            }
            // The store still reads the pages that were never freed.
            let mut buf = vec![0u8; DEFAULT_PAGE_SIZE];
            for &p in [pages[0], pages[2], pages[4]].iter() {
                s.read(p, &mut buf).unwrap();
                assert_eq!(buf[0], 7, "{name}: a live page was clobbered by the bad free list");
            }
        }
    }

    /// A header claiming a page size the file cannot have must be declined.
    ///
    /// Page size drives every offset in the store, so a wrong one does not read
    /// wrong data occasionally — it reads the wrong *bytes* for everything.
    #[test]
    fn an_impossible_header_is_declined() {
        use std::io::{Seek, SeekFrom, Write};
        for (name, offset, value) in [
            ("page size zero",        12u64, 0u32),
            ("page size not a power of two", 12, 3000),
            ("page size below the minimum",  12, 8),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("pages.bin");
            {
                let mut s = PageStore::create(&path, DEFAULT_PAGE_SIZE).unwrap();
                let p = s.alloc().unwrap();
                s.write(p, &vec![3u8; DEFAULT_PAGE_SIZE]).unwrap();
                s.sync().unwrap();
            }
            {
                let mut fh = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                fh.seek(SeekFrom::Start(offset)).unwrap();
                fh.write_all(&value.to_le_bytes()).unwrap();
            }
            assert!(PageStore::open(&path).unwrap().is_none(),
                    "{name}: a header that cannot describe a real store was accepted");
        }
    }
}
