//! Write-ahead log. Frame layout, little-endian:
//!   0  len     u32   payload length
//!   4  lsn     u64
//!   12 kind    u8
//!   13 pad     u8 x3
//!   16 crc32c  u32   over the 16-byte header with crc zeroed, then payload
//!   20 payload
//!
//! THE TORN TAIL, and the two mechanisms that hold the property.
//!
//! The predecessor engine lost five rows that had been written AND fsynced. A
//! crash left a complete header with a short payload; the next writer appended
//! at PHYSICAL END OF FILE, so those bytes survived and the new records landed
//! after them. On replay the torn header's declared length swallowed the new
//! records as its own payload, failed CRC, and stopped replay before reaching
//! them. Every torn-tail test in that engine passed, because none of them wrote
//! anything AFTER recovering.
//!
//! Here the property is held REDUNDANTLY, by two independent mechanisms, and
//! **either one alone is sufficient**:
//!
//!   1. `open` truncates to the end of the last CRC-valid frame only when the
//!      next header declares a frame that physically crosses EOF, or when
//!      every remaining byte is zero. A complete CRC-failed frame and a
//!      non-zero fragment shorter than a header are damage, never endings.
//!   2. `append` writes at `self.end` — the offset `scan`'s CRC walk
//!      established — and never at physical EOF, so a new frame is laid down ON
//!      TOP of any torn bytes and there is nothing left to swallow it.
//!
//! **This redundancy is a maintenance hazard and must be treated as one.**
//! Removing either mechanism alone breaks NO test, because the other still holds
//! the property. Someone deleting one will see a green suite and reasonably
//! conclude it was dead code; someone later deleting the other will see a green
//! suite too, right up until a power cut. Reproducing the original bug requires
//! removing BOTH — which is exactly what the falsification in this task does.
//!
//! Because the integration test cannot distinguish the two, each mechanism is
//! additionally pinned by its own direct test:
//! `an_all_zero_extension_is_a_clean_ending_and_is_truncated` and
//! `append_lands_at_the_scanned_end_not_at_physical_eof`.
//!
//! WHY A WALK MUST SAY WHY IT STOPPED (Task 19; Task 17 final review, F4).
//!
//! `scan` walks frames until one cannot be accepted. There are two utterly
//! different reasons that can happen, and for most of this engine's life
//! they left through the same door:
//!
//!   - THE LOG ENDS HERE. A writer was interrupted; what follows is a
//!     partial frame, or nothing, or zeros. Discarding it costs nothing,
//!     because no completed write is in it.
//!   - SOMETHING IS WRONG HERE. A bit rotted, a sector went bad, a header
//!     verifies but names a record kind this build does not know. What
//!     follows may be hundreds of committed frames.
//!
//! Treating the second as the first is how one flipped bit at the midpoint
//! of a log destroyed 1,499 of 3,000 committed rows while `open` returned
//! `Ok`, and how a single injected EIO on a header read erased half a log.
//! Both measured. The distinction is now carried by `Stop`, a type -- not by
//! a comment asserting that every `break` means the same thing, which is
//! what the previous version of this file did, incorrectly, in four places.
//!
//! Three rules follow, and all three are load-bearing:
//!   1. Only `Stop::End` may truncate.
//!   2. A read that FAILED is not an answer about content: it leaves as
//!      `Err`. "I could not read this" is not "the log ends here".
//!   3. Before CRC, the payload bound and physical extent limit reads. The
//!      unverified writer shape may only move classification toward DAMAGE;
//!      kind and LSN are trusted only after CRC verifies them.
//!
//! And a fourth rule, about the refusal itself: `Stop::Damaged` makes
//! `open` refuse, which is only half a design. A refusal nothing can clear
//! is as unrecoverable as a deletion, and Law 5 forbids both. `recover()`
//! is the other half -- it copies the whole log aside intact,
//! independently re-reads the copy, then reconstructs committed regions that
//! can be resynchronised as the live log -- and
//! `a_damaged_log_refuses_to_open_and_recover_clears_it` pins the two
//! halves together, because either alone is a defect.

use crate::io::{open_file, FileIo, IoMode};
use crate::{Error, Result};
use std::path::Path;

const HDR: usize = 20;

/// The largest number of bytes ONE frame this crate writes can occupy on
/// disk. The whole encoded frame must fit in the format's `u32` size bound;
/// D7's value-to-end frames therefore retain essentially the whole 4 GiB
/// payload range instead of returning to the old 64 KiB truncation.
///
/// This is a validity bound on every disk frame as well as every writer. The
/// length is checked before allocation: a larger value read off disk is
/// damage even when the file is large enough to satisfy it.
const MAX_FRAME_BYTES: u64 = u32::MAX as u64;
pub const MAX_PAYLOAD_BYTES: u64 = MAX_FRAME_BYTES - HDR as u64;
const CRC_CHUNK: usize = 64 * 1024;

/// Why `scan` stopped walking, as a type rather than as a comment.
///
/// `scan` had EIGHT `break` statements, four of which meant "the log
/// genuinely ends here" and four of which meant "something is wrong here" --
/// and all eight left through the same `Ok((end, ..))` return, which
/// `open_impl` then handed to `set_len(end)`. So an injected EIO on a single
/// header read returned a clean end and erased 640 of 1,280 bytes, and one
/// flipped bit at the midpoint of a 3,000-row log destroyed 1,499 committed
/// rows while `open()` returned `Ok`. Both measured (Task 17 final review,
/// F4).
///
/// The distinction is now in the type system, so no exit can join the wrong
/// one by accident:
///   - `End` is the ONLY variant `open_impl` will truncate on.
///   - `Damaged` refuses the open, touching nothing -- and, because a
///     refusal with no way out is its own Law 5 violation, `recover()`
///     clears it by setting the whole log aside intact and reconstructing
///     independently verified committed regions.
///   - An I/O error is neither: it is not an answer about the log's content
///     at all, so it leaves `scan` as `Err` and never reaches this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The log genuinely ends at `end`. Everything from there to EOF is
    /// either a writer-shaped header whose bounded frame crosses physical
    /// EOF, or an all-zero extension. `why` names which proof was established.
    End(&'static str),
    /// Something is wrong at `offset`. The remaining bytes may include
    /// committed frames regardless of how short they are. Nothing here may
    /// be discarded.
    Damaged { offset: u64, why: &'static str },
}

/// What a walk of the log found: how far it got, the LSN to carry on from,
/// and -- the part that used to be missing -- WHY it stopped.
#[derive(Debug, Clone, Copy)]
pub struct Scan {
    pub end: u64,
    pub next_lsn: u64,
    pub stop: Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecKind {
    Commit = 1,
    PageImage = 2,
    Put = 3,
    Delete = 4,
    DeletePrefix = 5,
    PutEmptyBatch = 6,
}

impl RecKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Commit,
            2 => Self::PageImage,
            3 => Self::Put,
            4 => Self::Delete,
            5 => Self::DeletePrefix,
            6 => Self::PutEmptyBatch,
            _ => return None,
        })
    }
}

