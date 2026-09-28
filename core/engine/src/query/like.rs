//! `LIKE` / `ILIKE` over one TEXT field, answered from the row.
//!
//! PostgreSQL's pattern rules: `%` is any run of characters, the empty one
//! included; `_` is exactly one character (a character, not a byte); the
//! escape character (a backslash unless the statement names another, none
//! when it names the empty string) makes the next character literal. `ILIKE`
//! compares both sides lower-cased, character by character, beyond ASCII.
//!
//! The pattern is compiled once. The shapes a search box writes -- contains
//! (`%x%`), starts-with (`x%`), ends-with (`%x`), exact and `%` alone -- are
//! answered by the standard library's substring search; anything else by the
//! classic wildcard walk, which backtracks only to the last `%`.

/// One compiled `LIKE` pattern.
#[derive(Clone, Debug)]
pub(crate) struct LikeMatcher {
    shape: Shape,
    insensitive: bool,
    /// `Contains`' needle, compiled once: the SIMD substring search
    /// (`memchr::memmem`) the row check runs per row.
    finder: Option<memchr::memmem::Finder<'static>>,
}

impl PartialEq for LikeMatcher {
    fn eq(&self, other: &Self) -> bool {
        // The finder is derived from the shape.
        self.shape == other.shape && self.insensitive == other.insensitive
    }
}

thread_local! {
    /// The lower-cased text of the row being checked, reused from row to row
    /// so an `ILIKE` over ASCII text allocates nothing per row.
    static LOWERED: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

#[derive(Clone, Debug, PartialEq)]
enum Shape {
    /// `%` alone (or several): every text matches.
    Any,
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    General(Vec<Token>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Token {
    Char(char),
    One,
    Many,
}

/// Why a pattern cannot be compiled: PostgreSQL's `22025`
/// (invalid_escape_sequence).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LikePatternError(pub(crate) String);

impl LikeMatcher {
    pub(crate) fn compile(
        pattern: &str,
        escape: Option<char>,
        insensitive: bool,
    ) -> Result<Self, LikePatternError> {
        let mut tokens = Vec::new();
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            if Some(c) == escape {
                let Some(next) = chars.next() else {
                    return Err(LikePatternError(
                        "LIKE pattern must not end with escape character".into(),
                    ));
                };
                push_char(&mut tokens, next, insensitive);
                continue;
            }
            match c {
                '%' => {
                    // Adjacent `%` are one `%`.
                    if tokens.last() != Some(&Token::Many) {
                        tokens.push(Token::Many);
                    }
                }
                '_' => tokens.push(Token::One),
                c => push_char(&mut tokens, c, insensitive),
            }
        }
        let shape = shape_of(tokens);
        let finder = match &shape {
            Shape::Contains(s) => Some(memchr::memmem::Finder::new(s.as_bytes()).into_owned()),
            _ => None,
        };
        Ok(Self {
            shape,
            insensitive,
            finder,
        })
    }

    /// Whether `text` matches. The caller decides what a missing or NULL
    /// value means (it matches nothing, `NOT LIKE` included).
    pub(crate) fn matches(&self, text: &str) -> bool {
        if !self.insensitive {
            return self.matches_folded(text);
        }
        // ASCII text lower-cases byte by byte, into a buffer reused across
        // rows; anything else takes the standard library's whole-string
        // lowercase, which is what defines ILIKE here.
        if text.is_ascii() {
            LOWERED.with(|buffer| {
                let mut buffer = buffer.borrow_mut();
                buffer.clear();
                buffer.push_str(text);
                buffer.make_ascii_lowercase();
                self.matches_folded(&buffer)
            })
        } else {
            self.matches_folded(&text.to_lowercase())
        }
    }

    fn matches_folded(&self, text: &str) -> bool {
        match &self.shape {
            Shape::Any => true,
            Shape::Exact(s) => text == s,
            Shape::Prefix(s) => text.starts_with(s.as_str()),
            Shape::Suffix(s) => text.ends_with(s.as_str()),
            Shape::Contains(s) => match &self.finder {
                Some(finder) => finder.find(text.as_bytes()).is_some(),
                None => text.contains(s.as_str()),
            },
            Shape::General(tokens) => wildcard(tokens, text),
        }
    }
}

fn push_char(tokens: &mut Vec<Token>, c: char, insensitive: bool) {
    if insensitive {
        tokens.extend(c.to_lowercase().map(Token::Char));
    } else {
        tokens.push(Token::Char(c));
    }
}

fn literal(tokens: &[Token]) -> Option<String> {
    tokens
        .iter()
        .map(|t| match t {
            Token::Char(c) => Some(*c),
            _ => None,
        })
        .collect()
}

fn shape_of(tokens: Vec<Token>) -> Shape {
    if tokens.iter().all(|t| *t == Token::Many) {
        return if tokens.is_empty() {
            Shape::Exact(String::new())
        } else {
            Shape::Any
        };
    }
    let starts = tokens.first() == Some(&Token::Many);
    let ends = tokens.last() == Some(&Token::Many);
    let inner = &tokens[usize::from(starts)..tokens.len() - usize::from(ends)];
    match (starts, ends, literal(inner)) {
        (false, false, Some(s)) => Shape::Exact(s),
        (false, true, Some(s)) => Shape::Prefix(s),
        (true, false, Some(s)) => Shape::Suffix(s),
        (true, true, Some(s)) => Shape::Contains(s),
        _ => Shape::General(tokens),
    }
}

/// The classic wildcard walk over characters: on a mismatch, return to the
/// last `%` and let it absorb one more character. Linear in the text for
/// every pattern with at most one `%`, and never worse than text x pattern.
fn wildcard(tokens: &[Token], text: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let (mut t, mut p) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match tokens.get(p) {
            Some(Token::Many) => {
                star = Some((p, t));
                p += 1;
            }
            Some(Token::One) => {
                t += 1;
                p += 1;
            }
            Some(Token::Char(c)) if *c == text[t] => {
                t += 1;
                p += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    tokens[p..].iter().all(|t| *t == Token::Many)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, insensitive: bool, text: &str) -> bool {
        LikeMatcher::compile(pattern, Some('\\'), insensitive)
            .unwrap()
            .matches(text)
    }

    #[test]
    fn shapes_and_the_general_walk_agree() {
        for (pattern, text, want) in [
            ("%doe%", "joshndoesadikin", true),
            ("%doe%", "JohnDoe", false),
            ("j_hn%", "john.doe", true),
            ("%o_s%", "joshndoesadikin", true),
            ("%o_e%", "joshndoesadikin", false),
            ("a%b%c", "axxbyyc", true),
            ("a%b%c", "axxbyy", false),
            ("", "", true),
            ("", "a", false),
            ("%", "", true),
            ("_", "", false),
            ("_", "é", true),
            ("%\\%%", "100% cotton", true),
            ("%\\%%", "100 cotton", false),
        ] {
            assert_eq!(m(pattern, false, text), want, "{pattern:?} on {text:?}");
        }
        assert!(m("%ÉCOLE%", true, "École Ubud"));
        assert!(m("%DOE%", true, "JohnDoe@example.com"));
        assert!(LikeMatcher::compile("abc\\", Some('\\'), false).is_err());
        assert!(LikeMatcher::compile("a!_b", Some('!'), false).unwrap().matches("a_b"));
        assert!(!LikeMatcher::compile("a!_b", Some('!'), false).unwrap().matches("axb"));
    }
}
