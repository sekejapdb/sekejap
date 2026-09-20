//! The tokenizer: raw statement text split into tokens, with the byte offset
//! of each so an error can point at the place it happened.
//!
//! Every multi-character operator PostgreSQL and its extensions spell is a
//! token here even when Tier 1 refuses it, because a refusal has to NAME the
//! construct it refuses (`docs/QL_CONTRACT.md` §6) and it cannot name what it
//! never lexed.

use super::{SqlError, SqlResult2};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Tok {
    /// An unquoted word: a keyword or an identifier, in its written case.
    Word(String),
    /// A double-quoted identifier, unfolded.
    Quoted(String),
    /// A single-quoted string, doubled quotes collapsed.
    Str(String),
    /// A numeric literal and whether it was written without a fraction or
    /// exponent, which is what decides an Int from a Real at compile time.
    Num(f64, bool),
    /// `$n`, one-based, as PostgreSQL numbers parameters.
    Param(usize),
    Star,
    Comma,
    Dot,
    Semicolon,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    /// `::`
    Cast,
    Eq,
    /// `<>` or `!=`
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Slash,
    Percent,
    Caret,
    /// `||`
    Concat,
    /// `|`
    Pipe,
    /// `&`
    Amp,
    /// `&&`, PostGIS's bounding-box overlap
    Overlaps,
    /// `@@`, the tsvector match
    Matches,
    /// `@>`
    ContainsOp,
    /// `<=>` cosine distance (pgvector)
    VecCosine,
    /// `<->` L2 distance (pgvector) and the PostGIS KNN operator
    VecL2,
    /// `<#>` negative inner product (pgvector)
    VecDot,
    /// `<+>` L1 distance (pgvector)
    VecL1,
    /// `->`
    Arrow,
    /// `->>`
    LongArrow,
    /// `<-`
    BackArrow,
    /// `~`, the POSIX regex match
    Tilde,
    Question,
    Bang,
    Eof,
}

impl Tok {
    /// The token as a statement would have written it, for an error message.
    pub(super) fn written(&self) -> String {
        match self {
            Self::Word(w) => w.clone(),
            Self::Quoted(w) => format!("\"{w}\""),
            Self::Str(s) => format!("'{s}'"),
            Self::Num(n, _) => format!("{n}"),
            Self::Param(n) => format!("${n}"),
            Self::Star => "*".into(),
            Self::Comma => ",".into(),
            Self::Dot => ".".into(),
            Self::Semicolon => ";".into(),
            Self::LParen => "(".into(),
            Self::RParen => ")".into(),
            Self::LBrace => "{".into(),
            Self::RBrace => "}".into(),
            Self::LBracket => "[".into(),
            Self::RBracket => "]".into(),
            Self::Colon => ":".into(),
            Self::Cast => "::".into(),
            Self::Eq => "=".into(),
            Self::Ne => "<>".into(),
            Self::Lt => "<".into(),
            Self::Le => "<=".into(),
            Self::Gt => ">".into(),
            Self::Ge => ">=".into(),
            Self::Plus => "+".into(),
            Self::Minus => "-".into(),
            Self::Slash => "/".into(),
            Self::Percent => "%".into(),
            Self::Caret => "^".into(),
            Self::Concat => "||".into(),
            Self::Pipe => "|".into(),
            Self::Amp => "&".into(),
            Self::Overlaps => "&&".into(),
            Self::Matches => "@@".into(),
            Self::ContainsOp => "@>".into(),
            Self::VecCosine => "<=>".into(),
            Self::VecL2 => "<->".into(),
            Self::VecDot => "<#>".into(),
            Self::VecL1 => "<+>".into(),
            Self::Arrow => "->".into(),
            Self::LongArrow => "->>".into(),
            Self::BackArrow => "<-".into(),
            Self::Tilde => "~".into(),
            Self::Question => "?".into(),
            Self::Bang => "!".into(),
            Self::Eof => "end of statement".into(),
        }
    }