/// 256 KiB append buffer. Appending was one pwrite + one Vec per record --
/// measured 59.4% of a load's wall time inside pwrite, 2.1M syscalls per 1M
/// rows. No engine does that (PostgreSQL: wal_buffers). Constant, so Law 1
/// holds. `end` = logical end; `flushed` = what the file has; invariant
/// flushed + buf.len() == end. Every barrier flushes first, so no durability
/// promise changes; commit() must flush in every mode -- Off promises no
/// BARRIER, it never promised the record stays in process RAM.
const WAL_BUF: usize = 256 * 1024;

pub struct Wal {
    file: Box<dyn FileIo>,
    end: u64,
    next_lsn: u64,
    buf: Vec<u8>,
    flushed: u64,
    limit: Option<u64>,
}

pub(crate) struct SalvagedWal {
    pub bytes: u64,
    pub hash: u32,
}

enum Candidate {
    Valid { next: u64, kind: RecKind },
    Invalid,
}

impl Wal {
    pub fn open(path: &Path, mode: IoMode) -> Result<Wal> {
        Self::open_limited(path, mode, None)
    }
    pub(crate) fn open_limited(path: &Path, mode: IoMode, limit: Option<u64>) -> Result<Wal> {
        if let Some(cap) = limit {
            match std::fs::metadata(path) {
                Ok(m) if m.len() > cap => return Err(Error::ResourceLimit("existing WAL exceeds allowance")),
                Ok(_) => (), Err(e) if e.kind() == std::io::ErrorKind::NotFound => (), Err(e) => return Err(e.into()),
            }
        }
        // The WAL is byte-addressed, not page-addressed, so it always uses
        // buffered I/O regardless of the store's mode.
        let _ = mode;
        let (file, _) = open_file(path, IoMode::Buffered)?;
        let mut wal = Self::open_on(file)?;
        wal.limit = limit;
        Ok(wal)
    }

    /// `open` with the file handed in, so a test can drive the whole open
    /// path -- classification, refusal, truncation -- against a `FileIo`
    /// that fails a specific read. Without this seam the "an I/O error is
    /// not the end of the log" property can only be argued, and it was
    /// argued wrongly for three rounds.
    fn open_on(file: Box<dyn FileIo>) -> Result<Wal> {
        let scan = Self::scan(&*file)?;
        match scan.stop {
            // Only ever on `End`. See `Stop`, and `set_len`'s own note
            // below on what this is and is not buying now.
            Stop::End(_) => {
                // Discard anything after the last good frame before we
                // append to it -- mechanism 1 of the two the module doc
                // comment names. What makes this safe, which it was not
                // before, is that `End` is now a CLASSIFIED answer: the
                // bytes past `end` have been shown to be a physically
                // incomplete bounded frame, or zeros.
                file.set_len(scan.end)?;
                file.sync_data()?;
            }
            Stop::Damaged { offset, why } => return Err(Error::CorruptWal { offset, why }),
        }
        Ok(Wal { file, end: scan.end, next_lsn: scan.next_lsn,
                 buf: Vec::with_capacity(WAL_BUF), flushed: scan.end, limit: None })
    }

    /// Walk the log without opening it for use and without changing a byte
    /// of it -- no `set_len`, no `sync_data`, and no refusal either: the
    /// caller gets the `Stop` and decides. `recover()` is the caller, and it
    /// needs all three of those properties, because it is the path that has
    /// to still work on exactly the images `open` refuses.
    pub fn inspect(path: &Path, mode: IoMode) -> Result<Scan> {
        let _ = mode;
        let (file, _) = open_file(path, IoMode::Buffered)?;
        Self::scan(&*file)
    }

    pub fn end_offset(&self) -> u64 { self.end }

    fn frame_crc(hdr: &[u8; HDR], payload: &[u8]) -> u32 {
        let mut h = [0u8; HDR];
        h.copy_from_slice(hdr);
        h[16..20].fill(0);
        crc32c::crc32c_append(crc32c::crc32c(&h), payload)
    }

    fn frame_crc_from_file(
        file: &dyn FileIo,
        hdr: &[u8; HDR],
        payload_at: u64,
        payload_len: u64,
        scratch: &mut [u8],
    ) -> Result<u32> {
        let mut h = *hdr;
        h[16..20].fill(0);
        let mut crc = crc32c::crc32c(&h);
        let mut at = payload_at;
        let end = payload_at.checked_add(payload_len).ok_or(Error::CorruptWal {
            offset: payload_at,
            why: "payload boundary overflows the WAL address space",
        })?;
        while at < end {
            let n = std::cmp::min(scratch.len() as u64, end - at) as usize;
            read_exact(file, &mut scratch[..n], at)?;
            crc = crc32c::crc32c_append(crc, &scratch[..n]);
            at += n as u64;
        }
        Ok(crc)
    }

