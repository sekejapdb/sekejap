//! All operating-system contact lives here. Everything above this module is
//! arithmetic over byte arrays and is platform-neutral by construction.
//!
//! SACRIFICE (Law 4): unbuffered I/O requires offset, length and buffer address
//! to be page-aligned, and forgoes kernel readahead. Bought: the kernel does not
//! keep a second copy of every page, so the cgroup charges only memory we chose.

use crate::page::PAGE_SIZE;
use crate::{Error, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(not(unix))]
type FileIdentity = ();

#[cfg(unix)]
fn file_identity(file: &File) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
#[cfg(not(unix))]
fn file_identity(_file: &File) -> std::io::Result<FileIdentity> { Ok(()) }

fn writer_paths() -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, FileIdentity>> {
    static PATHS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, FileIdentity>>,
    > = std::sync::OnceLock::new();
    PATHS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn writer_owned_by_this_process(path: &Path) -> bool {
    let Ok(path) = std::fs::canonicalize(path) else { return false };
    let paths = writer_paths().lock().unwrap();
    let Some(owned) = paths.get(&path) else { return false };
    let Ok(file) = File::open(&path) else { return false };
    file_identity(&file).is_ok_and(|current| current == *owned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode { Direct, Buffered }

/// Which durability barrier a caller wants. Named rather than boolean,
/// because `true` does not say what was promised.
///
/// There is deliberately no generic `sync()` on `FileIo`. One existed
/// briefly and called `File::sync_data`, which issues `F_FULLFSYNC` on
/// macOS and `fdatasync` on Linux -- the same call meaning two different
/// promises depending on the platform, which is exactly the ambiguity
/// `sync_data`/`sync_full` exist to remove. Leaving a `sync()` beside them
/// preserves the bug in the one place nobody looks: the buffer pool's own
/// checkpoint flush went through it, ungoverned by any `SyncMode`, even
/// after `Store::commit` stopped being ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barrier { None, Data, Full }

/// One file's I/O, counted. Per FILE: a global cursor interleaves the data file
/// and the WAL, counting every alternation as a seek and summing both files'
/// bytes as one -- both measured wrong before this existed.
#[derive(Debug, Default)]
pub struct IoStats {
    pub writes: std::sync::atomic::AtomicU64,
    pub write_bytes: std::sync::atomic::AtomicU64,
    pub reads: std::sync::atomic::AtomicU64,
}

impl IoStats {
    /// Snapshot and reset.
    pub fn take(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.writes.swap(0, Relaxed), self.write_bytes.swap(0, Relaxed), self.reads.swap(0, Relaxed))
    }
}

pub trait FileIo: Send + Sync {
    /// Experimental transactional allocator; ordinary files retain pool reuse.
    fn manages_free_pages(&self) -> bool { false }
    fn pop_free_page(&self) -> Result<Option<u32>> { Ok(None) }
    fn push_free_page(&self, _page: u32) -> Result<()> { unreachable!() }
    /// This file's counters, if it keeps any. Defaulted so test doubles need no change.
    fn stats(&self) -> Option<&IoStats> { None }

    /// True when this file demands page-aligned offsets, lengths and buffers.
    /// The WAL is byte-addressed and always opens Buffered, so it never does.
    fn requires_alignment(&self) -> bool;
    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()>;
    fn write_at(&self, buf: &[u8], off: u64) -> Result<()>;
    /// A data barrier that does NOT force the drive's own write cache: the
    /// write is durable against an OS crash, not necessarily against a loss
    /// of power to the drive. `fdatasync` on Linux; plain `fsync` on macOS.
    ///
    /// Deliberately not `std::fs::File::sync_data`: libstd's implementation
    /// calls `fcntl(F_FULLFSYNC)` on macOS (a safety choice in std, not a
    /// bug), which is exactly the strong, ~65x-costlier barrier `sync_full`
    /// exists to name separately. Going through it here would make `Normal`
    /// and `Full` issue the identical primitive on macOS while differing on
    /// Linux -- the SyncMode label would say one thing and the hardware would
    /// hear another, and differently on different platforms.
    fn sync_data(&self) -> Result<()>;
    /// The strongest barrier this platform can issue: durable even against a
    /// loss of power to the drive. `fcntl(F_FULLFSYNC)` on macOS (roughly 65x
    /// the cost of `sync_data` on the same hardware); `File::sync_all`
    /// (ordinary `fsync`) elsewhere.
    fn sync_full(&self) -> Result<()>;
    /// The exact primitive `sync_full` issues on this platform, so a
    /// measurement can state what it did rather than imply it.
    fn sync_full_primitive(&self) -> &'static str;
    fn sync_dir(&self) -> Result<()>;
    fn len(&self) -> Result<u64>;
    fn set_len(&self, n: u64) -> Result<()>;
}

struct PosixFile {
    f: File,
    #[cfg(unix)]
    dir: File,
    mode: IoMode,
    stats: IoStats,
    /// Present only on the fd that owns this process's writer lock.
    writer_path: Option<std::path::PathBuf>,
    writer_identity: Option<FileIdentity>,
}

impl Drop for PosixFile {
    fn drop(&mut self) {
        if let (Some(path), Some(identity)) =
            (self.writer_path.take(), self.writer_identity.take())
        {
            let mut paths = writer_paths().lock().unwrap();
            if paths.get(&path) == Some(&identity) { paths.remove(&path); }
        }
    }
}

impl FileIo for PosixFile {
    fn stats(&self) -> Option<&IoStats> { Some(&self.stats) }

    fn requires_alignment(&self) -> bool { self.mode == IoMode::Direct }
    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
        self.stats.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.requires_alignment() {
            debug_assert_eq!(buf.len() % PAGE_SIZE, 0, "unaligned length");
            debug_assert_eq!(off as usize % PAGE_SIZE, 0, "unaligned offset");
        }
        #[cfg(unix)] {
            use std::os::unix::fs::FileExt;
            self.f.read_exact_at(buf, off)?;
        }
        #[cfg(windows)] {
            use std::os::windows::fs::FileExt;
            let mut done = 0usize;
            while done < buf.len() {
                let n = self.f.seek_read(&mut buf[done..], off + done as u64)?;
                if n == 0 { return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "seek_read hit EOF").into()); }
                done += n;
            }
        }
        Ok(())
    }
    fn write_at(&self, buf: &[u8], off: u64) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        self.stats.writes.fetch_add(1, Relaxed);
        self.stats.write_bytes.fetch_add(buf.len() as u64, Relaxed);
        if self.requires_alignment() {
            debug_assert_eq!(buf.len() % PAGE_SIZE, 0, "unaligned length");
            debug_assert_eq!(off as usize % PAGE_SIZE, 0, "unaligned offset");
        }
        #[cfg(unix)] {
            use std::os::unix::fs::FileExt;
            self.f.write_all_at(buf, off)?;
        }
        #[cfg(windows)] {
            // DuckDB/SQLite shape: positional WriteFile via OVERLAPPED --
            // Rust's seek_write. Loop: seek_write may write short.
            use std::os::windows::fs::FileExt;
            let mut done = 0usize;
            while done < buf.len() {
                let n = self.f.seek_write(&buf[done..], off + done as u64)?;
                if n == 0 { return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "seek_write wrote 0").into()); }
                done += n;
            }
        }
        Ok(())
    }
    fn sync_data(&self) -> Result<()> { sync_data_raw(&self.f) }
    fn sync_full(&self) -> Result<()> { sync_full_raw(&self.f) }
    fn sync_full_primitive(&self) -> &'static str { sync_full_primitive_name() }
    fn sync_dir(&self) -> Result<()> {
        // Unix: fsync the directory so the file's EXISTENCE is durable (the
        // Law 3 checkpoint fix). Windows: opening a directory as a File is not
        // a thing, and NTFS journals metadata -- SQLite's os_win.c fsyncs no
        // directories either. No-op there, by design and stated.
        #[cfg(unix)] { self.dir.sync_all()?; }
        Ok(())
    }
    fn len(&self) -> Result<u64> { Ok(self.f.metadata()?.len()) }
    fn set_len(&self, n: u64) -> Result<()> { self.f.set_len(n)?; Ok(()) }
}