    /// The word this token spells, upper-cased, when it is a bare word.
    pub(super) fn keyword(&self) -> Option<String> {
        match self {
            Self::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Token {
    pub(super) tok: Tok,
    pub(super) at: usize,
}

/// Statement text in, tokens out. The only thing this stage decides is where
/// one token ends and the next begins; what a word means is the parser's.
pub(super) fn tokenize(text: &str) -> SqlResult2<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // `-- line comment` and `/* block comment */`, both as PostgreSQL
        // spells them. A block comment does not nest here; one level is what
        // the corpus uses and a second would be a silent difference.
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut j = i + 2;
            loop {
                if j + 1 >= bytes.len() {
                    return Err(SqlError::syntax("unterminated block comment", i));
                }
                if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                    break;
                }
                j += 1;
            }
            i = j + 2;
            continue;
        }
        let at = i;
        let tok = match c {
            b'\'' => {
                let mut value = String::new();
                let mut j = i + 1;
                loop {
                    if j >= bytes.len() {
                        return Err(SqlError::syntax("unterminated string literal", at));
                    }
                    if bytes[j] == b'\'' {
                        if bytes.get(j + 1) == Some(&b'\'') {
                            value.push('\'');
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    let start = j;
                    let mut end = j + 1;
                    while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    value.push_str(
                        std::str::from_utf8(&bytes[start..end])
                            .map_err(|_| SqlError::syntax("statement text is not UTF-8", at))?,
                    );
                    j = end;
                }
                i = j;
                Tok::Str(value)
            }
            b'"' => {
                let mut value = String::new();
                let mut j = i + 1;
                loop {
                    if j >= bytes.len() {
                        return Err(SqlError::syntax("unterminated quoted identifier", at));
                    }
                    if bytes[j] == b'"' {
                        if bytes.get(j + 1) == Some(&b'"') {
                            value.push('"');
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    let start = j;
                    let mut end = j + 1;
                    while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    value.push_str(
                        std::str::from_utf8(&bytes[start..end])
                            .map_err(|_| SqlError::syntax("statement text is not UTF-8", at))?,
                    );
                    j = end;
                }
                i = j;
                Tok::Quoted(value)
            }
            b'$' => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == i + 1 {
                    return Err(SqlError::syntax("`$` must be followed by a digit", at));
                }
                let n: usize = text[i + 1..j]
                    .parse()
                    .map_err(|_| SqlError::syntax("parameter number is out of range", at))?;
                if n == 0 {
                    return Err(SqlError::syntax("parameters are numbered from $1", at));
                }
                i = j;
                Tok::Param(n)
            }
            b'0'..=b'9' => number(text, bytes, &mut i, at)?,
            b'.' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                number(text, bytes, &mut i, at)?
            }
            c if c == b'_' || c.is_ascii_alphabetic() || c >= 0x80 => {
                let mut j = i;
                while j < bytes.len() {
                    let b = bytes[j];
                    if b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80 {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let word = text[i..j].to_owned();
                i = j;
                Tok::Word(word)
            }
            _ => {
                let two = bytes.get(i + 1).copied();
                let three = bytes.get(i + 2).copied();
                let (tok, width) = match (c, two, three) {
                    (b'<', Some(b'='), Some(b'>')) => (Tok::VecCosine, 3),
                    (b'<', Some(b'-'), Some(b'>')) => (Tok::VecL2, 3),
                    (b'<', Some(b'#'), Some(b'>')) => (Tok::VecDot, 3),
                    (b'<', Some(b'+'), Some(b'>')) => (Tok::VecL1, 3),
                    (b'-', Some(b'>'), Some(b'>')) => (Tok::LongArrow, 3),
                    (b'<', Some(b'>'), _) => (Tok::Ne, 2),
                    (b'<', Some(b'='), _) => (Tok::Le, 2),
                    (b'<', Some(b'-'), _) => (Tok::BackArrow, 2),
                    (b'>', Some(b'='), _) => (Tok::Ge, 2),
                    (b'!', Some(b'='), _) => (Tok::Ne, 2),
                    (b'-', Some(b'>'), _) => (Tok::Arrow, 2),
                    (b':', Some(b':'), _) => (Tok::Cast, 2),
                    (b'|', Some(b'|'), _) => (Tok::Concat, 2),
                    (b'&', Some(b'&'), _) => (Tok::Overlaps, 2),
                    (b'@', Some(b'@'), _) => (Tok::Matches, 2),
                    (b'@', Some(b'>'), _) => (Tok::ContainsOp, 2),
                    (b'*', _, _) => (Tok::Star, 1),
                    (b',', _, _) => (Tok::Comma, 1),
                    (b'.', _, _) => (Tok::Dot, 1),
                    (b';', _, _) => (Tok::Semicolon, 1),
                    (b'(', _, _) => (Tok::LParen, 1),
                    (b')', _, _) => (Tok::RParen, 1),
                    (b'{', _, _) => (Tok::LBrace, 1),
                    (b'}', _, _) => (Tok::RBrace, 1),
                    (b'[', _, _) => (Tok::LBracket, 1),
                    (b']', _, _) => (Tok::RBracket, 1),
                    (b':', _, _) => (Tok::Colon, 1),
                    (b'=', _, _) => (Tok::Eq, 1),
                    (b'<', _, _) => (Tok::Lt, 1),
                    (b'>', _, _) => (Tok::Gt, 1),
                    (b'+', _, _) => (Tok::Plus, 1),
                    (b'-', _, _) => (Tok::Minus, 1),
                    (b'/', _, _) => (Tok::Slash, 1),
                    (b'%', _, _) => (Tok::Percent, 1),
                    (b'^', _, _) => (Tok::Caret, 1),
                    (b'|', _, _) => (Tok::Pipe, 1),
                    (b'&', _, _) => (Tok::Amp, 1),
                    (b'~', _, _) => (Tok::Tilde, 1),
                    (b'?', _, _) => (Tok::Question, 1),
                    (b'!', _, _) => (Tok::Bang, 1),
                    (other, _, _) => {
                        return Err(SqlError::syntax(
                            format!("unexpected character `{}`", other as char),
                            at,
                        ))
                    }
                };
                i += width;
                tok
            }
        };
        out.push(Token { tok, at });
    }
    out.push(Token {
        tok: Tok::Eof,
        at: bytes.len(),
    });
    Ok(out)
}

fn number(text: &str, bytes: &[u8], i: &mut usize, at: usize) -> SqlResult2<Tok> {
    let start = *i;
    let mut j = *i;
    let mut exact = true;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'.' {
        exact = false;
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
    }
    if j < bytes.len() && (bytes[j] == b'e' || bytes[j] == b'E') {
        let mut k = j + 1;
        if k < bytes.len() && (bytes[k] == b'+' || bytes[k] == b'-') {
            k += 1;
        }
        if k < bytes.len() && bytes[k].is_ascii_digit() {
            exact = false;
            j = k;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
        }
    }
    let value: f64 = text[start..j]
        .parse()
        .map_err(|_| SqlError::syntax("number literal is out of range", at))?;
    *i = j;
    Ok(Tok::Num(value, exact))
}
