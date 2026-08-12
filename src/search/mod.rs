//! # Positional full-text search — the `SEARCH()` index
//!
//! This is the index behind the `SEARCH('query')` surface: a relevance-ranked
//! text search that also knows *where* each word appears, so it can reward
//! phrase proximity and exact matches (like a small search engine), not just
//! "contains the word". It's distinct from BM25 (pure relevance) and GIN
//! (substring `ILIKE`).
//!
//! - [`index`] — the in-memory index: an FST term dictionary, per-term postings
//!   (which docs, and at which word positions), and the cascade ranking that
//!   scores results. Also the resident/mmap ([`SearchIndex`]) split.
//! - `disk` — persist that index to a file and serve it back via mmap in paged
//!   mode, so a reopen doesn't rebuild it.

mod disk;
pub mod index;

pub use index::{SearchIndex, DocFields};