#[cfg(target_os = "linux")]
fn sync_data_raw(f: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: fd is valid for the lifetime of `f`; fdatasync takes one int arg.
    let rc = unsafe { libc::fdatasync(f.as_raw_fd()) };
    if rc == -1 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}

#[cfg(target_os = "macos")]
fn sync_data_raw(f: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // Plain fsync -- NOT std::fs::File::sync_data, which libstd routes
    // through fcntl(F_FULLFSYNC) on this platform. See the trait doc comment.
    // SAFETY: fd is valid for the lifetime of `f`; fsync takes one int arg.
    let rc = unsafe { libc::fsync(f.as_raw_fd()) };
    if rc == -1 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}

// Windows: FlushFileBuffers for BOTH sync levels -- what SQLite's winSync and
// DuckDB's FileSync call. There is no fdatasync distinction to honour, so
// Normal and Full collapse to the same (strong) barrier: conservative, stated.
// Android and other unixes: sync_all -- also conservative, also stated.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sync_data_raw(f: &File) -> Result<()> { f.sync_all()?; Ok(()) }

#[cfg(target_os = "macos")]
fn sync_full_raw(f: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: fd is valid for the lifetime of `f`; F_FULLFSYNC takes no argument.
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc == -1 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn sync_full_raw(f: &File) -> Result<()> { f.sync_all()?; Ok(()) }

fn sync_full_primitive_name() -> &'static str {
    #[cfg(target_os = "macos")] { "F_FULLFSYNC" }
    #[cfg(not(target_os = "macos"))] { "fsync (sync_all)" }
}

