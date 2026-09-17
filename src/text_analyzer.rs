//! Analyzer v1: pinned Unicode alphanumeric/lowercase behavior.
//! No std Unicode classification/case conversion occurs on the runtime path.
#[path = "text_unicode_v1.rs"]
mod tables;
use std::collections::BTreeMap;

pub(crate) const ANALYZER_VERSION: u16 = 1;
pub(crate) const UNICODE_VERSION: (u8, u8, u8) = tables::UNICODE_VERSION;
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TERM_BYTES: usize = 128;
pub(crate) const MAX_TOKENS: u32 = 16_384;
pub(crate) const MAX_TERMS: usize = 4096;
pub(crate) const MAX_PHRASE_TOKENS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Analysis {
    pub length: u32,
    pub terms: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhraseAnalysis {
    pub analysis: Analysis,
    pub sequence: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum PhraseScanError<E> {
    Analysis(&'static str),
    Callback(E),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhraseScanEvent {
    /// A cancellation-only checkpoint while scanning a long token or run of
    /// separators. This event does not represent a token of work.
    Poll,
    /// Emitted before a normalized document token is counted or compared.
    Token,
}

fn alphanumeric(ch: char) -> bool {
    if ch.is_ascii() {
        return ch.is_ascii_alphanumeric();
    }
    let cp = ch as u32;
    let at = tables::ALNUM.partition_point(|&(lo, _)| lo <= cp);
    at > 0 && cp <= tables::ALNUM[at - 1].1
}
fn push_lower(ch: char, token: &mut String) {
    if ch.is_ascii() {
        token.push(ch.to_ascii_lowercase());
        return;
    }
    match tables::LOWER.binary_search_by_key(&(ch as u32), |&(cp, _)| cp) {
        Ok(at) => token.push_str(tables::LOWER[at].1),
        Err(_) => token.push(ch),
    }
}

/// Missing/null is handled by the index lifecycle. An empty string is a
/// present document whose length is zero and whose term map is empty.
pub(crate) fn analyze(text: &str) -> Result<Analysis, &'static str> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("indexed text exceeds 64 KiB");
    }
    let mut result = Analysis {
        length: 0,
        terms: BTreeMap::new(),
    };
    let mut token = String::new();
    fn finish(token: &mut String, result: &mut Analysis) -> Result<(), &'static str> {
        if token.is_empty() {
            return Ok(());
        }
        if result.length == MAX_TOKENS {
            return Err("indexed text exceeds 16384 tokens");
        }
        result.length += 1;
        if let Some(count) = result.terms.get_mut(token.as_str()) {
            *count += 1;
        } else {
            if result.terms.len() == MAX_TERMS {
                return Err("indexed text exceeds 4096 distinct terms");
            }
            result.terms.insert(token.clone(), 1);
        }
        token.clear();
        Ok(())
    }
    for ch in text.chars() {
        if alphanumeric(ch) {
            push_lower(ch, &mut token);
            if token.len() > MAX_TERM_BYTES {
                return Err("indexed term exceeds 128 UTF-8 bytes");
            }
        } else {
            finish(&mut token, &mut result)?;
        }
    }
    finish(&mut token, &mut result)?;
    Ok(result)
}

/// The build-side analyzer: the same terms, the same term frequencies and the
/// same token count as [`analyze`], with no per-document allocation at all.
///
/// [`analyze`] is shaped for one document in isolation. It builds a fresh
/// `BTreeMap<String, u32>`, which costs an owned `String` and a tree node the
/// first time a document mentions a term, and a string-compare descent on
/// every occurrence including repeats. A late build runs that once per row: at
/// 200,000 rows and six distinct terms a document, roughly 2.4 million
/// allocations whose only purpose is to be dropped again a few microseconds
/// later.
///
/// FTS5 does not do that either -- its tokenizer writes straight into the hash
/// entry the term already has. This is that shape. Terms are interned ONCE for
/// the whole build; a document is a run of hash lookups into that table and an
/// increment of a counter the table already owns. The per-document output is
/// `(term id, term frequency)` in first-appearance order, and the caller keeps
/// its own state per id -- the build keeps a segment packer there.
///
/// Every bound [`analyze`] enforces is enforced here, in the same order and
/// with the same sentence: text size first, then token count, then term size,
/// then distinct terms per document. `analyze_oracle_equivalence` in this
/// module is the proof, over generated bodies and over each bound.
///
/// Sacrifice (Law 1): the intern table holds the build's whole vocabulary --
/// one `Box<str>` and eight bytes per DISTINCT term, not per document and not
/// per posting.
pub(crate) struct BuildAnalyzer {
    ids: std::collections::HashMap<Box<str>, u32>,
    names: Vec<Box<str>>,
    /// Current document's frequency per term id. Zero outside a document, so
    /// a non-zero slot is also "this document has already seen this term".
    frequency: Vec<u32>,
    touched: Vec<u32>,
    document: Vec<(u32, u32)>,
    token: String,
    length: u32,
}

impl Default for BuildAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildAnalyzer {
    pub(crate) fn new() -> Self {
        Self {
            ids: std::collections::HashMap::new(),
            names: Vec::new(),
            frequency: Vec::new(),
            touched: Vec::new(),
            document: Vec::new(),
            token: String::new(),
            length: 0,
        }
    }

    /// Every term interned so far, indexed by term id.
    pub(crate) fn names(&self) -> &[Box<str>] {
        &self.names
    }

    /// The last analyzed document's `(term id, term frequency)` pairs.
    pub(crate) fn document(&self) -> &[(u32, u32)] {
        &self.document
    }

    /// Analyze one document. Returns its token count; its terms are then in
    /// [`BuildAnalyzer::document`]. On any error the per-document state is
    /// drained exactly as on success, so the analyzer stays usable.
    pub(crate) fn analyze(&mut self, text: &str) -> Result<u32, &'static str> {
        let outcome = self.run(text);
        self.document.clear();
        if outcome.is_ok() {
            self.document.reserve(self.touched.len());
        }
        for at in 0..self.touched.len() {
            let id = self.touched[at] as usize;
            if outcome.is_ok() {
                self.document.push((id as u32, self.frequency[id]));
            }
            self.frequency[id] = 0;
        }
        self.touched.clear();
        self.token.clear();
        outcome
    }

    fn run(&mut self, text: &str) -> Result<u32, &'static str> {
        if text.len() > MAX_TEXT_BYTES {
            return Err("indexed text exceeds 64 KiB");
        }
        self.length = 0;
        for ch in text.chars() {
            if alphanumeric(ch) {
                push_lower(ch, &mut self.token);
                if self.token.len() > MAX_TERM_BYTES {
                    return Err("indexed term exceeds 128 UTF-8 bytes");
                }
            } else {
                self.finish_token()?;
            }
        }
        self.finish_token()?;
        Ok(self.length)
    }

    fn finish_token(&mut self) -> Result<(), &'static str> {
        if self.token.is_empty() {
            return Ok(());
        }
        if self.length == MAX_TOKENS {
            return Err("indexed text exceeds 16384 tokens");
        }
        self.length += 1;
        let id = match self.ids.get(self.token.as_str()) {
            Some(id) => *id as usize,
            None => {
                let id = self.names.len();
                let name: Box<str> = self.token.as_str().into();
                self.ids.insert(name.clone(), id as u32);
                self.names.push(name);
                self.frequency.push(0);
                id
            }
        };
        if self.frequency[id] == 0 {
            if self.touched.len() == MAX_TERMS {
                return Err("indexed text exceeds 4096 distinct terms");
            }
            self.touched.push(id as u32);
        }
        self.frequency[id] += 1;
        self.token.clear();
        Ok(())
    }
}

