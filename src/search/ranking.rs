//! # Cascade ranking — notes on how search results are ordered
//!
//! This file is documentation-only: the actual ranking lives in
//! `SearchIndex::score()` (in `index.rs`). It's recorded here because the idea is
//! worth understanding on its own.
//!
//! "Cascade ranking" (the Meilisearch approach) orders results by a *sequence*
//! of rules, each more important than the next: **words** (how many query words
//! matched) → **typo** (fewer spelling slips is better) → **proximity** (matched
//! words closer together) → **field_order** → **exactness**. The trick that makes
//! it a single sortable number: each rule produces a value in `[0.0, 1.0]` and is
//! placed in its own magnitude band (`1e12` down to `1e0`), so a better score on a
//! higher-priority rule always outweighs everything below it — lexicographic
//! ordering expressed as one `f64`.