    /// Walk frames from offset 0 until one of them cannot be accepted, and
    /// say WHY the walk stopped (see `Stop`).
    ///
    /// Two rules govern the order of the checks:
    ///
    /// 1. Before CRC, payload bounds and physical extents limit reads. An
    ///    impossible writer shape may only reject toward `Damaged`; semantic
    ///    fields are trusted for acceptance only after verification.
    /// 2. A read that FAILS is not an answer about the log's content. EIO on
    ///    a header read means "I could not read", not "the log ends here",
    ///    so it leaves as `Err` -- measured: laundering it into a clean end
    ///    made `set_len` erase half the log on one transient error.
    fn scan(file: &dyn FileIo) -> Result<Scan> {
        let len = file.len()?;
        let mut off = 0u64;
        let mut next_lsn = 1u64;
        let mut crc_scratch = vec![0u8; CRC_CHUNK];
        let stop = loop {
            let header_end = off.checked_add(HDR as u64).ok_or(Error::CorruptWal {
                offset: off,
                why: "header boundary overflows the WAL address space",
            })?;
            if header_end > len {
                if off == len {
                    break Stop::End("the last verified frame reaches physical EOF");
                }
                if Self::is_all_zeros(file, off, len)? {
                    break Stop::End("nothing past the last good frame but zeros");
                }
                break Stop::Damaged {
                    offset: off,
                    why: "non-zero bytes remain but there is not a complete frame header",
                };
            }
            let mut hdr = [0u8; HDR];
            read_exact(file, &mut hdr, off)?;
            let plen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as u64;
            let lsn = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
            if plen > MAX_PAYLOAD_BYTES {
                break Stop::Damaged {
                    offset: off,
                    why: "frame payload length exceeds the writer maximum",
                };
            }
            let frame_end = header_end.checked_add(plen).ok_or(Error::CorruptWal {
                offset: off,
                why: "frame boundary overflows the WAL address space",
            })?;
            if frame_end > len {
                // An incomplete physical write can leave a complete header,
                // but that header must still have the shape every writer
                // emits. Rejecting an implausible unverified header is the
                // conservative direction: it preserves bytes and refuses.
                if !Self::has_writer_header_shape(&hdr) {
                    break Stop::Damaged {
                        offset: off,
                        why: "incomplete-looking frame header was not emitted by this writer",
                    };
                }
                // A corrupt length byte in a COMPLETE frame can also point
                // past EOF. If an independently checksummed frame exists
                // later, this cannot be a lone interrupted physical write.
                // Preserve the whole non-zero tail and refuse ordinary open.
                if Self::has_verified_frame_after(
                    file,
                    off.checked_add(1).ok_or(Error::CorruptWal {
                        offset: off,
                        why: "WAL resynchronisation offset overflow",
                    })?,
                    len,
                    &mut crc_scratch,
                )? {
                    break Stop::Damaged {
                        offset: off,
                        why: "incomplete-looking frame has verified log frames behind it",
                    };
                }
                break Stop::End("a bounded frame header crosses physical EOF");
            }
            let want = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
            if Self::frame_crc_from_file(file, &hdr, header_end, plen, &mut crc_scratch)? != want {
                if Self::is_all_zeros(file, off, len)? {
                    break Stop::End("nothing past the last good frame but zeros");
                }
                break Stop::Damaged {
                    offset: off,
                    why: "complete frame fails its checksum",
                };
            }
            // Verified. Only now may anything in the header be believed.
            if RecKind::from_u8(hdr[12]).is_none() {
                break Stop::Damaged {
                    offset: off,
                    why: "frame verifies but names a record kind this build does not know",
                };
            }
            next_lsn = lsn.checked_add(1).ok_or(Error::CorruptWal {
                offset: off,
                why: "verified frame exhausts the log sequence number space",
            })?;
            off = frame_end;
        };
        Ok(Scan { end: off, next_lsn, stop })
    }

    /// Read `[off, len)` in fixed-size chunks looking for a non-zero byte.
    /// Chunked, not slurped: the remainder can be as large as the log, and
    /// nothing in this engine is allowed to allocate in proportion to the
    /// store (Law 1). It runs only after frame verification has already
    /// stopped, so ordinary valid frames never pay for it.
    fn is_all_zeros(file: &dyn FileIo, off: u64, len: u64) -> Result<bool> {
        const CHUNK: usize = 64 * 1024;
        let mut buf = vec![0u8; CHUNK];
        let mut at = off;
        while at < len {
            let n = std::cmp::min(CHUNK as u64, len - at) as usize;
            read_exact(file, &mut buf[..n], at)?;
            if buf[..n].iter().any(|&b| b != 0) { return Ok(false); }
            at += n as u64;
        }
        Ok(true)
    }

    pub fn append(&mut self, kind: RecKind, payload: &[u8]) -> Result<u64> {
        if payload.len() as u64 > MAX_PAYLOAD_BYTES {
            return Err(Error::TooLarge);
        }
        let lsn = self.next_lsn;
        let next_lsn = self.next_lsn.checked_add(1).ok_or(Error::CorruptWal {
            offset: self.end,
            why: "log sequence number space is exhausted",
        })?;
        let frame_len = (HDR as u64).checked_add(payload.len() as u64).ok_or(Error::TooLarge)?;
        if frame_len > MAX_FRAME_BYTES { return Err(Error::TooLarge); }
        let new_end = self.end.checked_add(frame_len).ok_or(Error::TooLarge)?;
        if self.limit.is_some_and(|cap| new_end > cap) {
            return Err(Error::ResourceLimit("WAL allowance full; reduce transaction size"));
        }
        let mut hdr = [0u8; HDR];
        hdr[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        hdr[4..12].copy_from_slice(&lsn.to_le_bytes());
        hdr[12] = kind as u8;
        #[cfg(feature = "write-trace")]
        let crc_started = crate::write_trace::active().then(std::time::Instant::now);
        let c = Self::frame_crc(&hdr, payload);
        #[cfg(feature = "write-trace")]
        if let Some(started) = crc_started {
            crate::write_trace::add(crate::write_trace::Field::WalCrc, started.elapsed());
        }
        hdr[16..20].copy_from_slice(&c.to_le_bytes());

        // Into the buffer, no per-record allocation. Frames still land at
        // self.end (scan's CRC-walk offset, never physical EOF), so a torn
        // tail is still overwritten, not landed behind: flush writes at
        // self.flushed which starts at the same scan.end.
        #[cfg(feature = "write-trace")]
        let copy_started = crate::write_trace::active().then(std::time::Instant::now);
        self.buf.extend_from_slice(&hdr);
        self.buf.extend_from_slice(payload);
        #[cfg(feature = "write-trace")]
        if let Some(started) = copy_started {
            crate::write_trace::add(crate::write_trace::Field::WalBufferCopy, started.elapsed());
            crate::write_trace::value_copy();
        }
        self.next_lsn = next_lsn;
        self.end = new_end;
        if self.buf.len() >= WAL_BUF {
            #[cfg(feature = "write-trace")]
            let flush_started = crate::write_trace::active().then(std::time::Instant::now);
            self.flush()?;
            #[cfg(feature = "write-trace")]
            if let Some(started) = flush_started {
                crate::write_trace::add(crate::write_trace::Field::WalFlush, started.elapsed());
            }
        }
        Ok(lsn)
    }

    /// `SyncMode::Normal`'s barrier: durable against an OS crash, not
    /// necessarily against a loss of power to the drive.
    /// Push the buffer to the file. Not a barrier. Every path that reads the
    /// file or promises durability calls this first.
    pub fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() { return Ok(()); }
        write_all(&*self.file, &self.buf, self.flushed)?;
        crate::write_stats::add(crate::write_stats::Phase::Wal, self.buf.len() as u64);
        self.flushed += self.buf.len() as u64;
        self.buf.clear();
        debug_assert_eq!(self.flushed, self.end);
        Ok(())
    }

