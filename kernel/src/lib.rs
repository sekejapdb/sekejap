#![forbid(unsafe_op_in_unsafe_fn)]

pub mod btree;
pub mod budget;
pub mod limits;
pub mod bulk;
pub mod io;
pub mod keys;
pub mod graph;
pub mod meta;
pub mod page;
pub mod pool;
pub mod recover;
pub mod vecquant;
pub mod text;
pub mod score;
pub mod readers;
pub mod spatial;
pub mod geomath;
pub mod nav;
pub mod store;
#[cfg(test)]
mod test_support;
pub mod wal;
#[doc(hidden)]
pub mod write_stats;
/// Structural verification. `verify_published_tree` is public so a live
/// database's shape can be proven after a graft; the rest stays internal.
pub mod verify;
#[cfg(feature = "write-trace")]
pub mod write_trace;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A page failed verification. Carries the page number that was asked for.
    Corrupt { page_no: u32, why: &'static str },
    /// A structural limit was hit, e.g. a record too large for a page.
    TooLarge,
    /// A mutation was attempted on a snapshot reader (2f). Readers serve
    /// the state of one published generation; every write path refuses.
    ReadOnly,
    /// Another live writer already owns the data file. The lock is advisory
    /// and attached to that writer's file descriptor, so dropping the handle
    /// or process exit releases it; snapshot readers do not take this lock.
    WriterLocked,
    /// The write-ahead log holds something at `offset` that cannot be
    /// accepted AND cannot be treated as the log simply ending there --
    /// `Wal::scan` classified it `Stop::Damaged` (see that type). `open`
    /// refuses rather than truncating past it, because the bytes behind it
    /// may be committed frames and a reader that is unsure has no business
    /// deleting them (Law 3).
    ///
    /// NOT terminal, and that is half the design rather than a detail (Law
    /// 5): a refusal with no way to clear it is as unrecoverable as a
    /// deletion. `recover()` is the way through -- it copies and hashes the
    /// whole log as `wal.corrupt.N`, resynchronises later committed regions
    /// into a verified live log, and the store opens. A previous round shipped this
    /// refusal without that path and turned 29 of 400 single-bit flips into
    /// stores that would never open again.
    ///
    /// Deliberately not `Corrupt`: a WAL frame has no page number, and
    /// reusing `page_no` for a byte offset would mislabel what failed.
    CorruptWal { offset: u64, why: &'static str },
    /// A `Store` whose logged write or checkpoint failed partway through
    /// refuses every further write. A tree error may escape after a leaf was
    /// compacted or split but before its replacement or parent was installed;
    /// `flush_all` clears each frame's dirty bit
    /// BEFORE its barrier is issued, so a barrier that fails does not mean
    /// the writes never happened -- those bytes can already be sitting in
    /// the OS page cache, forgotten by our own bookkeeping, and reach the
    /// disk anyway via later, unrelated writeback with no further fsync from
    /// us. The store therefore cannot say whether its last checkpoint took
    /// effect, and the pages it believes clean may not be durable.
    ///
    /// What makes continuing actively dangerous rather than merely
    /// uncertain is `checkpoint`'s last step: `Wal::rotate` DELETES the log.
    /// A second checkpoint would flush nothing (those frames are marked
    /// clean), issue a barrier that may well return `Ok` this time -- a
    /// failed `fsync` is reported once and the kernel then forgets it -- and
    /// go on to discard the one remaining copy of records whose pages never
    /// reached the medium. That is Law 3 exactly: something that can be
    /// wrong about what exists, deleting. So every writer refuses:
    /// `put`, `delete`, `commit`, `checkpoint` and `bulk_load` alike.
    ///
    /// NOT a dead end (Law 5). The flag is per-instance and never persisted:
    /// dropping the `Store` and calling `Store::open` again clears it, and
    /// that reopen is not a way of ignoring the problem -- it re-reads
    /// `Meta` from disk and replays the log, which is what re-establishes
    /// what is actually durable. `recover()` is available for the case where
    /// the reopen itself finds damage.
    StorePoisoned,
    /// A memory reservation could not be granted.
    OutOfBudget,
    /// A configured resource ceiling refused work. A partially changed writer
    /// must be dropped and reopened; committed metadata and readers survive.
    ResourceLimit(&'static str),
    /// A bulk load's input contained two entries with the same key. Not
    /// `Corrupt` -- page 0 is the superblock, and naming it for a condition
    /// that has nothing to do with a page reads as structural damage in a
    /// log when it is really just an input the caller must deduplicate.
    DuplicateKey,
    /// A packed range can only be grafted where the live tree has no key.
    /// Overwriting through this path would bypass ordinary update semantics.
    RangeNotEmpty,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e) }
}

/// Every variant says what failed AND where the way out is, because these
/// strings are what a wrapper user sees: a refusal with no route forward
/// reads as a dead end (Law 5), and the `Display` text is often the only
/// part of that law a caller ever meets.
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corrupt { page_no, why } =>
                write!(f, "page {page_no} failed verification ({why}); recover() copies the \
                           damage aside and reopens what is readable"),
            Error::TooLarge => write!(f, "record too large for a page"),
            Error::ReadOnly => write!(f, "this handle is a snapshot reader; snapshot readers \
                                          serve one published generation and never write"),
            Error::WriterLocked => write!(f, "database already has an active writer; the \
                                          exclusive writer lock is held (close that writer or \
                                          wait for its process to exit; read-only snapshots \
                                          remain available)"),
            Error::CorruptWal { offset, why } =>
                write!(f, "write-ahead log unusable at offset {offset} ({why}); recover() \
                           preserves the whole log as wal.corrupt.N and opens the readable \
                           prefix"),
            Error::StorePoisoned =>
                write!(f, "a logged write or checkpoint failed partway, so this handle cannot say what is \
                           durable and refuses every further write; drop it and open again \
                           to re-read the meta and replay the log"),
            Error::OutOfBudget => write!(f, "memory reservation refused: cache budget exhausted"),
            Error::ResourceLimit(why) => write!(f, "resource limit: {why}; reduce the transaction, release old snapshots, or export to a larger store"),
            Error::DuplicateKey => write!(f, "bulk load input contained a duplicate key"),
            Error::RangeNotEmpty => write!(f, "packed range overlaps keys already present in the live tree"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self { Error::Io(e) => Some(e), _ => None }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The gate's edge formula, shared so e3 and the SQLite harness traverse the
/// IDENTICAL logical graph: per src, 2 near edges (locality) + 2 far (cross).
pub fn bench_edges(src: u64, n: u64) -> [(u64, u64); 4] {
    let far1 = 1 + (src.wrapping_mul(2_654_435_761)) % n;
    let far2 = 1 + (src.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1) % n;
    // A distinct type per slot makes (src,ty,dst) unique by construction --
    // far1 can equal far2 and the keys still cannot collide.
    [
        (1, 1 + src % n),          // src+1 wrap
        (2, 1 + (src + 7) % n),
        (3, far1),
        (4, far2),
    ]
}