/// Open `path`, asking for `want`. Returns the mode ACTUALLY obtained.
///
/// A platform that cannot give unbuffered I/O gets buffered I/O and says so.
/// It must never silently behave like a different engine — that failure mode
/// (an mmap path returning None on Windows, every index quietly falling back to
/// resident) is exactly what this return value exists to prevent.
pub fn open_file(path: &Path, want: IoMode) -> Result<(Box<dyn FileIo>, IoMode)> {
    open_file_impl(path, want, false)
}

/// Open the database data file for its sole writer and retain an advisory
/// whole-file lock for exactly as long as the returned `FileIo` owns the fd.
/// Readers deliberately use `open_file_readonly` and never contend here.
pub fn open_file_writer(path: &Path, want: IoMode) -> Result<(Box<dyn FileIo>, IoMode)> {
    open_file_impl(path, want, true)
}

fn open_file_impl(path: &Path, want: IoMode, writer: bool) -> Result<(Box<dyn FileIo>, IoMode)> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    let dir = File::open(parent)?;

    // A FRESH builder per attempt. `custom_flags` mutates the builder in place
    // and has no reset, so reusing one would carry O_DIRECT into the fallback
    // and make the retry fail identically — turning "degrade and report" into a
    // hard error on every filesystem that refuses unbuffered I/O. tmpfs and
    // overlayfs both do, and this engine is measured inside a container.
    let base = || {
        let mut o = OpenOptions::new();
        o.read(true).write(true).create(true);
        o
    };

    let (f, got) = match want {
        IoMode::Direct => match open_unbuffered(base(), path) {
            Ok(f) => (f, IoMode::Direct),
            Err(_) => (base().open(path)?, IoMode::Buffered),
        },
        IoMode::Buffered => (base().open(path)?, IoMode::Buffered),
    };
    let (writer_path, writer_identity) = if writer {
        let path = std::fs::canonicalize(path)?;
        let identity = file_identity(&f)?;
        let mut paths = writer_paths().lock().unwrap();
        if paths.get(&path) == Some(&identity) {
            // A few crash tests deliberately `forget` a Store and reopen it
            // in the same test process. A real process death closes the fd;
            // forgetting Rust ownership cannot. Test-support builds allow
            // that legacy simulation to reach recovery, while production
            // refuses the second in-process writer here. Other processes do
            // not share this registry and still contend on the OS lock below.
            #[cfg(feature = "test-support")]
            { (None, None) }
            #[cfg(not(feature = "test-support"))]
            { return Err(Error::WriterLocked); }
        } else {
            if !try_lock_exclusive(&f)? { return Err(Error::WriterLocked); }
            paths.insert(path.clone(), identity);
            (Some(path), Some(identity))
        }
    } else {
        (None, None)
    };
    Ok((Box::new(PosixFile {
        f,
        #[cfg(unix)]
        dir,
        mode: got,
        stats: IoStats::default(),
        writer_path,
        writer_identity,
    }), got))
}