    pub fn sync_data(&mut self) -> Result<()> { self.flush()?; self.file.sync_data() }

    /// `SyncMode::Full`'s barrier: the strongest this platform can issue.
    pub fn sync_full(&mut self) -> Result<()> { self.flush()?; self.file.sync_full() }

    /// The exact primitive `sync_full` issues here, so a measurement can name
    /// what it did rather than imply it.
    pub fn sync_full_primitive(&self) -> &'static str { self.file.sync_full_primitive() }

    /// First recovery pass: find the byte immediately after the last Commit.
    /// Headers are fixed-size and payloads are skipped, so retained RAM is one
    /// header regardless of the number or size of committed transactions.
    pub(crate) fn committed_end(&self) -> Result<u64> {
        // Reads the FILE; anything buffered would be silently missing -- a
        // replay that drops committed records. Only caller is open, where
        // nothing is buffered; checked, not trusted.
        assert!(self.buf.is_empty(), "recovery with {} bytes buffered", self.buf.len());
        let mut off = 0u64;
        let mut committed = 0u64;
        while off < self.end {
            let mut hdr = [0u8; HDR];
            read_exact(&*self.file, &mut hdr, off)?;
            let plen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as u64;
            if plen > MAX_PAYLOAD_BYTES {
                return Err(Error::CorruptWal { offset: off, why: "recovery payload exceeds the writer maximum" });
            }
            let next = off.checked_add(HDR as u64).and_then(|n| n.checked_add(plen))
                .ok_or(Error::CorruptWal { offset: off, why: "frame boundary overflow during recovery" })?;
            if next > self.end {
                return Err(Error::CorruptWal { offset: off, why: "frame crosses verified WAL boundary during recovery" });
            }
            let kind = RecKind::from_u8(hdr[12]).ok_or(Error::CorruptWal {
                offset: off, why: "unknown record kind inside verified WAL",
            })?;
            if kind == RecKind::Commit { committed = next; }
            off = next;
        }
        Ok(committed)
    }

    /// Second recovery pass: read exactly one frame from the committed prefix.
    /// The returned payload is the only transaction data retained by replay.
    pub(crate) fn record_at(&self, off: u64, committed_end: u64)
        -> Result<Option<(u64, RecKind, Vec<u8>, u64)>>
    {
        if off == committed_end { return Ok(None); }
        if off > committed_end || committed_end > self.end {
            return Err(Error::CorruptWal { offset: off, why: "recovery cursor outside committed WAL boundary" });
        }
        let header_end = off.checked_add(HDR as u64)
            .ok_or(Error::CorruptWal { offset: off, why: "header boundary overflow during recovery" })?;
        if header_end > committed_end {
            return Err(Error::CorruptWal { offset: off, why: "truncated header inside committed WAL boundary" });
        }
        let mut hdr = [0u8; HDR];
        read_exact(&*self.file, &mut hdr, off)?;
        let plen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        if plen as u64 > MAX_PAYLOAD_BYTES {
            return Err(Error::CorruptWal { offset: off, why: "recovery payload exceeds the writer maximum" });
        }
        let next = header_end.checked_add(plen as u64)
            .ok_or(Error::CorruptWal { offset: off, why: "payload boundary overflow during recovery" })?;
        if next > committed_end {
            return Err(Error::CorruptWal { offset: off, why: "frame crosses committed WAL boundary" });
        }
        let mut payload = vec![0u8; plen];
        read_exact(&*self.file, &mut payload, header_end)?;
        let want = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
        if Self::frame_crc(&hdr, &payload) != want {
            return Err(Error::CorruptWal { offset: off, why: "recovery frame changed after the opening CRC walk" });
        }
        // Only verified fields may control replay.
        let lsn = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let kind = RecKind::from_u8(hdr[12]).ok_or(Error::CorruptWal {
            offset: off, why: "unknown record kind inside committed WAL",
        })?;
        Ok(Some((lsn, kind, payload, next)))
    }

    #[cfg(test)]
    fn replay(&self) -> Result<Vec<(u64, RecKind, Vec<u8>)>> {
        // Unit tests for the scanner inspect every verified frame, including
        // an uncommitted tail; production recovery uses `committed_end` above.
        let end = self.end;
        let mut out = Vec::new();
        let mut off = 0;
        while let Some((lsn, kind, payload, next)) = self.record_at(off, end)? {
            out.push((lsn, kind, payload));
            off = next;
        }
        Ok(out)
    }

    /// Raise the LSN floor. `scan` derives `next_lsn` from the file, so after a
    /// rotation leaves the log empty, a restart would otherwise renumber from 1.
    /// The store persists the high-water mark in the superblock and re-supplies
    /// it here at open.
    pub fn set_lsn_floor(&mut self, floor: u64) {
        if floor > self.next_lsn { self.next_lsn = floor; }
    }

    pub fn next_lsn(&self) -> u64 { self.next_lsn }

