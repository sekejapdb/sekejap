//! # Memory-mapped file view — reading a file as if it were a byte array
//!
//! Normally, to read part of a file you ask the operating system to copy those
//! bytes into a buffer you own (a `read()` syscall). Memory mapping is a
//! different deal with the OS: it hands your program a pointer, and pretends the
//! whole file is already sitting in memory at that address. When you actually
//! touch a byte, the OS quietly loads that page from disk in the background and
//! caches it. You never call `read()`; you just index into memory.
//!
//! That is the whole trick behind sekejap being "disk-first": an index can live
//! on disk as one big file, and this type lets the rest of the code treat it as
//! a `&[u8]` slice with **zero copying** — the bytes are read straight out of the
//! kernel's page cache. RAM only fills with the pages you actually touch, and the
//! OS reclaims cold ones on its own. So a 40 GB index costs almost no RAM until
//! it's used.
//!
//! ## How it works
//!
//! 1. [`MmapView::try_new`] calls the raw `mmap` syscall to map a file
//!    read-only and privately, and stores the returned pointer + length.
//! 2. [`MmapView::slice`] does bounds-checked pointer arithmetic to hand back a
//!    `&[u8]` into the mapping — no syscall, no copy, just an offset.
//! 3. `Drop` calls `munmap` to release the mapping when the view goes away.
//!
//! ## Core components
//!
//! - [`MmapView`] — the whole file: a raw `ptr` + `len`. It is `unsafe impl
//!   Send + Sync` because a read-only mapping is safe to share across threads
//!   (nothing mutates it).
//! - **Unix only.** `mmap`/`munmap` are Unix syscalls, so the real type is
//!   `#[cfg(unix)]`. On other platforms (Windows) a stub with the same shape
//!   exists so the disk-first modules still compile — `try_new` just returns
//!   `None`, and every index falls back to its in-RAM path.
//!
//! Used by [`PayloadStore`](crate::PayloadStore),
//! [`VectorStore`](super::vecstore::VectorStore), and every mmap-served index.

/// Read-only view into a memory-mapped file region.
///
/// Created via [`MmapView::try_new`]; dropped automatically via `munmap`.
/// Zero-copy reads via [`slice()`](Self::slice) — no syscall, just pointer
/// arithmetic into the kernel page cache.
///
/// Rust note for newcomers: `*const u8` is a **raw pointer** — a bare memory
/// address, like a pointer in C. Unlike Rust's normal `&` references, a raw
/// pointer has no borrow-checker protection and no lifetime, so *we* are
/// responsible for only reading valid, in-bounds bytes. That is exactly why the
/// read method below is careful, and why creating/using it needs `unsafe`.
#[cfg(unix)]
pub(crate) struct MmapView {
    ptr: *const u8, // start address the kernel gave us for the mapping
    len: usize,     // how many bytes are mapped (the bound we check against)
}

// `Send` = "safe to move to another thread"; `Sync` = "safe to share by
// reference across threads". Rust won't auto-derive them for a struct holding a
// raw pointer (it can't know the pointer is safe to share), so we assert it with
// `unsafe impl`. It IS safe here because the mapping is READ-ONLY: many threads
// reading the same immutable bytes can never race. If this view were writable,
// these impls would be unsound.
#[cfg(unix)]
unsafe impl Send for MmapView {}
#[cfg(unix)]
unsafe impl Sync for MmapView {}

#[cfg(unix)]
impl MmapView {
    /// Map the first `len` bytes of `file` into memory, read-only.
    ///
    /// Asks the kernel for a mapping and keeps the pointer it returns. Nothing is
    /// read from disk yet — pages load lazily on first touch (see [`slice`]).
    /// Returns `None` if `len == 0` or the kernel refuses the mapping (e.g. out
    /// of address space); callers treat `None` as "fall back to reading in RAM".
    ///
    /// [`slice`]: Self::slice
    pub fn try_new(file: &std::fs::File, len: usize) -> Option<Self> {
        if len == 0 { return None; } // an empty mapping is meaningless — bail early
        use std::os::unix::io::AsRawFd; // brings the `.as_raw_fd()` method into scope
        // `extern "C" { ... }` is Rust's FFI (Foreign Function Interface): it
        // declares functions that live in the operating system's C library so we
        // can call them directly — no `libc` crate dependency needed. `mmap`
        // creates the mapping; `madvise` gives the kernel a hint about how we'll
        // read it.
        extern "C" {
            fn mmap(
                addr: *mut std::ffi::c_void, length: usize,
                prot: i32, flags: i32, fd: i32, offset: i64,
            ) -> *mut std::ffi::c_void;
            fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
        }
        const PROT_READ: i32 = 1;    // pages are readable, never writable
        const MAP_PRIVATE: i32 = 2;  // our own view; the file on disk is never modified
        // Calling a C function is `unsafe`: the compiler can't verify the OS keeps
        // its promises, so we take responsibility. addr = null (let the kernel
        // choose the address), offset = 0 (map from the start of the file).
        let ptr = unsafe {
            mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, file.as_raw_fd(), 0)
        };
        // mmap signals failure with the sentinel MAP_FAILED (all-ones), not null.
        if ptr == !0usize as *mut std::ffi::c_void {
            return None;
        }
        // MADV_NORMAL (0): use the OS's default read-ahead. We touch these files
        // both sequentially and randomly, so no special advice wins across the board.
        unsafe { madvise(ptr, len, 0); }
        Some(Self { ptr: ptr as *const u8, len })
    }

    /// Borrow `read_len` bytes starting at `offset` as a plain `&[u8]`.
    ///
    /// This is the hot path — a bounds check plus pointer arithmetic, no syscall
    /// and no copy. Returns `None` if the range runs past the end of the mapping
    /// (or the offset + length overflows), so a corrupt on-disk offset can never
    /// read out of bounds.
    #[inline]
    pub fn slice(&self, offset: usize, read_len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(read_len)?;      // reject overflow, not just OOB
        if end > self.len { return None; }            // stay inside the mapping
        // Safe: the range is verified in-bounds above, and the mapping outlives
        // the returned slice (tied to `&self`).
        unsafe { Some(std::slice::from_raw_parts(self.ptr.add(offset), read_len)) }
    }

    /// Total number of mapped bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(unix)]
impl Drop for MmapView {
    fn drop(&mut self) {
        extern "C" {
            fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
        }
        unsafe { munmap(self.ptr as *mut std::ffi::c_void, self.len); }
    }
}

// ── Non-Unix stub ────────────────────────────────────────────────────────────
//
// sekejap's memory-mapped paged mode is Unix-only (raw `mmap`/`munmap`). On
// non-Unix targets (Windows) `MmapView` still exists so the disk-first index
// modules compile and share one code path, but it never maps anything:
// `try_new` returns `None`, so paged-mode loads fall through and every index
// uses its resident path. A real Windows mapping (e.g. via `memmap2`) could
// enable paged mode there later — see roadmap "Format & language stability".
#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) struct MmapView {
    _never: std::convert::Infallible,
}

#[cfg(not(unix))]
#[allow(dead_code)]
impl MmapView {
    pub fn try_new(_file: &std::fs::File, _len: usize) -> Option<Self> {
        None
    }

    #[inline]
    pub fn slice(&self, _offset: usize, _read_len: usize) -> Option<&[u8]> {
        match self._never {}
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self._never {}
    }
}
