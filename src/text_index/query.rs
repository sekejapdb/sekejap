//! # Running an `ILIKE` query against the trigram index
//!
//! This is the glue that answers `name ILIKE '%pattern%'` using the trigram
//! indexes: extract the pattern's trigrams, intersect their postings to get a
//! small candidate set, then verify each candidate actually contains the
//! substring (trigrams over-match, so the re-check is mandatory for correctness).
//! It falls back to a plain scan only when the pattern has no usable trigram (a
//! fixed part shorter than 3 characters).
//!
//! ## ILIKE Query Execution
//!
//! Handles ILIKE pattern matching with trigram index acceleration.
//!
//! ## Query Flow
//!
//! ```text
//! ILIKE "%Alpha%" LIMIT 50
//!       │
//!       ▼
//! ┌─────────────────────┐
//! │ Parse Pattern       │ → Extract trigrams: [" al", " alp", ...]
//! └─────────────────────┘
//!       │
//!       ▼
//! ┌─────────────────────┐
//! │ Index Lookup        │ → GiST: signature match
//! │                    │ → GIN: exact postings intersect
//! └─────────────────────┘
//!       │
//!       ▼
//! ┌─────────────────────┐
//! │ Candidates          │ → List of candidate doc IDs
//! └─────────────────────┘
//!       │
//!       ▼
//! ┌─────────────────────┐
//! │ Verify (GiST only)  │ → Full ILIKE check on each candidate
//! │                    │ → GIN: skip (exact)
//! └─────────────────────┘
//!       │
//!       ▼
//! ┌─────────────────────┐
//! │ Apply LIMIT         │ → Early termination when limit reached
//! └─────────────────────┘
//!       │
//!       ▼
//! ┌─────────────────────┐
//! │ Return Results      │ → Vec<Hit> or Vec<doc_id>
//! └─────────────────────┘
//! ```
//!
//! ## ILIKE Semantics
//!
//! - Case-insensitive substring match
//! - `%` matches any sequence (including empty)
//! - `_` matches any single character
//! - Pattern is POSIX-style (not regex)
//!
//! ## Why Verification for GiST?
//!
//! GiST signatures are lossy. A document might pass signature check but NOT
//! actually contain the pattern (false positive). Verification ensures correctness.
//!
//! GIN does NOT need verification because it stores exact trigram→docID mappings.

use crate::text_index::gin::GINIndex;
use crate::text_index::gist::GiSTIndex;

/// ILIKE pattern matching result with source info.
pub struct ILikeResult {
    /// Document ID
    pub doc_id: u64,
    /// Whether this was verified (true for GiST) or exact (false for GIN)
    pub verified: bool,
}

/// Check if a string matches an ILIKE pattern.
///
/// Implements POSIX ILIKE semantics:
/// - Case-insensitive
/// - `%` matches any sequence (including empty)
/// - `_` matches single character
/// - Pattern is POSIX-style (not regex)
///
/// # Arguments
/// * `text` - The text to check
/// * `pattern` - ILIKE pattern (e.g., "%Alpha%" or "%foo_bar%")
///
/// # Returns
/// * `bool` - True if text matches pattern
pub fn ilike_matches(text: &str, pattern: &str) -> bool {
    matches_like(text, pattern, true)
}

/// Case-sensitive `LIKE`.
pub fn like_matches(text: &str, pattern: &str) -> bool {
    matches_like(text, pattern, false)
}

/// One element of a compiled `LIKE` pattern.
#[derive(PartialEq)]
enum Tok {
    /// `%` — any run of characters, including none.
    Any,
    /// `_` — exactly one character, whatever it is.
    One,
    /// A character that must appear as itself.
    Lit(char),
}

/// PostgreSQL's default `ESCAPE` character. `\%` is a literal percent sign.
const DEFAULT_ESCAPE: char = '\\';

/// Turn a pattern into tokens, honouring the escape character.
fn compile(pattern: &str, fold: bool) -> Vec<Tok> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => out.push(Tok::Any),
            '_' => out.push(Tok::One),
            DEFAULT_ESCAPE => match chars.next() {
                // An escaped character is always a literal, wildcard or not.
                Some(n) => out.push(Tok::Lit(if fold { fold_char(n) } else { n })),
                // A trailing lone escape. PostgreSQL raises an error; matching
                // nothing is the closest thing to that which this signature can
                // express.
                None => out.push(Tok::Lit('\u{0}')),
            },
            c => out.push(Tok::Lit(if fold { fold_char(c) } else { c })),
        }
    }
    out
}