/// 2f: open for a SNAPSHOT READER -- read-only at the OS level, so the
/// reader cannot write even by bug, and always Buffered (a reader shares
/// the file with a live writer; O_DIRECT's alignment contract buys nothing
/// on a cache the writer is also warming).
pub fn open_file_readonly(path: &Path) -> Result<Box<dyn FileIo>> {
    let parent = path.parent().unwrap_or(Path::new("."));
    #[cfg(unix)]
    let dir = File::open(parent)?;
    let f = OpenOptions::new().read(true).open(path)?;
    Ok(Box::new(PosixFile {
        f,
        #[cfg(unix)]
        dir,
        mode: IoMode::Buffered,
        stats: IoStats::default(),
        writer_path: None,
        writer_identity: None,
    }))
}

/// Recovery reads through an OS read-only descriptor while excluding writers.
/// No create semantics: a missing source must stay missing.
pub fn open_recovery_source(path: &Path) -> Result<Box<dyn FileIo>> {
    let parent = path.parent().unwrap_or(Path::new("."));
    #[cfg(unix)]
    let dir = File::open(parent)?;
    let f = OpenOptions::new().read(true).open(path)?;
    let path = std::fs::canonicalize(path)?;
    let identity = file_identity(&f)?;
    let mut paths = writer_paths().lock().unwrap();
    if paths.get(&path) == Some(&identity) || !try_lock_exclusive(&f)? {
        return Err(Error::WriterLocked);
    }
    paths.insert(path.clone(), identity);
    Ok(Box::new(PosixFile {
        f,
        #[cfg(unix)]
        dir,
        mode: IoMode::Buffered,
        stats: IoStats::default(),
        writer_path: Some(path),
        writer_identity: Some(identity),
    }))
}

/// Make a newly created directory's entry durable using the same portability
/// contract as FileIo::sync_dir (Windows relies on the metadata journal).
pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    { File::open(path)?.sync_all()?; }
    #[cfg(windows)]
    { let _ = path; }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_unbuffered(mut opts: OpenOptions, path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    opts.custom_flags(libc::O_DIRECT).open(path)
}

#[cfg(target_os = "macos")]
fn open_unbuffered(_opts: OpenOptions, _path: &Path) -> std::io::Result<File> {
    // R1: on the tested external APFS volume, concurrent F_NOCACHE writes /
    // reads returned unrelated bytes, also in a standalone C probe with aligned
    // buffers (23/1600 mismatches; buffered control 0/1600). We cannot identify
    // all affected OS/device combinations from an open() success. Until that
    // boundary is proven, use the explicit Buffered fallback on macOS.
    // Sacrifice: macOS Direct requests use the OS cache. P1/P2 already used
    // Buffered. See docs/RECOVERY_R1.md in the E4 root for retained evidence.
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
        "macOS uncached I/O disabled after failed data-isolation probe"))
}

// Windows upgrade path: FILE_FLAG_NO_BUFFERING via OpenOptionsExt (needs
// sector-aligned buffers; AlignedRegion already is). Basic posture for now:
// refuse, so open_file degrades to Buffered and REPORTS it -- same as any
// filesystem that cannot do O_DIRECT.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_unbuffered(_opts: OpenOptions, _path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no unbuffered mode"))
}

