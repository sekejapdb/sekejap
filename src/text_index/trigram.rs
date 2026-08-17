//! # Trigram extraction — chopping text into 3-character shingles
//!
//! This is the small, pure-math counterpart to `ginstore.rs`: given a piece of
//! text, produce its **trigrams** — every 3-character window. The index in
//! `ginstore.rs` stores, per trigram, which documents contain it; this file is
//! what turns a string into that list of trigrams in the first place, for both
//! the documents being indexed and the `%…%` pattern of an `ILIKE` query.
//!
//! ## The sliding window (the core algorithm)
//!
//! Slide a width-3 window across the string one character at a time and record
//! each window. `"alpha"` → `alp`, `lph`, `pha`. That's it — a string of length
//! `n` yields `n - 2` trigrams. Strings shorter than 3 characters yield none.
//!
//! ## Why the spaces? (the clever detail, from PostgreSQL's pg_trgm)
//!
//! We first pad the text with a leading and trailing space: `"alpha"` becomes
//! `" alpha "`, giving the extra trigrams `" al"` and `"ha "`. Those *boundary*
//! trigrams encode "this is the start/end of the value", which lets an
//! **anchored** query like `name ILIKE 'alpha%'` (starts-with) use the index
//! precisely. An *unanchored* `%alpha%` doesn't want the boundary trigrams
//! (`alpha` could sit anywhere inside a longer value), so the pattern extractor
//! only pads the edges that are actually anchored — that asymmetry is the whole
//! subtlety of [`extract_pattern_trigrams`].
//!
//! ## Core components
//!
//! - [`extract_trigrams`] — trigrams of a document value (always padded).
//! - [`extract_pattern_trigrams`] — trigrams a `%…%` pattern requires, padding
//!   only anchored edges; these are the trigrams that MUST appear in any match.
//! - [`hash_trigram`] — a fast FNV-1a hash so a trigram becomes a `u32` key.
//! - [`dedup_trigrams`] — drops repeats while keeping order (fewer index lookups).

use std::collections::HashSet;

/// All trigrams of a document value, space-padded on both ends.
///
/// Lowercases first (so matching is case-insensitive), space-pads to capture the
/// start/end boundary trigrams, then slides a width-3 window. `"Alpha"` →
/// `" al", "alp", "lph", "pha", "ha "`. A value shorter than 3 characters has no
/// trigrams and returns an empty `Vec`.
pub fn extract_trigrams(text: &str) -> Vec<String> {
    // Lowercase up front — the index is case-insensitive, so "Cat" and "cat"
    // must produce the same trigrams.
    let lower = text.to_lowercase();
    // Collect into a `Vec<char>`: a Rust `&str` is UTF-8 *bytes*, but a "3-char
    // window" means 3 *characters*, and one character can be several bytes. So we
    // work over `char`s to slide correctly on non-ASCII text.
    let chars: Vec<char> = lower.chars().collect();
    let len = chars.len();

    if len < 3 {
        return vec![]; // nothing to slide a width-3 window over
    }

    // Build the padded character sequence " …text… ". `with_capacity` pre-sizes
    // the Vec (len + the two spaces) to avoid re-allocations while pushing.
    let mut result = Vec::with_capacity(len + 2);
    result.push(' '); // leading boundary marker
    for c in &chars {
        result.push(*c);
    }
    result.push(' '); // trailing boundary marker

    // `.windows(3)` is a standard slice method: it yields every overlapping
    // 3-element sub-slice ([0,1,2], then [1,2,3], …) — exactly a sliding window.
    let mut trigrams = Vec::with_capacity(len);
    for window in result.windows(3) {
        // `iter().collect::<String>()` turns the 3 chars back into a `String`.
        trigrams.push(window.iter().collect::<String>());
    }

    trigrams
}

/// Whether a trigram index can be trusted to answer this pattern at all.
///
/// The extractor below treats every character that is not `%` as literal text to
/// look up, which is right for `%` and wrong for everything else a pattern can
/// contain:
///
/// * `_` matches any single character, so a segment holding one asks the index
///   for trigrams containing a literal underscore. Those are not there, the
///   index reports no matches, and rows that do match are never looked at.
///   `'open' LIKE '_pen'` is the smallest example.
/// * an escaped wildcard (`\%`) is literal text, but the backslash is not, so
///   the segment fed to the index contains a character the value does not.
///
/// A pattern like that is not unindexable in principle — it is unindexable by
/// *this* extractor — so the honest answer is to say so and let the caller scan.
/// The alternative is an index that silently subtracts rows, which is the one
/// thing an index may never do.
/// * a pattern with no segment of three characters yields no trigrams at all, so
///   the lookup is empty for a reason that has nothing to do with the data.
///   `LIKE ''` matches exactly the empty string; through the index it matched
///   nothing.
pub fn pattern_is_indexable(pattern: &str) -> bool {
    !pattern.contains('_')
        && !pattern.contains('\\')
        && !extract_pattern_trigrams(pattern).is_empty()
}