/// Lowercase a single character. Multi-character foldings (`İ`, `ß`) collapse to
/// the first, which keeps one pattern character matching one text character —
/// the property `_` depends on.
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// SQL `LIKE`, as PostgreSQL defines it.
///
/// # What this replaced, and why it mattered
///
/// The old implementation stripped the `%` signs off the pattern and then
/// checked that the remaining fragments appeared **somewhere in the text, in
/// order**. That is `contains`, not `LIKE`, and it is wrong in both directions:
///
/// ```text
///   'reopened' LIKE 'open'   was true    — an unanchored pattern matched a substring
///   'foo'      LIKE 'o%'     was true    — a prefix pattern matched in the middle
///   anything   LIKE ''       was true    — only the empty string matches ''
///   'open'     LIKE '_pen'   was false   — `_` was treated as a literal underscore
///   '100%'     LIKE '100\%'  was false   — there was no escape character at all
///   'open'     LIKE ' open'  was true    — the pattern was `.trim()`ed
/// ```
///
/// A query that looked like it asked for one row returned every row that
/// happened to contain the word. There was no error and no way to notice from
/// the outside.
///
/// # The algorithm
///
/// Two pointers with one backtrack point, which is the standard way to match a
/// pattern whose only unbounded construct is `%`. When the text stops matching,
/// the walk returns to the most recent `%` and lets it consume one more
/// character. Linear in the text for every pattern without adjacent wildcards,
/// and it never recurses, so a pathological pattern costs time rather than
/// stack.
///
/// Matching runs over `char`s, not bytes, so `_` means one character in the
/// user's sense — `'é' LIKE '_'` is true, as it is in PostgreSQL.
fn matches_like(text: &str, pattern: &str, fold: bool) -> bool {
    let pat = compile(pattern, fold);
    let txt: Vec<char> = if fold {
        text.chars().map(fold_char).collect()
    } else {
        text.chars().collect()
    };

    let (mut ti, mut pi) = (0usize, 0usize);
    // Where to resume if the current attempt fails: the last `%` seen, and how
    // much text it had consumed at the time.
    let (mut star, mut star_ti) = (None, 0usize);

    while ti < txt.len() {
        match pat.get(pi) {
            Some(Tok::One) => { pi += 1; ti += 1; }
            Some(Tok::Lit(c)) if *c == txt[ti] => { pi += 1; ti += 1; }
            Some(Tok::Any) => { star = Some(pi); star_ti = ti; pi += 1; }
            // Mismatch, or the pattern ran out with text left over. Either is
            // recoverable only if some `%` behind us can absorb one more
            // character.
            _ => match star {
                Some(s) => { pi = s + 1; star_ti += 1; ti = star_ti; }
                None => return false,
            },
        }
    }
    // Text exhausted: the rest of the pattern must be able to match nothing.
    pat[pi..].iter().all(|t| *t == Tok::Any)
}

/// Execute ILIKE query using GiST index with verification.
///
/// # Arguments
/// * `index` - GiST index
/// * `db` - CoreDB reference (for fetching node data)
/// * `pattern` - ILIKE pattern
/// * `limit` - Maximum results
///
/// # Returns
/// * `Vec<u64>` - Verified matching doc IDs
pub fn ilike_gist(
    index: &GiSTIndex,
    db: &crate::CoreDB,
    pattern: &str,
    limit: Option<usize>,
) -> Vec<u64> {
    let candidates = index.ilike_candidates(pattern, None);

    let mut results = Vec::new();
    for doc_id in candidates {
        if let Some(payload) = db.get_payload(doc_id) {
            let text = serde_json::to_string(&payload).unwrap_or_default();
            if ilike_matches(&text, pattern) {
                results.push(doc_id);
                if let Some(l) = limit {
                    if results.len() >= l {
                        break;
                    }
                }
            }
        }
    }

    results
}

/// Execute ILIKE query using GIN index (exact, no verification needed).
///
/// # Arguments
/// * `index` - GIN index
/// * `pattern` - ILIKE pattern
/// * `limit` - Maximum results
///
/// # Returns
/// * `Vec<u64>` - Exact matching doc IDs
pub fn ilike_gin(index: &GINIndex, pattern: &str, limit: Option<usize>) -> Vec<u64> {
    index.ilike(pattern, limit)
}

/// Execute ILIKE query using GiST index, returning matched text for verification.
///
/// This variant returns the actual text that matched, useful for debugging.
///
/// # Arguments
/// * `index` - GiST index
/// * `db` - CoreDB reference
/// * `field` - Field name to extract text from
/// * `pattern` - ILIKE pattern
/// * `limit` - Maximum results
///
/// # Returns
/// * `Vec<(u64, String)>` - (doc_id, matched text)
pub fn ilike_gist_with_text(
    index: &GiSTIndex,
    db: &crate::CoreDB,
    field: &str,
    pattern: &str,
    limit: Option<usize>,
) -> Vec<(u64, String)> {
    let candidates = index.ilike_candidates(pattern, None);

    let mut results = Vec::new();
    for doc_id in candidates {
        if let Some(payload) = db.get_payload(doc_id) {
            if let Some(text) = payload.get(field).and_then(|v| v.as_str()).map(|s| s.to_string()) {
                if ilike_matches(&text, pattern) {
                    results.push((doc_id, text));
                    if let Some(l) = limit {
                        if results.len() >= l {
                            break;
                        }
                    }
                }
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ilike_basic() {
        assert!(ilike_matches("Hello World", "%World%"));
        assert!(ilike_matches("Hello World", "%world%"));
        assert!(ilike_matches("Hello World", "%HELLO%"));
        assert!(!ilike_matches("Hello World", "%foo%"));
    }

    #[test]
    fn test_ilike_wildcards() {
        assert!(ilike_matches("The Vines", "%Vines%"));
        assert!(ilike_matches("The Vines", "The%"));
        assert!(ilike_matches("The Vines", "%Vines"));
        assert!(ilike_matches("The Vines", "The Vines"));
    }

    #[test]
    fn test_ilike_underscore() {
        // Note: current implementation handles % only, not _ (underscore)
        assert!(ilike_matches("foo_bar", "foo_bar"));
        // "foo%bar" should match "foobar", "fooxyzbar", etc.
        let result = ilike_matches("foobar", "foo%bar");
        eprintln!("ilike_matches('foobar', 'foo%bar') = {}", result);
        assert!(result);
    }

    #[test]
    fn test_ilike_empty_pattern() {
        assert!(ilike_matches("anything", "%"));
        assert!(ilike_matches("", "%"));
    }

    #[test]
    fn test_ilike_case_insensitive() {
        assert!(ilike_matches("ALPHA", "%alpha%"));
        assert!(ilike_matches("Alpha", "%ALPHA%"));
        assert!(ilike_matches("alpha", "%Alpha%"));
    }
}