/// A page-aligned heap region. The buffer pool owns exactly one of these, which
/// satisfies the alignment requirement of unbuffered I/O and the "allocate once,
/// never free" requirement that keeps allocator retention out of the picture.
pub struct AlignedRegion { ptr: *mut u8, len: usize }

// SAFETY: the region is owned exclusively and never aliased across threads
// except behind the pool's own synchronisation.
unsafe impl Send for AlignedRegion {}
unsafe impl Sync for AlignedRegion {}

impl AlignedRegion {
    pub fn new(len: usize) -> Result<Self> {
        // A zero-size layout is undefined behaviour in alloc_zeroed, and
        // `0 % PAGE_SIZE == 0` passes the alignment check, so it must be
        // rejected explicitly rather than assumed away in a comment.
        assert!(len > 0, "an AlignedRegion of zero bytes is a zero-size allocation");
        assert_eq!(len % PAGE_SIZE, 0);
        let layout = std::alloc::Layout::from_size_align(len, PAGE_SIZE).unwrap();
        // SAFETY: layout has non-zero size and a valid power-of-two alignment.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() { return Err(Error::OutOfBudget); }
        Ok(AlignedRegion { ptr, len })
    }
    fn frame_start(&self, i: usize) -> usize {
        // checked, because in a release build `i + 1` on a pathological index
        // wraps to 0 and defeats the bound entirely.
        let start = i.checked_mul(PAGE_SIZE).expect("frame index overflow");
        let end = start.checked_add(PAGE_SIZE).expect("frame index overflow");
        assert!(end <= self.len, "frame {i} is outside the region");
        start
    }

    /// # Safety
    /// No `&mut` to frame `i` may be live for the lifetime of the returned
    /// slice. The buffer pool guarantees this with its pin count; this type
    /// cannot.
    pub unsafe fn page(&self, i: usize) -> &[u8] {
        let start = self.frame_start(i);
        // SAFETY: bounds checked above; exclusivity is the caller's obligation.
        unsafe { std::slice::from_raw_parts(self.ptr.add(start), PAGE_SIZE) }
    }

    /// # Safety
    /// No other reference to frame `i` — shared or mutable — may be live for
    /// the lifetime of the returned slice.
    ///
    /// This is deliberately an `unsafe fn`. As a safe fn, ordinary safe code
    /// could call it twice and hold two `&mut [u8]` over the same bytes, which
    /// is undefined behaviour however careful the pool is. A comment cannot
    /// make an unenforceable invariant sound; moving the obligation to the
    /// caller, in the type system, can.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn page_mut(&self, i: usize) -> &mut [u8] {
        let start = self.frame_start(i);
        // SAFETY: bounds checked above; exclusivity is the caller's obligation.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(start), PAGE_SIZE) }
    }

    /// A page-multiple prefix for one aligned multi-page I/O. The caller must
    /// enforce the same aliasing discipline as [`Self::page`].
    pub unsafe fn prefix(&self, len: usize) -> &[u8] {
        assert!(len > 0 && len <= self.len && len % PAGE_SIZE == 0);
        unsafe { std::slice::from_raw_parts(self.ptr, len) }
    }
}