/// Trigrams a `%…%` pattern requires — the ones that MUST appear in any match.
///
/// An `ILIKE` pattern has `%` wildcards ("any run of characters"), so we keep
/// only the fixed literal segments between the wildcards and take *their*
/// trigrams. The subtle part is padding (see the module docs): a `%` next to a
/// segment means that side is unanchored, so we do NOT add the boundary space
/// there. `'Alpha%'` (starts-with) pads only the left → includes `" al"`;
/// `'%Alpha%'` (contains) pads neither → just the interior `alp`, `lph`, `pha`.
///
/// Segments shorter than 3 characters carry no trigram signal and are skipped —
/// which is why a query like `%ab%` can't use the index and falls back to a scan.
///
/// ```
/// use sekejap::text_index::trigram::extract_pattern_trigrams;
/// // "%Alpha%" is unanchored on both sides → interior trigrams only, no spaces.
/// let trigrams = extract_pattern_trigrams("%Alpha%");
/// assert!(trigrams.contains(&"alp".to_string()));
/// assert!(!trigrams.contains(&" al".to_string())); // no leading-space boundary
/// ```
pub fn extract_pattern_trigrams(pattern: &str) -> Vec<String> {
    // Split pattern on wildcards and collect fixed literal segments.
    // Each segment separated by % can match at any position in the document,
    // so we must NOT add space-padding — only interior trigrams are valid.
    // Space-padding is only appropriate when a segment is anchored to the
    // start or end of the value (no leading/trailing %).
    let has_leading_pct  = pattern.starts_with('%');
    let has_trailing_pct = pattern.ends_with('%');

    // Strip leading/trailing wildcards and split remaining on %
    let inner = pattern.trim_matches(|c| c == '%' || c == '_');
    let segments: Vec<&str> = inner.split('%').filter(|s| s.len() >= 3).collect();

    if segments.is_empty() {
        return vec![];
    }

    let mut all_trigrams: Vec<String> = Vec::new();

    for (i, seg) in segments.iter().enumerate() {
        let lower = seg.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        if chars.len() < 3 { continue; }

        // Decide whether to space-pad this segment's edges:
        // - pad start only if this is the first segment AND no leading %
        // - pad end only if this is the last segment AND no trailing %
        let pad_start = i == 0 && !has_leading_pct;
        let pad_end   = i == segments.len() - 1 && !has_trailing_pct;

        let mut padded: Vec<char> = Vec::with_capacity(chars.len() + 2);
        if pad_start { padded.push(' '); }
        padded.extend_from_slice(&chars);
        if pad_end   { padded.push(' '); }

        for window in padded.windows(3) {
            all_trigrams.push(window.iter().collect());
        }
    }

    all_trigrams
}

/// Turn a trigram into a compact `u32` key with the FNV-1a hash.
///
/// The index stores trigrams by a `u32` hash rather than the string itself
/// (smaller, faster to compare). FNV-1a is a tiny non-cryptographic hash: start
/// from a fixed seed and, for each byte, XOR it in then multiply by a fixed
/// prime. It's fast and spreads short 3-byte strings evenly across the `u32`
/// range, which is all an index bucket key needs.
pub fn hash_trigram(trigram: &str) -> u32 {
    let bytes = trigram.as_bytes(); // hash over raw UTF-8 bytes
    let mut hash: u32 = 2166136261; // FNV offset basis (the standard seed)
    for &b in bytes {
        hash ^= b as u32; // mix the byte in with XOR
        // `wrapping_mul` multiplies and lets the result overflow-wrap around the
        // u32 instead of panicking — overflow is intended and part of the hash.
        hash = hash.wrapping_mul(16777619); // FNV prime
    }
    hash
}