/// Analyze a literal phrase query while preserving its ordered token stream.
/// The distinct-term bounds are identical to ordinary text queries; the
/// position-sensitive stream has its own explicit, smaller bound.
pub(crate) fn analyze_phrase_query(text: &str) -> Result<PhraseAnalysis, &'static str> {
    let analysis = analyze(text)?;
    let mut sequence = Vec::new();
    let mut token = String::new();
    fn finish(token: &mut String, sequence: &mut Vec<String>) -> Result<(), &'static str> {
        if token.is_empty() {
            return Ok(());
        }
        if sequence.len() == MAX_PHRASE_TOKENS {
            return Err("text phrase query exceeds 64 tokens");
        }
        sequence.push(std::mem::take(token));
        Ok(())
    }
    for ch in text.chars() {
        if alphanumeric(ch) {
            push_lower(ch, &mut token);
        } else {
            finish(&mut token, &mut sequence)?;
        }
    }
    finish(&mut token, &mut sequence)?;
    Ok(PhraseAnalysis { analysis, sequence })
}

/// The KMP failure function of a phrase, built ONCE for the query.
///
/// It depends on the phrase alone, and `analyze_phrase_document` rebuilt it --
/// an allocation and a `Vec` of `usize` -- for every document it scanned.
pub(crate) fn phrase_prefix(phrase: &[String]) -> Vec<usize> {
    let mut prefix = vec![0usize; phrase.len()];
    for index in 1..phrase.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && phrase[index] != phrase[matched] {
            matched = prefix[matched - 1];
        }
        if phrase[index] == phrase[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    prefix
}

/// Scan authoritative primary text for an ordered contiguous phrase, counting
/// only what the caller is going to read.
///
/// The difference from [`analyze_phrase_document`] is what is NOT built. That
/// one returns a whole `Analysis`: a `BTreeMap<String, u32>` of every distinct
/// term in the document, which is a map node and an owned `String` per term,
/// per document -- and the query then looks up two of them and drops the rest.
/// Here the caller names the terms it will ask about and gets their counts
/// back positionally in a buffer it owns, so a document costs no allocation at
/// all once the buffers have settled.
///
/// Returns `(token count, phrase found)`. `seen[i]` is how often `terms[i]`
/// occurred. `token` is scratch; its contents on return mean nothing.
///
/// The distinct-term bound is the one check that cannot survive dropping the
/// map, and it is the one that cannot fire: `analyze` refuses to INDEX text
/// over that bound, so a document reaching this scanner is one the index
/// already accepted. The bounds that guard this scan's own work -- text size,
/// token count, term size -- are all still here, and the caller still holds
/// the count and the frequencies against what the index recorded.
pub(crate) fn scan_phrase_document<E>(
    text: &str,
    phrase: &[String],
    prefix: &[usize],
    terms: &[String],
    seen: &mut Vec<u32>,
    token: &mut String,
    mut callback: impl FnMut(PhraseScanEvent) -> Result<(), E>,
) -> Result<(u32, bool), PhraseScanError<E>> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(PhraseScanError::Analysis("indexed text exceeds 64 KiB"));
    }
    debug_assert_eq!(prefix.len(), phrase.len(), "the phrase and its failure function agree");
    seen.clear();
    seen.resize(terms.len(), 0);
    token.clear();
    let mut length = 0u32;
    let mut matched = 0usize;
    let mut found = false;
    let mut next_poll = 256usize;
    fn finish<E>(
        token: &mut String,
        length: &mut u32,
        terms: &[String],
        seen: &mut [u32],
        phrase: &[String],
        prefix: &[usize],
        matched: &mut usize,
        found: &mut bool,
        callback: &mut impl FnMut(PhraseScanEvent) -> Result<(), E>,
    ) -> Result<(), PhraseScanError<E>> {
        if token.is_empty() {
            return Ok(());
        }
        callback(PhraseScanEvent::Token).map_err(PhraseScanError::Callback)?;
        if *length == MAX_TOKENS {
            return Err(PhraseScanError::Analysis(
                "indexed text exceeds 16384 tokens",
            ));
        }
        *length += 1;
        // A text query carries at most 64 terms and usually one or two, so a
        // linear pass over them beats hashing the token.
        for (at, term) in terms.iter().enumerate() {
            if term.as_str() == token.as_str() {
                seen[at] = seen[at].saturating_add(1);
                break;
            }
        }
        if !phrase.is_empty() {
            while *matched > 0 && token != &phrase[*matched] {
                *matched = prefix[*matched - 1];
            }
            if token == &phrase[*matched] {
                *matched += 1;
            }
            if *matched == phrase.len() {
                *found = true;
                *matched = prefix[*matched - 1];
            }
        }
        token.clear();
        Ok(())
    }
    for (offset, ch) in text.char_indices() {
        if offset >= next_poll {
            callback(PhraseScanEvent::Poll).map_err(PhraseScanError::Callback)?;
            next_poll = offset.saturating_add(256);
        }
        if alphanumeric(ch) {
            push_lower(ch, token);
            if token.len() > MAX_TERM_BYTES {
                return Err(PhraseScanError::Analysis(
                    "indexed term exceeds 128 UTF-8 bytes",
                ));
            }
        } else {
            finish(
                token, &mut length, terms, seen, phrase, prefix, &mut matched, &mut found,
                &mut callback,
            )?;
        }
    }
    finish(
        token, &mut length, terms, seen, phrase, prefix, &mut matched, &mut found, &mut callback,
    )?;
    Ok((length, found))
}

