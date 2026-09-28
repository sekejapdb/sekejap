//! The trigram analyzer: every 3-character piece of a text, for `LIKE` and
//! `ILIKE` with any pattern (`col gin_trgm_ops`, 0.19 A1).
//!
//! A trigram index is a text-family index whose descriptor names analyzer 2.
//! It writes the keyspaces analyzer v1 writes -- postings, norms, term and
//! corpus statistics, packed segments -- with pieces where v1 has words, so
//! every write, build, drop, verification and recovery path is the text
//! index's own. What is new is only this file: how a text becomes pieces,
//! and which pieces a pattern REQUIRES.
//!
//! ## The one property
//!
//! The index only narrows. Every row it hands back is checked by the `LIKE`
//! matcher (`crate::query::like`), so a false candidate costs a row read and
//! nothing else. A MISSING candidate is a wrong answer. So:
//!
//! > every row the matcher accepts holds every piece [`required`] names.
//!
//! Three choices keep that true.
//!
//! * **One fold for both sides.** A piece is cut from the FOLDED text: each
//!   character lower-cased through the pinned analyzer-v1 table (the same
//!   table full text uses, so a toolchain upgrade cannot change a stored
//!   piece), and final sigma `ς` written as `σ`. The matcher lower-cases the
//!   text as a whole string, where `Σ` at the end of a word becomes `ς`, and
//!   the pattern one character at a time, where it becomes `σ`
//!   (`like.rs`); the two only ever differ in which sigma they write, and
//!   the fold writes one. Folding is per character, so a text that CONTAINS
//!   a literal -- with or without `ILIKE` -- has a folded form that contains
//!   the folded literal.
//! * **Pieces never cross a wildcard.** A pattern is cut into runs of
//!   literal characters (escapes resolved first); `%` and `_` end a run. A
//!   run touching the pattern's start is prefixed with two START markers,
//!   one touching its end is followed by two END markers -- the text carries
//!   the same markers -- and a run beside a `_` touches nothing: `'_abc'`
//!   may start anywhere after one character.
//! * **Nothing that verification cannot store.** A piece holding NUL is
//!   neither written nor required (a stored term ends at its first NUL).
//!
//! A marker character inside a text makes pieces that look like a marked
//! piece. That adds candidates, which the matcher removes; it never removes
//! one.
//!
//! When the running toolchain's Unicode is not the pinned one, `ILIKE` asks
//! only for pieces made of ASCII and markers: the matcher lower-cases with
//! the standard library, and only ASCII is certain to lower-case the same way
//! in both. Fewer required pieces is a larger candidate set, never a smaller.

use super::analyzer::{push_lower, Analysis, MAX_TEXT_BYTES};
use std::collections::BTreeMap;

/// The descriptor's analyzer field for a trigram index.
pub(crate) const TRIGRAM_ANALYZER_VERSION: u16 = 2;
/// Written twice before the folded text and required before a run that
/// touches the pattern's start.
pub(crate) const START: char = '\u{2}';
/// Written twice after the folded text and required after a run that touches
/// the pattern's end.
pub(crate) const END: char = '\u{3}';
/// Distinct pieces one text may hold. A 64 KiB text (the text family's bound)
/// has at most about 128 Ki characters once folded, so this bounds the norm
/// and the per-row posting count without refusing any text the bound admits.
pub(crate) const MAX_PIECES: usize = 1 << 18;
/// Distinct pieces [`required`] collects for one pattern. Any subset of the
/// required pieces is still a complete candidate set, so a pattern with more
/// keeps the first this many it yields and the query reads at most this
/// many term statistics before it picks the 64 rarest.
pub(crate) const MAX_REQUIRED_PIECES: usize = 256;

/// The folded characters of `text`: pinned per-character lowercase, one
/// sigma.
pub(crate) fn fold(text: &str, out: &mut Vec<char>) {
    let mut lowered = String::new();
    for ch in text.chars() {
        lowered.clear();
        push_lower(ch, &mut lowered);
        out.extend(lowered.chars().map(|c| if c == 'ς' { 'σ' } else { c }));
    }
}

/// Every 3-character window of `chars` that holds no NUL, in order.
fn windows(chars: &[char], mut each: impl FnMut(&[char]) -> Result<(), &'static str>) -> Result<(), &'static str> {
    for window in chars.windows(3) {
        if !window.contains(&'\0') {
            each(window)?;
        }
    }
    Ok(())
}

/// The characters a text is cut from: two START markers, the folded text,
/// two END markers.
pub(crate) fn marked(text: &str) -> Result<Vec<char>, &'static str> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("indexed text exceeds 64 KiB");
    }
    let mut chars = Vec::with_capacity(text.len() + 4);
    chars.extend([START, START]);
    fold(text, &mut chars);
    chars.extend([END, END]);
    Ok(chars)
}

/// A text's pieces and how often each occurs; `length` is the number of
/// pieces written, repeats included. Missing and NULL are the index
/// lifecycle's, as for analyzer v1; the empty string is a present text with
/// the two marker pieces.
pub(crate) fn analyze(text: &str) -> Result<Analysis, &'static str> {
    let chars = marked(text)?;
    let mut result = Analysis {
        length: 0,
        terms: BTreeMap::new(),
    };
    windows(&chars, |window| {
        let piece: String = window.iter().collect();
        if let Some(count) = result.terms.get_mut(&piece) {
            *count += 1;
        } else {
            if result.terms.len() == MAX_PIECES {
                return Err("indexed text exceeds 262144 distinct trigram pieces");
            }
            result.terms.insert(piece, 1);
        }
        result.length += 1;
        Ok(())
    })?;
    Ok(result)
}