    /// Reconstruct committed regions from a damaged log into a fresh file.
    ///
    /// A damaged region invalidates the transaction around it. Resync scans
    /// byte-by-byte for a complete independently checksummed frame, discards
    /// through the first verified Commit boundary, then copies later complete
    /// transactions. Any later damage repeats that rule. The source is never
    /// changed; recovery has already made and verified its quarantine copy.
    pub(crate) fn salvage_committed(src: &Path, dst: &Path) -> Result<SalvagedWal> {
        use std::io::{Seek, SeekFrom};

        let (file, _) = open_file(src, IoMode::Buffered)?;
        let len = file.len()?;
        let mut out = std::fs::File::create(dst)?;
        let mut src_off = 0u64;
        let mut out_end = 0u64;
        let mut last_commit = 0u64;
        let mut resynchronising = false;
        let mut scratch = vec![0u8; CRC_CHUNK];

        while src_off < len {
            match Self::candidate_at(&*file, src_off, len, &mut scratch)? {
                Candidate::Valid { next, kind } => {
                    if resynchronising {
                        src_off = next;
                        if kind == RecKind::Commit {
                            resynchronising = false;
                        }
                        continue;
                    }
                    copy_range(&*file, &mut out, src_off, next - src_off, &mut scratch)?;
                    out_end = out_end.checked_add(next - src_off)
                        .ok_or(Error::CorruptWal {
                            offset: src_off,
                            why: "salvaged WAL output length overflow",
                        })?;
                    if kind == RecKind::Commit { last_commit = out_end; }
                    src_off = next;
                }
                Candidate::Invalid => {
                    out.set_len(last_commit)?;
                    out.seek(SeekFrom::Start(last_commit))?;
                    out_end = last_commit;
                    resynchronising = true;
                    src_off = src_off.checked_add(1).ok_or(Error::CorruptWal {
                        offset: src_off,
                        why: "WAL resynchronisation offset overflow",
                    })?;
                }
            }
        }
        out.set_len(last_commit)?;
        out.sync_all()?;
        drop(out);

        let hash = hash_prefix(dst, last_commit)?;
        Ok(SalvagedWal { bytes: last_commit, hash })
    }

    fn candidate_at(
        file: &dyn FileIo,
        off: u64,
        len: u64,
        scratch: &mut [u8],
    ) -> Result<Candidate> {
        let Some(header_end) = off.checked_add(HDR as u64) else { return Ok(Candidate::Invalid) };
        if header_end > len { return Ok(Candidate::Invalid); }
        let mut hdr = [0u8; HDR];
        read_exact(file, &mut hdr, off)?;
        // Cheap necessary writer invariants keep byte-wise resynchronisation
        // linear on arbitrary input. They never make a candidate valid; a
        // surviving candidate still has to pass its full CRC independently.
        if !Self::has_writer_header_shape(&hdr) {
            return Ok(Candidate::Invalid);
        }
        let plen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as u64;
        if plen > MAX_PAYLOAD_BYTES { return Ok(Candidate::Invalid); }
        let Some(next) = header_end.checked_add(plen) else { return Ok(Candidate::Invalid) };
        if next > len { return Ok(Candidate::Invalid); }
        let want = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
        if Self::frame_crc_from_file(file, &hdr, header_end, plen, scratch)? != want {
            return Ok(Candidate::Invalid);
        }
        let kind = RecKind::from_u8(hdr[12]).expect("kind was prefiltered above");
        let lsn = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        if lsn.checked_add(1).is_none() { return Ok(Candidate::Invalid); }
        Ok(Candidate::Valid { next, kind })
    }

    fn has_writer_header_shape(hdr: &[u8; HDR]) -> bool {
        hdr[13..16] == [0, 0, 0]
            && RecKind::from_u8(hdr[12]).is_some()
            && u64::from_le_bytes(hdr[4..12].try_into().unwrap()).checked_add(1).is_some()
    }

    fn has_verified_frame_after(
        file: &dyn FileIo,
        mut off: u64,
        len: u64,
        scratch: &mut [u8],
    ) -> Result<bool> {
        while off < len {
            if matches!(Self::candidate_at(file, off, len, scratch)?, Candidate::Valid { .. }) {
                return Ok(true);
            }
            off = off.checked_add(1).ok_or(Error::CorruptWal {
                offset: off,
                why: "WAL resynchronisation offset overflow",
            })?;
        }
        Ok(false)
    }

    /// Discard the log. MUST be called only after the pages it protects are
    /// durably on disk and the directory has been fsynced.
    pub fn rotate(&mut self) -> Result<()> {
        // Discard, not flush: the log is being thrown away.
        self.buf.clear();
        self.flushed = 0;
        self.file.set_len(0)?;
        self.file.sync_data()?;
        self.file.sync_dir()?;
        self.end = 0;
        Ok(())
    }

    /// Ordinary checkpoint after durable data/root publication, with already
    /// durable filenames. Truncation still needs its file barrier, but creates
    /// no directory-entry obligation. Create/recovery retain `rotate` above.
    pub(crate) fn rotate_published(&mut self) -> Result<()> {
        self.buf.clear();
        self.flushed = 0;
        self.file.set_len(0)?;
        self.file.sync_data()?;
        self.end = 0;
        Ok(())
    }
}

// The FileIo trait is page-aligned by contract; the WAL needs byte access, so it
// wraps unaligned reads/writes here rather than weakening the trait.
fn read_exact(f: &dyn FileIo, buf: &mut [u8], off: u64) -> Result<()> { f.read_at(buf, off) }
fn write_all(f: &dyn FileIo, buf: &[u8], off: u64) -> Result<()> { f.write_at(buf, off) }

fn copy_range(
    src: &dyn FileIo,
    dst: &mut std::fs::File,
    mut at: u64,
    mut n: u64,
    scratch: &mut [u8],
) -> Result<()> {
    use std::io::Write;
    while n > 0 {
        let take = std::cmp::min(n, scratch.len() as u64) as usize;
        read_exact(src, &mut scratch[..take], at)?;
        dst.write_all(&scratch[..take])?;
        at += take as u64;
        n -= take as u64;
    }
    Ok(())
}