/// Remove duplicate trigrams while keeping their original order.
///
/// A word like `"banana"` repeats the trigram `"ana"`; intersecting the same
/// postings list twice during a query is wasted work, so we drop repeats first.
/// Order is preserved (a plain `HashSet` would lose it) so the query can still
/// intersect the rarest trigram first.
pub fn dedup_trigrams(trigrams: &[String]) -> Vec<String> {
    let mut seen = HashSet::new(); // trigrams we've already emitted
    let mut result = Vec::new();
    for t in trigrams {
        // `HashSet::insert` returns `true` only if the value was NOT already
        // present — so this both records `t` and tells us if it's the first sighting.
        if seen.insert(t) {
            result.push(t.clone()); // first time we've seen it → keep it
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_trigrams_basic() {
        let trigrams = extract_trigrams("Alpha");
        eprintln!("trigrams for 'Alpha': {:?}", trigrams);
        // Implementation: space padding + sliding window of 3
        // " Alpha" → [" al", "alp", "lph", "pha", "ha "]
        assert!(
            trigrams.contains(&" al".to_string()),
            "should have ' al': {:?}",
            trigrams
        );
        assert!(
            trigrams.contains(&"alp".to_string()),
            "should have 'alp': {:?}",
            trigrams
        );
        assert!(
            trigrams.contains(&"lph".to_string()),
            "should have 'lph': {:?}",
            trigrams
        );
        assert!(
            trigrams.contains(&"pha".to_string()),
            "should have 'pha': {:?}",
            trigrams
        );
        assert!(
            trigrams.contains(&"ha ".to_string()),
            "should have 'ha ': {:?}",
            trigrams
        );
    }

    #[test]
    fn test_extract_trigrams_case_insensitive() {
        let lower = extract_trigrams("alpha");
        let upper = extract_trigrams("ALPHA");
        assert_eq!(lower, upper);
    }

    #[test]
    fn test_extract_trigrams_short() {
        assert!(extract_trigrams("AB").is_empty());
        assert!(extract_trigrams("A").is_empty());
        assert!(extract_trigrams("").is_empty());
    }

    #[test]
    fn test_extract_trigrams_with_spaces() {
        let trigrams = extract_trigrams("The Vines");
        assert!(trigrams.contains(&" th".to_string()));
        assert!(trigrams.contains(&"the".to_string()));
        assert!(trigrams.contains(&"he ".to_string()));
        assert!(trigrams.contains(&"e v".to_string()));
    }

    #[test]
    fn test_extract_pattern_trigrams() {
        // %Alpha% — both sides wildcarded, so only interior trigrams (no space padding)
        let trigrams = extract_pattern_trigrams("%Alpha%");
        assert!(trigrams.contains(&"alp".to_string()));
        assert!(trigrams.contains(&"lph".to_string()));
        assert!(trigrams.contains(&"pha".to_string()));
        // Space-padded boundary trigrams must NOT be present
        assert!(!trigrams.contains(&" al".to_string()), "leading space should not appear with leading %");
        assert!(!trigrams.contains(&"ha ".to_string()), "trailing space should not appear with trailing %");

        // Alpha% — no leading wildcard, so leading space IS added
        let trigrams2 = extract_pattern_trigrams("Alpha%");
        assert!(trigrams2.contains(&" al".to_string()), "no leading % → leading space expected");
        assert!(!trigrams2.contains(&"ha ".to_string()), "trailing % → trailing space NOT expected");

        // %Alpha — no trailing wildcard, trailing space IS added
        let trigrams3 = extract_pattern_trigrams("%Alpha");
        assert!(!trigrams3.contains(&" al".to_string()), "leading % → leading space NOT expected");
        assert!(trigrams3.contains(&"ha ".to_string()), "no trailing % → trailing space expected");
    }

    #[test]
    fn test_extract_pattern_trigrams_wildcards_removed() {
        let trigrams = extract_pattern_trigrams("%foo_bar%");
        let has_underscore = trigrams.iter().any(|t| t == "_");
        assert!(!has_underscore);
    }

    #[test]
    fn test_hash_trigram() {
        let h1 = hash_trigram(" alp");
        let h2 = hash_trigram(" alp");
        let h3 = hash_trigram("bet");
        assert_eq!(h1, h2); // Same input = same hash
        assert_ne!(h1, h3); // Different input = different hash
    }

    #[test]
    fn test_dedup_trigrams() {
        let input = vec![
            " al".to_string(),
            " alp".to_string(),
            "alp".to_string(),
            " al".to_string(),
        ];
        let deduped = dedup_trigrams(&input);
        assert_eq!(deduped.len(), 3);
        assert_eq!(deduped[0], " al");
        assert_eq!(deduped[1], " alp");
        assert_eq!(deduped[2], "alp");
    }
}