/// Analyze authoritative primary text and test an ordered contiguous phrase
/// without retaining document positions. `callback(Token)` runs before each
/// token is counted or compared. `Poll` keeps cancellation responsive during
/// punctuation-only or single-token inputs.
pub(crate) fn analyze_phrase_document<E>(
    text: &str,
    phrase: &[String],
    mut callback: impl FnMut(PhraseScanEvent) -> Result<(), E>,
) -> Result<(Analysis, bool), PhraseScanError<E>> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(PhraseScanError::Analysis("indexed text exceeds 64 KiB"));
    }
    let prefix = phrase_prefix(phrase);

    let mut result = Analysis {
        length: 0,
        terms: BTreeMap::new(),
    };
    let mut token = String::new();
    let mut matched = 0usize;
    let mut found = false;
    let mut next_poll = 256usize;
    fn finish<E>(
        token: &mut String,
        result: &mut Analysis,
        phrase: &[String],
        prefix: &[usize],
        matched: &mut usize,
        found: &mut bool,
        callback: &mut impl FnMut(PhraseScanEvent) -> Result<(), E>,
    ) -> Result<(), PhraseScanError<E>> {
        if token.is_empty() {
            return Ok(());
        }
        callback(PhraseScanEvent::Token).map_err(PhraseScanError::Callback)?;
        if result.length == MAX_TOKENS {
            return Err(PhraseScanError::Analysis(
                "indexed text exceeds 16384 tokens",
            ));
        }
        result.length += 1;
        if let Some(count) = result.terms.get_mut(token.as_str()) {
            *count += 1;
        } else {
            if result.terms.len() == MAX_TERMS {
                return Err(PhraseScanError::Analysis(
                    "indexed text exceeds 4096 distinct terms",
                ));
            }
            result.terms.insert(token.clone(), 1);
        }

        if !phrase.is_empty() {
            while *matched > 0 && token != &phrase[*matched] {
                *matched = prefix[*matched - 1];
            }
            if token == &phrase[*matched] {
                *matched += 1;
            }
            if *matched == phrase.len() {
                *found = true;
                *matched = prefix[*matched - 1];
            }
        }
        token.clear();
        Ok(())
    }

    for (offset, ch) in text.char_indices() {
        if offset >= next_poll {
            callback(PhraseScanEvent::Poll).map_err(PhraseScanError::Callback)?;
            next_poll = offset.saturating_add(256);
        }
        if alphanumeric(ch) {
            push_lower(ch, &mut token);
            if token.len() > MAX_TERM_BYTES {
                return Err(PhraseScanError::Analysis(
                    "indexed term exceeds 128 UTF-8 bytes",
                ));
            }
        } else {
            finish(
                &mut token,
                &mut result,
                phrase,
                &prefix,
                &mut matched,
                &mut found,
                &mut callback,
            )?;
        }
    }
    finish(
        &mut token,
        &mut result,
        phrase,
        &prefix,
        &mut matched,
        &mut found,
        &mut callback,
    )?;
    Ok((result, found))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate bodies the way a corpus does: repeated terms, punctuation,
    /// unicode, empty documents, long runs of separators.
    fn generated_body(i: u64) -> String {
        let words = [
            "flood", "levee", "river", "CAFÉ", "cafe\u{301}", "İstanbul", "ΣΟΣ", "北京", "１２３",
            "Straße", "rock-and-roll", "silt", "tide", "rail", "flood", "flood",
        ];
        if i % 37 == 0 {
            return String::new();
        }
        let mut body = String::new();
        for j in 0..(1 + i % 11) {
            if j > 0 {
                body.push_str(if j % 3 == 0 { ", " } else { "  ---  " });
            }
            body.push_str(words[((i * 7 + j * 13) % words.len() as u64) as usize]);
            if j % 4 == 0 {
                body.push('.');
                body.push_str(words[((i + j) % words.len() as u64) as usize]);
            }
        }
        body
    }

    /// The build analyzer is only allowed to be faster. Term set, term
    /// frequency, token count and every refusal must be what `analyze` says,
    /// document by document, with one analyzer reused across all of them.
    #[test]
    fn the_build_analyzer_agrees_with_analyze_on_every_document() {
        let mut build = BuildAnalyzer::new();
        for i in 0..2_000u64 {
            let body = generated_body(i);
            let oracle = analyze(&body).unwrap();
            let length = build.analyze(&body).unwrap();
            assert_eq!(length, oracle.length, "token count for {body:?}");
            let mut seen: BTreeMap<String, u32> = BTreeMap::new();
            for &(id, frequency) in build.document() {
                assert!(
                    seen.insert(build.names()[id as usize].to_string(), frequency)
                        .is_none(),
                    "term id {id} repeated in one document"
                );
            }
            assert_eq!(seen, oracle.terms, "terms for {body:?}");
        }
    }

    /// Every bound, with the same sentence, and the analyzer still usable
    /// afterwards -- a refused document must not leave a half-counted one
    /// behind for the next row of the build.
    #[test]
    fn the_build_analyzer_refuses_exactly_what_analyze_refuses() {
        let cases = [
            "x".repeat(MAX_TEXT_BYTES + 1),
            "y".repeat(MAX_TERM_BYTES + 1),
            (0..MAX_TOKENS as usize + 1)
                .map(|n| format!("t{n} "))
                .collect::<String>(),
            (0..MAX_TERMS + 1)
                .map(|n| format!("u{n} "))
                .collect::<String>(),
        ];
        let mut build = BuildAnalyzer::new();
        for case in &cases {
            let oracle = analyze(case).unwrap_err();
            let theirs = build.analyze(case).unwrap_err();
            assert_eq!(theirs, oracle, "refusal sentence");
            // Still usable, and not carrying the refused document's counts.
            let length = build.analyze("levee levee river").unwrap();
            assert_eq!(length, 3);
            let mut seen: BTreeMap<String, u32> = BTreeMap::new();
            for &(id, frequency) in build.document() {
                seen.insert(build.names()[id as usize].to_string(), frequency);
            }
            assert_eq!(
                seen,
                analyze("levee levee river").unwrap().terms,
                "state survived a refusal"
            );
        }
    }
    #[test]
    fn fixed_multilingual_tokens_preserve_e3_semantics() {
        let a = analyze(
            "Hello, HELLO! CAFÉ cafe\u{301} İstanbul ΣΟΣ 北京 １２３ Straße _ rock-and-roll",
        )
        .unwrap();
        let expected: BTreeMap<_, _> = [
            ("hello", 2),
            ("café", 1),
            ("cafe", 1),
            ("i\u{307}stanbul", 1),
            ("σοσ", 1),
            ("北京", 1),
            ("１２３", 1),
            ("straße", 1),
            ("rock", 1),
            ("and", 1),
            ("roll", 1),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        assert_eq!(a.length, 12);
        assert_eq!(a.terms, expected);
        assert_eq!(
            analyze("\0 !-_").unwrap(),
            Analysis {
                length: 0,
                terms: BTreeMap::new()
            }
        );
        assert_eq!(ANALYZER_VERSION, 1);
        assert_ne!(tables::GENERATOR_COMPILER, "unrecorded");
    }
    #[test]
    fn bounds_reject_whole_analysis_without_truncation() {
        assert!(analyze(&"x".repeat(MAX_TERM_BYTES + 1)).is_err());
        assert!(analyze(&" ".repeat(MAX_TEXT_BYTES + 1)).is_err());
        assert!(analyze(&"a ".repeat(MAX_TOKENS as usize + 1)).is_err());
        let many = (0..MAX_TERMS + 1)
            .map(|n| format!("w{n} "))
            .collect::<String>();
        assert!(analyze(&many).is_err());
        // Case expansion counts encoded output bytes, not input characters.
        assert!(analyze(&"İ".repeat(43)).is_err());
        assert_eq!(
            analyze(&"a ".repeat(MAX_TOKENS as usize)).unwrap().length,
            MAX_TOKENS
        );
    }
    #[test]
    fn table_shape_and_complete_generation_oracle() {
        for pair in tables::ALNUM.windows(2) {
            assert!(pair[0].1 + 1 < pair[1].0);
        }
        for pair in tables::LOWER.windows(2) {
            assert!(pair[0].0 < pair[1].0);
        }
        // Compiler Unicode upgrades do not redefine v1. On the generation
        // Unicode version, exhaustively compare every scalar to the source API.
        if std::char::UNICODE_VERSION != UNICODE_VERSION {
            return;
        }
        for cp in 0..=0x10ffff {
            let Some(ch) = char::from_u32(cp) else {
                continue;
            };
            assert_eq!(alphanumeric(ch), ch.is_alphanumeric(), "U+{cp:X}");
            let mut mapped = String::new();
            push_lower(ch, &mut mapped);
            assert_eq!(mapped, ch.to_lowercase().collect::<String>(), "U+{cp:X}");
        }
    }

    #[test]
    fn phrase_sequence_and_streamed_match_preserve_order_and_repetition() {
        let phrase = analyze_phrase_query("İSTANBUL, rock-rock").unwrap();
        assert_eq!(
            phrase.sequence,
            ["i\u{307}stanbul", "rock", "rock"]
        );
        assert_eq!(phrase.analysis.length, 3);

        let mut tokens = 0;
        let (document, matched) = analyze_phrase_document(
            "before İstanbul rock rock after",
            &phrase.sequence,
            |event| -> Result<(), ()> {
                if event == PhraseScanEvent::Token {
                    tokens += 1;
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(matched);
        assert_eq!(tokens, usize::try_from(document.length).unwrap());
        assert!(!analyze_phrase_document(
            "İstanbul rock gap rock",
            &phrase.sequence,
            |_| Ok::<_, ()>(()),
        )
        .unwrap()
        .1);
        assert!(analyze_phrase_query(&"x ".repeat(MAX_PHRASE_TOKENS + 1)).is_err());
    }
}
