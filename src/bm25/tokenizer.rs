//! # Simple tokenizer — turning raw text into searchable terms
//!
//! Searches don't match raw documents by comparing whole strings. A database
//! first chops each document into smaller pieces — **terms** — and then builds
//! an index that says "this term appears in these documents". When you search,
//! the DB looks up your query the same way and compares term to term. This file
//! does that chopping step for BM25, the ranking algorithm sekejap uses to say
//! *which* matching document is most relevant.
//!
//! The job is deliberately tiny and boring: **split text into lowercase words,
//! drop any word shorter than 3 characters, and hand back the survivors.** No
//! stemming, no stop-word removal — just the plain split. Keeping it simple is a
//! feature: a tokenizer is measured on consistency and speed, not cleverness.
//!
//! ## How it works
//!
//! The straightforward way to "split on spaces" breaks the moment punctuation
//! shows up (`"hello,"` should become `"hello"`, not a word with a comma stuck on
//! the end). So instead of splitting on spaces, the code flips the question:
//!
//! 1. Lowercase the whole input once, up front.
//! 2. Walk the characters one at a time, **accumulating an in-progress word**.
//! 3. Whenever we hit a character that is *not* a letter or digit, the current
//!    word is finished: if it is long enough, push it into the result; then
//!    clear the buffer and keep going.
//! 4. When the input runs out, flush whatever word is still in the buffer.
//!
//! The same loop body powers all three public functions — only what they do with
//! each finished word differs (keep it, keep it + where it was, count it).
//!
//! ## The clever foundational idea
//!
//! The real trick is that the tokenizer never materializes the split list of
//! words from the raw string. It keeps a **single growable buffer** (`current`)
//! and reuses it for every word: characters are pushed in one at a time and the
//! buffer is cleared between words. That means the code can treat *any* character
//! as a boundary — space, comma, period, newline — with exactly the same code
//! path, and it never has to guess where words end ahead of time. The "split on
//! non-alphanumerics" rule falls out naturally from the loop instead of being a
//! special case.
//!
//! ## Core components
//!
//! - [`tokenize`] — the base splitter: text in, `Vec<String>` of terms out.
//! - [`tokenize_with_positions`] — terms plus their **position** (0-based index
//!   in the token stream), which is what phrase/multi-word queries need.
//! - [`tokenize_with_freq`] — terms with their **frequency** (how many times each
//!   appears), which is what BM25's term-frequency term needs.
//!
//! Consumed by the BM25 index stored in this crate's `bm25` module.

use std::collections::HashSet;

/// Tokenize text into terms.
///
/// Returns lowercase terms with length >= 3.
///
/// ## How it works, step by step
///
/// `text` is a `&str` — Rust's name for "a string I am only borrowing, not
/// owning". Callers hand this function a slice of some string they already have,
/// and the function promises not to take ownership of it. That is why the
/// function builds its own `Vec<String>` of *owned* strings to return: the input
/// is borrowed, but the output must outlive the function call.
///
/// The function keeps one character buffer, `current`, and fills it across the
/// whole input:
///
/// 1. `text.to_lowercase()` produces a *new* owned string with every letter
///    lowercased — an allocation, but a single cheap one that runs once.
/// 2. `.chars()` is an **iterator** over the string's Unicode characters. An
///    iterator is a lazy "next one, please" machine: `.chars()` doesn't build a
///    list of characters, it just remembers how to hand you the next one. The
///    `for` loop pulls characters out of it until it runs dry.
/// 3. For each character: if it's a letter/digit, append to the current word.
///    Otherwise the word is over — flush it if it's long enough, then reset.
///
/// The final `if` after the loop is easy to miss but essential: it flushes the
/// *last* word, which ends because the input ended, not because we hit a
/// non-alphanumeric character.
///
/// ## Ownership note for newcomers
///
/// `current.clone()` inside the loop copies the finished word into a brand-new
/// `String` before pushing it into `terms`. This matters because `current` is
/// about to be `clear()`ed and reused for the next word — if we pushed `current`
/// itself, every term in the vector would end up pointing at the same now-cleared
/// buffer. Cloning is the "give me my own copy" operation; `terms` then owns each
/// copy independently.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new(); // the answer, grown one finished word at a time
    let mut current = String::new();         // the word being built right now (reused)

    // `to_lowercase()` copies the whole input once; `.chars()` yields one
    // character per iteration. `c` is a `char` — a single Unicode character.
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            // A letter or digit: this character belongs to the word in progress.
            current.push(c);
        } else if !current.is_empty() {
            // A non-alphanumeric (space, comma, punctuation, ...) ends the word.
            // `current.len()` is the byte length; for ASCII words it equals the
            // character count (the >= 3 minimum-term-length rule).
            if current.len() >= 3 {
                // `clone()` copies the word so `terms` owns it independently of
                // the reused `current` buffer.
                terms.push(current.clone());
            }
            current.clear(); // reset the buffer; it gets reused for the next word
        }
    }

    // Flush the trailing word: the loop above only emits on a boundary
    // character, so the last word (no boundary after it) needs this final check.
    if !current.is_empty() && current.len() >= 3 {
        // Unlike the loop, here we can move `current` in directly — no clone
        // needed, because this is the last word and the buffer is never reused.
        terms.push(current);
    }

    terms
}