/// One element of a pattern once its escapes are resolved.
#[derive(Clone, Copy, PartialEq)]
enum Token {
    Char(char),
    Wild,
}

/// The pieces every text matching `pattern` holds, sorted and distinct.
///
/// `escape` is the pattern's escape character (`None` for `ESCAPE ''`),
/// exactly as the matcher takes it; a pattern the matcher refuses (one
/// ending in its escape character) is refused here with the same sentence.
/// `insensitive` is `ILIKE`. An empty answer means the pattern has no piece
/// to ask for and the index cannot narrow it.
pub(crate) fn required(
    pattern: &str,
    escape: Option<char>,
    insensitive: bool,
) -> Result<Vec<String>, &'static str> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            let next = chars
                .next()
                .ok_or("LIKE pattern must not end with escape character")?;
            tokens.push(Token::Char(next));
            continue;
        }
        tokens.push(match c {
            '%' | '_' => Token::Wild,
            c => Token::Char(c),
        });
    }
    let ascii_only = insensitive && std::char::UNICODE_VERSION != super::analyzer::UNICODE_VERSION;
    let mut pieces = std::collections::BTreeSet::new();
    // One run of literal characters: the tokens `from..to`. It touches the
    // pattern's start when `from` is 0 and its end when `to` is the length;
    // a run between wildcards touches neither.
    let mut cut = |from: usize, to: usize, run: &[char]| -> Result<(), &'static str> {
        let starts = from == 0;
        let ends = to == tokens.len();
        if run.is_empty() && !(starts && ends) {
            return Ok(());
        }
        let mut chars = Vec::with_capacity(run.len() + 4);
        if starts {
            chars.extend([START, START]);
        }
        fold(&run.iter().collect::<String>(), &mut chars);
        if ends {
            chars.extend([END, END]);
        }
        windows(&chars, |window| {
            if pieces.len() < MAX_REQUIRED_PIECES && (!ascii_only || window.iter().all(char::is_ascii)) {
                pieces.insert(window.iter().collect::<String>());
            }
            Ok(())
        })
    };
    let mut run = Vec::new();
    let mut from = 0;
    for (at, token) in tokens.iter().enumerate() {
        match token {
            Token::Char(c) => {
                if run.is_empty() {
                    from = at;
                }
                run.push(*c);
            }
            Token::Wild => {
                cut(from, at, &run)?;
                run.clear();
                from = at + 1;
            }
        }
    }
    cut(from, tokens.len(), &run)?;
    Ok(pieces.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(pattern: &str) -> Vec<String> {
        required(pattern, Some('\\'), false).unwrap()
    }
    fn shown(pieces: &[String]) -> Vec<String> {
        pieces
            .iter()
            .map(|p| p.replace(START, "^").replace(END, "$"))
            .collect()
    }

    #[test]
    fn a_text_is_cut_into_marked_windows() {
        let a = analyze("Ab").unwrap();
        assert_eq!(shown(&a.terms.keys().cloned().collect::<Vec<_>>()), ["^^a", "^ab", "ab$", "b$$"]);
        assert_eq!(a.length, 4);
        let repeated = analyze("aaaa").unwrap();
        assert_eq!(repeated.terms.get("aaa"), Some(&2));
        assert_eq!(repeated.length, 6);
        // The empty text is present: its two marker pieces.
        assert_eq!(shown(&analyze("").unwrap().terms.keys().cloned().collect::<Vec<_>>()), ["^^$", "^$$"]);
    }

    #[test]
    fn required_pieces_never_cross_a_wildcard_or_an_absent_boundary() {
        assert_eq!(shown(&pieces("%york%")), ["ork", "yor"]);
        assert_eq!(shown(&pieces("ne%")), ["^^n", "^ne"]);
        assert_eq!(shown(&pieces("%ity")), ["ity", "ty$", "y$$"]);
        assert_eq!(shown(&pieces("abc")), ["^^a", "^ab", "abc", "bc$", "c$$"]);
        assert_eq!(shown(&pieces("_abc")), ["abc", "bc$", "c$$"]);
        assert_eq!(shown(&pieces("abc_")), ["^^a", "^ab", "abc"]);
        assert_eq!(shown(&pieces("a_bc%")), ["^^a"]);
        assert!(pieces("%yo%").is_empty());
        assert!(pieces("%_%").is_empty());
        assert!(pieces("%").is_empty());
        // The empty pattern is the empty text.
        assert_eq!(shown(&pieces("")), ["^^$", "^$$"]);
        // Escapes are resolved before the runs are cut.
        assert_eq!(shown(&pieces("%0\\% c%")), ["% c", "0% "]);
        assert_eq!(shown(&required("%r!_s%", Some('!'), false).unwrap()), ["r_s"]);
        assert_eq!(shown(&required("%abc%", None, false).unwrap()), ["abc"]);
        assert!(required("abc\\", Some('\\'), false).is_err());
    }

    #[test]
    fn folding_writes_one_sigma_and_the_dotted_i_as_two_characters() {
        let mut out = Vec::new();
        fold("ΑΟΣ", &mut out);
        assert_eq!(out.iter().collect::<String>(), "αοσ");
        out.clear();
        fold("αος", &mut out);
        assert_eq!(out.iter().collect::<String>(), "αοσ");
        out.clear();
        fold("İz", &mut out);
        assert_eq!(out, ['i', '\u{307}', 'z']);
    }

    #[test]
    fn a_piece_never_holds_nul() {
        assert!(analyze("a\0bcd").unwrap().terms.keys().all(|p| !p.contains('\0')));
        assert_eq!(shown(&pieces("%a\0bcd%")), ["bcd"]);
    }
}
