//! Shared memory-mapped file view.
//!
//! Thin wrapper around the OS `mmap()` syscall, used by both
//! [`PayloadStore`](crate::PayloadStore) and
//! [`VectorStore`](super::vecstore::VectorStore) for zero-copy reads.

/// Read-only view into a memory-mapped file region.
///
/// Created via [`MmapView::try_new`]; dropped automatically via `munmap`.
/// Zero-copy reads via [`slice()`](Self::slice) — no syscall, just pointer
/// arithmetic into the kernel page cache.
#[cfg(unix)]
pub(crate) struct MmapView {
    ptr: *const u8,
    len: usize,
}

#[cfg(unix)]
unsafe impl Send for MmapView {}
#[cfg(unix)]
unsafe impl Sync for MmapView {}

#[cfg(unix)]
impl MmapView {
    /// Map the first `len` bytes of `file` into memory (read-only, private).
    ///
    /// Returns `None` if `len == 0` or the kernel rejects the mapping.
    pub fn try_new(file: &std::fs::File, len: usize) -> Option<Self> {
        if len == 0 { return None; }
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn mmap(
                addr: *mut std::ffi::c_void, length: usize,
                prot: i32, flags: i32, fd: i32, offset: i64,
            ) -> *mut std::ffi::c_void;
            fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
        }
        const PROT_READ: i32 = 1;
        const MAP_PRIVATE: i32 = 2;
        let ptr = unsafe {
            mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, file.as_raw_fd(), 0)
        };
        if ptr == !0usize as *mut std::ffi::c_void { // MAP_FAILED
            return None;
        }
        // MADV_NORMAL (0) — let OS use default readahead policy.
        unsafe { madvise(ptr, len, 0); }
        Some(Self { ptr: ptr as *const u8, len })
    }

    /// Zero-copy slice into the mapped region.
    ///
    /// Returns `None` if the requested range exceeds the mapped length.
    #[inline]
    pub fn slice(&self, offset: usize, read_len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(read_len)?;
        if end > self.len { return None; }
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