/// Tokenize text into terms with their positions in the token stream.
///
/// Returns `(term, position)` pairs where position is the 0-based token index.
///
/// ## How it works
///
/// This is the same scanning loop as [`tokenize`], with one extra job: it counts
/// each finished word as it goes, so every returned pair knows *where* that term
/// sits in the document. The first kept word is position `0`, the second is `1`,
/// and so on.
///
/// Why does position matter? Consider the query "new york". Two documents could
/// both contain the words "new" and "york", but only one has them *next to each
/// other*. Position data is what lets a search engine tell "New York" (adjacent,
/// position `new`=0, `york`=1) from "york is not new" (far apart). Storing
/// positions enables phrase matching and proximity ranking.
///
/// ## Two important details
///
/// - `pos` only increments when a word is actually *kept* (long enough). Short
///   words are dropped from the stream entirely, so positions count only the
///   terms that survive — this keeps positions consistent with what [`tokenize`]
///   would have returned.
/// - The buffer is cleared after every word, exactly as in [`tokenize`].
pub fn tokenize_with_positions(text: &str) -> Vec<(String, usize)> {
    let mut result = Vec::new(); // (term, position) pairs, in document order
    let mut pos = 0;             // 0-based index of the next kept word
    let mut current = String::new(); // reused word buffer, same as tokenize

    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            if current.len() >= 3 {
                // A *tuple* `(current.clone(), pos)` bundles the term and its
                // position into one value, then `push` appends it to the vector.
                result.push((current.clone(), pos));
                pos += 1; // only advance for words that actually made it into the result
            }
            current.clear();
        }
    }

    // Flush the trailing word, same as tokenize. `current` is moved in directly
    // (no clone) because it's never reused after this.
    if !current.is_empty() && current.len() >= 3 {
        result.push((current, pos));
    }

    result
}

/// Tokenize and deduplicate, preserving frequency count.
///
/// ## What "frequency" means and why it matters
///
/// BM25's whole premise is that a term which appears *many times* in a document
/// makes that document more relevant to a query containing the term. So, before
/// scoring, we need to know, for each distinct term, **how many times it
/// occurred**. This function turns `["rust", "rust", "rust", "db"]` into the
/// pairs `("rust", 3)` and `("db", 1)`.
///
/// ## How it works
///
/// Two collections work together:
///
/// 1. A [`HashMap`](std::collections::HashMap) counts occurrences: each distinct
///    term is a **key**, and its count is the **value**. Counting is done with
///    the `entry().or_default()` pattern — look up the term, default its count
///    to 0 if missing, then add 1.
/// 2. A [`HashSet`](std::collections::HashSet) is the return type. It is *like*
///    a HashMap but with no values — just a set of unique items. Each `(term,
///    count)` pair is inserted once, which is redundant work here (the map
///    already guarantees uniqueness) but gives callers a set they can query for
///    membership ("does term X appear at all?").
///
/// ## Teaching the key concepts
///
/// - **HashMap** stores key→value pairs, looking each up in O(1) — roughly
///   "constant time, no matter how large". Here the key is the `String` term and
///   the value is the `u32` count.
/// - **`entry(term)`** hands us a special "entry" object for that key, and
///   **`.or_default()`** says: if the key is absent, insert it with a default
///   value (for `u32`, that's `0`). The returned reference `*counts.entry(...)`
///   points at the value inside the map, and `+= 1` increments it in place —
///   no copying the map around.
/// - **`into_iter()`** consumes the map, yielding each `(key, value)` pair in
///   turn. **`.map(|(t, c)| (t, c))`** is a **closure** — an inline anonymous
///   function (`|args| body`) — applied to each pair. Here it just passes the
///   pair through, but writing it this way shows the shape of the data flowing
///   through the iterator pipeline.
/// - **`freq.extend(...)`** pushes every item an iterator yields into the set.
///   The `..` is a `Range` iterator; `.chain(..)` would let you extend from two
///   sources at once. `extend` is the key idea: "drain this iterator into me".
pub fn tokenize_with_freq(text: &str) -> HashSet<(String, u32)> {
    let tokens = tokenize(text);
    // `freq` will be the returned set; `counts` is the intermediate counting map.
    let mut freq: HashSet<(String, u32)> = HashSet::new();
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    // Count pass: for every term, bump its count. `or_default()` makes a missing
    // term start at 0, so `+= 1` records the first occurrence as 1.
    for term in tokens {
        *counts.entry(term).or_default() += 1;
    }

    // Convert the map into the set: `into_iter()` yields (term, count) pairs,
    // `.map` passes each through unchanged, and `extend` inserts them all.
    freq.extend(counts.into_iter().map(|(t, c)| (t, c)));
    freq
}

#[cfg(test)]
mod tests {
    use super::*;

    // `#[cfg(test)]` means this module only exists when running `cargo test` —
    // it is compiled out of the shipped library. `use super::*;` imports the
    // parent module's items so the tests can call `tokenize` directly.

    #[test]
    fn test_tokenize_basic() {
        let terms = tokenize("Hello World Rust Tutorial");
        // Note the lowercase: "Hello" becomes "hello". Compare against a slice
        // of &str; Rust auto-comparable because Vec<String> derefs to slices.
        assert_eq!(terms, &["hello", "world", "rust", "tutorial"]);
    }

    #[test]
    fn test_tokenize_punctuation() {
        // Commas, `!`, and `?` are non-alphanumeric and act as word boundaries.
        let terms = tokenize("Hello, world! Rust is great?");
        assert_eq!(terms, &["hello", "world", "rust", "great"]);
    }

    #[test]
    fn test_tokenize_short_words() {
        // "I", "am", and "a" are all < 3 chars and get dropped; 2-char "is"
        // (in the punctuation test above) is also dropped.
        let terms = tokenize("I am a Rust programmer");
        assert_eq!(terms, &["rust", "programmer"]);
    }

    #[test]
    fn test_tokenize_min_length() {
        // Boundary case: "the" is exactly 3 characters, so it is kept (>= 3).
        let terms = tokenize("The Rust is great");
        assert_eq!(terms, &["the", "rust", "great"]);
    }
}