impl Drop for AlignedRegion {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.len, PAGE_SIZE).unwrap();
        // SAFETY: ptr came from alloc_zeroed with this exact layout.
        unsafe { std::alloc::dealloc(self.ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PAGE_SIZE;

    #[test]
    fn pages_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let (f, _) = open_file(&path, IoMode::Buffered).unwrap();

        let mut w = vec![0u8; PAGE_SIZE];
        for (i, b) in w.iter_mut().enumerate() { *b = (i % 251) as u8; }
        f.write_at(&w, (PAGE_SIZE * 3) as u64).unwrap();
        f.sync_data().unwrap();

        let mut r = vec![0u8; PAGE_SIZE];
        f.read_at(&mut r, (PAGE_SIZE * 3) as u64).unwrap();
        assert_eq!(r, w);
        assert_eq!(f.len().unwrap(), (PAGE_SIZE * 4) as u64);
    }

    #[test]
    fn a_read_past_the_end_is_an_error_not_a_short_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let (f, _) = open_file(&dir.path().join("t.db"), IoMode::Buffered).unwrap();
        let mut r = vec![0u8; PAGE_SIZE];
        assert!(f.read_at(&mut r, 0).is_err());
    }

    /// Requesting Direct must NEVER fail merely because Direct is unavailable.
    /// It must degrade to Buffered and say so. Filesystems that refuse
    /// unbuffered I/O (tmpfs, overlayfs, many container mounts) are ordinary,
    /// and this engine is measured inside a container.
    ///
    /// `assert!(matches!(got, Direct | Buffered))` would be tautological — the
    /// enum has exactly those variants — so it asserts nothing. This asserts
    /// the two things that can actually be wrong: that the open succeeded, and
    /// that `requires_alignment` follows the mode obtained rather than the one
    /// requested.
    #[test]
    fn requesting_direct_degrades_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let (f, got) = open_file(&dir.path().join("t.db"), IoMode::Direct)
            .expect("requesting Direct must degrade, never error");
        assert_eq!(f.requires_alignment(), got == IoMode::Direct);
        #[cfg(target_os = "macos")]
        assert_eq!(got, IoMode::Buffered, "unproven uncached mode must stay disabled");

        // Usable either way.
        let w = vec![0u8; PAGE_SIZE];
        f.write_at(&w, 0).unwrap();
        f.sync_data().unwrap();
        let mut r = vec![0u8; PAGE_SIZE];
        f.read_at(&mut r, 0).unwrap();
        assert_eq!(r, w);
    }

    #[test]
    fn concurrent_direct_and_buffered_files_keep_their_own_bytes() {
        // R1 observed one direct read return another file's freelist header.
        // Exercise both zero and distinctive payloads under directory churn;
        // retain actual bytes and the path if the symptom recurs.
        std::thread::scope(|scope| {
            for worker in 0..8u8 {
                scope.spawn(move || {
                    for round in 0..32u8 {
                        let dir = tempfile::tempdir().unwrap();
                        let mode = if worker % 2 == 0 { IoMode::Direct } else { IoMode::Buffered };
                        let (f, _) = open_file(&dir.path().join("roundtrip"), mode).unwrap();
                        let mut w = vec![0u8; PAGE_SIZE];
                        if round % 2 != 0 {
                            for (i, byte) in w.iter_mut().enumerate() { *byte = worker.wrapping_add(round).wrapping_add(i as u8); }
                        }
                        f.write_at(&w, 0).unwrap();
                        f.sync_data().unwrap();
                        let mut r = vec![0u8; PAGE_SIZE];
                        f.read_at(&mut r, 0).unwrap();
                        if r != w {
                            let retained = dir.keep();
                            std::fs::write(retained.join("expected"), &w).unwrap();
                            std::fs::write(retained.join("observed"), &r).unwrap();
                            panic!("I/O isolation failed: worker {worker}, round {round}, evidence {}", retained.display());
                        }
                    }
                });
            }
        });
    }
}

/// Advisory whole-file lock, non-blocking (2n reader table). Ok(true) =
/// acquired; Ok(false) = held by a live process. The lock dies with the fd
/// -- crash-safe by construction. Platform code lives HERE (the 2d rule).
pub fn try_lock_exclusive(f: &std::fs::File) -> std::io::Result<bool> {
    // std's file lock rather than raw flock: same contract on Unix, and it is
    // the only form that EXISTS on Windows (LockFileEx underneath). The hand-
    // rolled flock made the whole kernel un-compilable off Unix, which the
    // reader table cannot afford -- without it the writer must assume a reader
    // at generation 0 and page recycling stops for good.
    match f.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}