pub(crate) fn hash_prefix(path: &Path, n: u64) -> Result<u32> {
    use std::io::Read;
    const CHUNK: usize = 64 * 1024;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; CHUNK];
    let mut left = n;
    let mut hash = 0u32;
    let mut first = true;
    while left > 0 {
        let want = std::cmp::min(left, CHUNK as u64) as usize;
        file.read_exact(&mut buf[..want])?;
        hash = if first {
            first = false;
            crc32c::crc32c(&buf[..want])
        } else {
            crc32c::crc32c_append(hash, &buf[..want])
        };
        left -= want as u64;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal(dir: &std::path::Path) -> Wal { Wal::open(&dir.join("wal"), crate::io::IoMode::Buffered).unwrap() }

    #[test]
    fn records_replay_in_order() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"one").unwrap();
        w.append(RecKind::Put, b"two").unwrap();
        w.append(RecKind::Commit, b"").unwrap();
        w.sync_data().unwrap();
        drop(w);

        let got = wal(d.path()).replay().unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].2, b"one");
        assert_eq!(got[1].2, b"two");
        assert_eq!(got[2].1, RecKind::Commit);
    }

    #[test]
    fn a_torn_tail_stops_replay_at_the_last_good_frame() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"good").unwrap();
        w.sync_data().unwrap();
        let n = w.end_offset();
        drop(w);

        // Simulate a crash mid-frame: a complete header, a short payload.
        let mut f = std::fs::OpenOptions::new().write(true).open(d.path().join("wal")).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        f.seek(SeekFrom::Start(n)).unwrap();
        f.write_all(&[0u8; 12]).unwrap();   // header claiming a payload that isn't there
        f.sync_all().unwrap();

        let got = wal(d.path()).replay().unwrap();
        assert_eq!(got.len(), 1, "only the intact prefix replays");
    }

    /// THE test. sekejap lost five fsynced rows to exactly this: a writer that
    /// appended after a torn header, so replay swallowed the new records as the
    /// torn frame's payload.
    #[test]
    fn a_write_after_a_torn_tail_survives_the_next_open() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"before").unwrap();
        w.sync_data().unwrap();
        let n = w.end_offset();
        drop(w);

        let mut f = std::fs::OpenOptions::new().write(true).open(d.path().join("wal")).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        f.seek(SeekFrom::Start(n)).unwrap();
        let mut hdr = [0u8; HDR];
        hdr[0..4].copy_from_slice(&100u32.to_le_bytes());
        hdr[12] = RecKind::Put as u8;
        f.write_all(&hdr).unwrap();
        f.write_all(&[0xAA; 9]).unwrap();
        f.sync_all().unwrap();

        // Reopen and write MORE. This is the half every torn-tail test forgets.
        let mut w2 = wal(d.path());
        w2.append(RecKind::Put, b"after").unwrap();
        w2.sync_data().unwrap();
        drop(w2);

        let got = wal(d.path()).replay().unwrap();
        let payloads: Vec<&[u8]> = got.iter().map(|r| r.2.as_slice()).collect();
        assert_eq!(payloads, vec![b"before".as_ref(), b"after".as_ref()]);
    }

    /// Mechanism 1, pinned directly. The integration test above cannot see
    /// this one, because mechanism 2 would hold the property without it --
    /// and that is still exactly true after Task 19: deleting the
    /// `set_len(scan.end)` in `open_on` fails THIS test and nothing else in
    /// the suite (checked: 73 lib tests, the 200-trial corruption sweep,
    /// durability, recovery and bulk all stay green). It is kept as the
    /// documented redundancy this module exists to explain, and it is safe
    /// to keep now in a way it was not before: `End` is a classified
    /// answer: a bounded frame physically crosses EOF, or every remaining
    /// byte is zero. Here it is the zeros branch -- only "these cannot be a
    /// record anyone wrote" makes it an ending.
    #[test]
    fn an_all_zero_extension_is_a_clean_ending_and_is_truncated() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"one").unwrap();
        w.sync_data().unwrap();
        let end = w.end_offset();
        drop(w);

        let f = std::fs::OpenOptions::new().write(true)
            .open(d.path().join("wal")).unwrap();
        f.set_len(end + 4096).unwrap();   // orphaned bytes past the last frame
        f.sync_all().unwrap();
        drop(f);

        let w2 = wal(d.path());
        assert_eq!(w2.end_offset(), end);
        assert_eq!(
            std::fs::metadata(d.path().join("wal")).unwrap().len(), end,
            "open must not leave orphaned bytes on disk"
        );
    }

    /// Mechanism 2, pinned directly. Bytes are added past the log's logical end
    /// AFTER open, so mechanism 1 has already run and cannot mask this.
    #[test]
    fn append_lands_at_the_scanned_end_not_at_physical_eof() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"one").unwrap();
        w.sync_data().unwrap();
        let end = w.end_offset();

        {
            let f = std::fs::OpenOptions::new().write(true)
                .open(d.path().join("wal")).unwrap();
            f.set_len(end + 4096).unwrap();
            f.sync_all().unwrap();
        }

        w.append(RecKind::Put, b"two").unwrap();
        w.sync_data().unwrap();

        // Read the bytes off disk, bypassing the Wal's own bookkeeping.
        // Asserting on `end_offset()` here would be tautological: `self.end` is
        // incremented unconditionally by `append`, whatever offset the write
        // actually landed at, so that assertion passes identically for a broken
        // implementation. Only the file can say where the frame really went.
        let raw = std::fs::read(d.path().join("wal")).unwrap();
        let hdr = &raw[end as usize..end as usize + HDR];
        assert_eq!(
            u32::from_le_bytes(hdr[0..4].try_into().unwrap()), 3,
            "the second frame must physically start at the scanned end"
        );
        assert_eq!(hdr[12], RecKind::Put as u8);

        let got = wal(d.path()).replay().unwrap();
        let payloads: Vec<&[u8]> = got.iter().map(|r| r.2.as_slice()).collect();
        assert_eq!(payloads, vec![b"one".as_ref(), b"two".as_ref()]);
    }

    #[test]
    fn non_zero_garbage_shorter_than_a_frame_is_damage_and_is_preserved() {
        // The whole 1..=7 range, not a single representative. Short is not
        // evidence of an interrupted write: every non-zero byte remains
        // evidence until a complete physical frame can be ruled out.
        for len in [1usize, 7, 19, 20, 31, 100] {
            let d = tempfile::tempdir().unwrap();
            let stub: Vec<u8> = (0..len).map(|i| (i + 1) as u8).collect();
            std::fs::write(d.path().join("wal"), &stub).unwrap();
            assert!(matches!(
                Wal::open(&d.path().join("wal"), IoMode::Buffered),
                Err(crate::Error::CorruptWal { offset: 0, .. })
            ));
            assert_eq!(std::fs::read(d.path().join("wal")).unwrap(), stub,
                       "a refused {len}-byte tail must remain byte-for-byte intact");
        }
    }

    #[test]
    fn a_log_payload_larger_than_the_writer_max_is_refused_before_allocation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct HeaderOnly {
            hdr: [u8; HDR],
            len: u64,
            payload_read: Arc<AtomicBool>,
        }
        impl FileIo for HeaderOnly {
            fn requires_alignment(&self) -> bool { false }
            fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
                if off == 0 && buf.len() == HDR {
                    buf.copy_from_slice(&self.hdr);
                    return Ok(());
                }
                self.payload_read.store(true, Ordering::Relaxed);
                Err(std::io::Error::other("payload read must not happen").into())
            }
            fn write_at(&self, _: &[u8], _: u64) -> Result<()> { Ok(()) }
            fn sync_data(&self) -> Result<()> { Ok(()) }
            fn sync_full(&self) -> Result<()> { Ok(()) }
            fn sync_full_primitive(&self) -> &'static str { "test" }
            fn sync_dir(&self) -> Result<()> { Ok(()) }
            fn len(&self) -> Result<u64> { Ok(self.len) }
            fn set_len(&self, _: u64) -> Result<()> { Ok(()) }
        }

        let plen = u32::MAX;
        let mut hdr = [0u8; HDR];
        hdr[0..4].copy_from_slice(&plen.to_le_bytes());
        hdr[12] = RecKind::Put as u8;
        let payload_read = Arc::new(AtomicBool::new(false));
        let f = HeaderOnly {
            hdr,
            len: HDR as u64 + plen as u64,
            payload_read: payload_read.clone(),
        };
        assert!(matches!(Wal::open_on(Box::new(f)), Err(crate::Error::CorruptWal { offset: 0, .. })));
        assert!(!payload_read.load(Ordering::Relaxed),
                "an off-disk length above the writer maximum must be rejected before allocation/read");
    }

    #[test]
    fn an_exhausted_lsn_is_refused_with_checked_arithmetic() {
        let d = tempfile::tempdir().unwrap();
        let mut hdr = [0u8; HDR];
        hdr[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        hdr[12] = RecKind::Commit as u8;
        let crc = Wal::frame_crc(&hdr, &[]);
        hdr[16..20].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(d.path().join("wal"), hdr).unwrap();
        assert!(matches!(
            Wal::open(&d.path().join("wal"), IoMode::Buffered),
            Err(crate::Error::CorruptWal { offset: 0, .. })
        ));
    }

    // -- Task 19: the exit taxonomy (Task 17 final review, F4) --

    /// A `FileIo` that fails one specific read. `scan` used to swallow this:
    /// `read_exact(..).is_err()` was one of its eight `break`s, so an EIO
    /// came back as a clean `end` and `open`'s `set_len` then erased
    /// everything behind it (measured: 640 of 1,280 bytes on a single
    /// injected failure). An I/O error is not a statement about what the
    /// log contains.
    struct FailReadAt { inner: Box<dyn FileIo>, fail_at: u64 }
    impl FileIo for FailReadAt {
        fn requires_alignment(&self) -> bool { self.inner.requires_alignment() }
        fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
            if off == self.fail_at {
                return Err(std::io::Error::other("injected read failure").into());
            }
            self.inner.read_at(buf, off)
        }
        fn write_at(&self, buf: &[u8], off: u64) -> Result<()> { self.inner.write_at(buf, off) }
        fn sync_data(&self) -> Result<()> { self.inner.sync_data() }
        fn sync_full(&self) -> Result<()> { self.inner.sync_full() }
        fn sync_full_primitive(&self) -> &'static str { self.inner.sync_full_primitive() }
        fn sync_dir(&self) -> Result<()> { self.inner.sync_dir() }
        fn len(&self) -> Result<u64> { self.inner.len() }
        fn set_len(&self, n: u64) -> Result<()> { self.inner.set_len(n) }
    }

    /// Ten equal frames; the header read for frame 5 fails. The open must
    /// come back `Err`, and -- the half that actually cost data -- the file
    /// must still be ten frames long. With the `read_exact(..).is_err() =>
    /// break` this replaces, `scan` returns `end = 640` and `set_len`
    /// truncates the last five frames away, `open` reporting `Ok`.
    #[test]
    fn an_io_error_on_a_header_read_is_an_error_not_a_clean_end() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        let p = vec![b'A'; 100];
        for _ in 0..10 { w.append(RecKind::Put, &p).unwrap(); }
        w.sync_data().unwrap();
        let full = w.end_offset();
        drop(w);
        let frame = HDR as u64 + p.len() as u64;
        let midpoint = frame * 5;

        let (real, _) = open_file(&d.path().join("wal"), IoMode::Buffered).unwrap();
        let failing = Box::new(FailReadAt { inner: real, fail_at: midpoint });
        match Wal::open_on(failing) {
            Err(crate::Error::Io(_)) => {}
            Err(e) => panic!("an unreadable header must surface as an I/O error, got {e:?}"),
            Ok(_) => panic!("an unreadable header must not be reported as a clean end of log"),
        }
        assert_eq!(
            std::fs::metadata(d.path().join("wal")).unwrap().len(), full,
            "a read that FAILED must not be able to shorten the log -- {full} bytes were on \
             disk and the reader could not read one header of them"
        );
    }

    /// One flipped bit in the middle of a log, with committed frames behind
    /// it. This is the measured 1,499-rows-for-one-byte case at its own
    /// level: the frame fails CRC, but there are thousands of bytes behind
    /// it -- far more than one interrupted write could have left -- so it is
    /// damage, not an ending. The open must refuse, and every byte must
    /// still be there afterwards for `recover()` to work with.
    #[test]
    fn damage_with_more_log_behind_it_refuses_the_open_and_keeps_every_byte() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        let p = vec![b'A'; 100];
        for _ in 0..100 { w.append(RecKind::Put, &p).unwrap(); }
        w.sync_data().unwrap();
        drop(w);

        let path = d.path().join("wal");
        let frame = HDR + p.len();
        let victim = frame * 50;          // the header of frame 50
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[victim + 4] ^= 0x01;        // one bit of the LSN: only the CRC can see it
        std::fs::write(&path, &bytes).unwrap();

        match Wal::open(&path, IoMode::Buffered) {
            Err(crate::Error::CorruptWal { offset, .. }) => {
                assert_eq!(offset as usize, victim, "the refusal must name where it stopped");
            }
            Err(e) => panic!("expected CorruptWal, got {e:?}"),
            Ok(_) => panic!("damage with committed frames behind it must not open"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(), bytes,
            "a refused open must leave the log byte for byte as it found it"
        );
    }

    /// The other half of the CRC-first rule. A frame that VERIFIES cannot
    /// have come from an interrupted write, so an unknown kind byte on it is
    /// damage however small the remainder is -- and, conversely, the kind
    /// byte is never consulted before the CRC has spoken, so a torn tail
    /// with a garbage kind byte is still an ordinary ending (the test above
    /// this one and `a_write_after_a_torn_tail_survives_the_next_open`).
    #[test]
    fn a_verified_frame_naming_an_unknown_kind_is_damage() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"good").unwrap();
        w.sync_data().unwrap();
        let end = w.end_offset();
        drop(w);

        // A complete, correctly checksummed frame whose kind byte is 9.
        let mut hdr = [0u8; HDR];
        let payload = b"future".to_vec();
        hdr[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        hdr[4..12].copy_from_slice(&7u64.to_le_bytes());
        hdr[12] = 9;
        let c = Wal::frame_crc(&hdr, &payload);
        hdr[16..20].copy_from_slice(&c.to_le_bytes());
        let mut bytes = std::fs::read(d.path().join("wal")).unwrap();
        bytes.extend_from_slice(&hdr);
        bytes.extend_from_slice(&payload);
        std::fs::write(d.path().join("wal"), &bytes).unwrap();

        match Wal::open(&d.path().join("wal"), IoMode::Buffered) {
            Err(crate::Error::CorruptWal { offset, .. }) => assert_eq!(offset, end),
            Err(e) => panic!("expected CorruptWal, got {e:?}"),
            Ok(_) => panic!("a verified frame naming an unknown kind must not be walked past"),
        }
        assert_eq!(std::fs::read(d.path().join("wal")).unwrap(), bytes,
                   "and it must not have been truncated on the way out");
    }

    /// The recovery read is independently checksummed. A byte changing after
    /// `open` performed its scan must be refused before the payload is used.
    #[test]
    fn replay_refuses_a_byte_changed_after_the_opening_crc_walk() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"\x01\0key-value").unwrap();
        w.append(RecKind::Commit, b"").unwrap();
        w.sync_data().unwrap();
        drop(w);

        let w = wal(d.path());
        let committed = w.committed_end().unwrap();
        let path = d.path().join("wal");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[HDR + 3] ^= 1;
        std::fs::write(path, bytes).unwrap();

        assert!(matches!(
            w.record_at(0, committed),
            Err(crate::Error::CorruptWal { offset: 0, .. })
        ));
    }

    /// `MAX_FRAME_BYTES` is only as good as the claim that no writer exceeds
    /// it, and the last two bounds in this file were wrong by two bytes and
    /// by a whole record respectively. So assert it per `RecKind`, against
    /// the widest payload each writer can actually construct, rather than
    /// against a comment. Every current logical writer is stopped by
    /// `Wal::append`'s payload bound; `Commit` is empty; `PageImage` is never
    /// appended by anything, and if that changes this test is what fails.
    #[test]
    fn writers_cannot_emit_a_frame_larger_than_max_frame_bytes() {
        let widest_put = MAX_PAYLOAD_BYTES as usize;
        let widest_put_empty_batch = MAX_PAYLOAD_BYTES as usize;
        let widest_delete = MAX_PAYLOAD_BYTES as usize;
        let widest_commit = 0usize;
        for (kind, payload) in [
            ("Put", widest_put),
            ("PutEmptyBatch", widest_put_empty_batch),
            ("Delete", widest_delete),
            ("Commit", widest_commit),
        ] {
            assert!(
                (HDR + payload) as u64 <= MAX_FRAME_BYTES,
                "a {kind} frame can reach {} bytes, past MAX_FRAME_BYTES ({MAX_FRAME_BYTES})",
                HDR + payload
            );
        }
    }

    /// A real, partial frame at the physical end -- the case the whole
    /// module exists for -- must still be an ordinary ending: verification
    /// fails because the bounded header declares bytes beyond physical EOF,
    /// so nothing behind it can be lost by truncating. 300 bytes of a
    /// 1,000-byte frame.
    #[test]
    fn a_partial_frame_at_the_end_is_an_ending_not_damage() {
        let d = tempfile::tempdir().unwrap();
        let mut w = wal(d.path());
        w.append(RecKind::Put, b"committed").unwrap();
        w.sync_data().unwrap();
        let end = w.end_offset();
        drop(w);

        let mut hdr = [0u8; HDR];
        hdr[0..4].copy_from_slice(&1000u32.to_le_bytes());
        hdr[12] = RecKind::Put as u8;
        let mut bytes = std::fs::read(d.path().join("wal")).unwrap();
        bytes.extend_from_slice(&hdr);
        bytes.extend_from_slice(&vec![b'x'; 280]);   // 300 of the 1,020 bytes landed
        std::fs::write(d.path().join("wal"), &bytes).unwrap();

        let w2 = Wal::open(&d.path().join("wal"), IoMode::Buffered)
            .expect("an interrupted write is how this log ends, not damage");
        assert_eq!(w2.end_offset(), end);
        assert_eq!(w2.replay().unwrap().len(), 1);
        assert_eq!(std::fs::metadata(d.path().join("wal")).unwrap().len(), end,
                   "only the incomplete physical frame is truncated");
    }
}
