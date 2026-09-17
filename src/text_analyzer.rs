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
