//! The typo-tolerant atomic: a BOUNDED edit-distance walk over the term
//! dictionary that is already on disk.
//!
//! # Why this adds no keyspace tag and no feature bit
//!
//! Every term of a text index is already one B-tree row. `term_stats_key(id,
//! term)` (`super::term_stats_key`) is `index_prefix(TERM_STATS = 0x77, id) ++
//! term.as_bytes() ++ 0`, and the value is the term's document frequency. So
//! for one index the whole dictionary is a CONTIGUOUS run of keys in sorted
//! byte order, which is sorted code-point order for the alphanumeric-only
//! tokens analyzer v1 produces. A prefix range is a range scan over that run.
//! A bounded Levenshtein walk is an ORDERED traversal of the same run with a
//! seek that skips a dead subtree. Each accepted term's postings are the rows
//! already at `POSTING = 0x75`, read through [`super::TermPostings`].
//!
//! Nothing here writes. Nothing here reads a tag that did not exist before.
//!
//! # The walk
//!
//! One query is analyzer-v1 tokenized into an ORDERED token list (the order
//! matters: the LAST token completes as a prefix, the earlier ones do not).
//! Each token is walked separately over the same dictionary run:
//!
//! * The dynamic-programming row for `distance(term[0..i], token)` is
//!   extended one character of the dictionary term at a time.
//! * `min(row)` is NON-DECREASING in `i` -- every cell of row `i + 1` is at
//!   least `min(row_i)` -- so once `min(row_i) > max_edits` no extension of
//!   `term[0..i]` can ever match. The walk then SEEKS past every key that has
//!   `term[0..i]` as a byte prefix instead of stepping over them.
//! * An earlier token is accepted when the FULL term is within `max_edits`.
//!   The final token is accepted when SOME prefix of the term is, which is
//!   search-as-you-type: `min over p of distance(term[0..p], token)`.
//!
//! # The two bounds
//!
//! `max_edits` is chosen from the token's length in characters by
//! [`edit_bound`]: 0 for 1..=4, 1 for 5..=8, 2 for 9 and longer. It is the
//! rule Meilisearch uses, and it exists because one edit over a three-letter
//! token matches most of a dictionary while saying nothing about intent.
//!
//! [`MAX_VISITED`] bounds the dictionary entries the whole expansion may
//! step over and [`MAX_TERMS`] bounds the terms it may accept, so a
//! pathological query cannot walk an entire dictionary. Hitting either one
//! TRUNCATES the expansion, and a truncated expansion is reported -- never
//! silently short. The accepted terms are kept by quality, so a truncated
//! walk keeps the best matches it found rather than the first ones.
use super::{
    decode_count, index_prefix, Database, IndexId, Result, TERM_STATS,
};
use crate::collections::{corrupt, has_prefix};

/// Dictionary entries one `search()` expansion may step over, across every
/// token of the query.
pub(crate) const MAX_VISITED: usize = 4_096;

/// Dictionary terms one `search()` expansion may accept, across every token.
/// It is deliberately the same 64 that `MAX_TEXT_TERMS` already bounds a
/// prepared text query by: an expansion becomes that query's term list, and
/// two different ceilings on one list would be one ceiling too many.
pub(crate) const MAX_TERMS: usize = 64;

/// The edit bound this engine spends on a token of `chars` characters.
///
/// Stated as a function rather than buried in the walk because the contract
/// row and the test both name it.
pub(crate) fn edit_bound(chars: usize) -> u32 {
    match chars {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

/// One query token's accepted dictionary term.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Accepted {
    /// Index into [`Expansion::terms`].
    pub(crate) term: usize,
    /// Edits actually spent reaching it.
    pub(crate) distance: u32,
    /// Characters of the dictionary term the prefix had to complete. Zero for
    /// every token but the last, which is the only one that completes.
    pub(crate) completed: usize,
    /// The [0,1] quality this (token, term) pair contributes, see
    /// [`quality`].
    pub(crate) score: f64,
}

/// What one `search(col, 'query')` compiles to against a dictionary.
#[derive(Clone, Debug)]
pub(crate) struct Expansion {
    /// The analyzer-v1 tokens of the query, in the order they were written.
    pub(crate) tokens: Vec<String>,
    /// Every accepted dictionary term, sorted and without repeats. One
    /// posting stream is opened per entry.
    pub(crate) terms: Vec<String>,
    /// The persisted document frequency of each entry of `terms`.
    pub(crate) dfs: Vec<u64>,
    /// Per query token, the terms that token accepted. A document satisfies
    /// the search when EVERY group has at least one term present in it.
    pub(crate) groups: Vec<Vec<Accepted>>,
    /// Dictionary entries stepped over.
    pub(crate) visited: usize,
    /// Did a bound stop the walk short of the terms that would have matched?
    pub(crate) truncated: bool,
}

/// The [0,1] quality of one (token, term) pair.
///
/// `edits` is what the automaton spent and `bound` is what it was allowed;
/// `typed` and `found` are the token's and the term's lengths in characters.
/// Written here, once, because `docs/lang/QL_CONTRACT.md` §5 deviation 8
/// quotes this function and the test asserts its monotonicity rather than its
/// constants.
///
/// ```text
/// quality = (1 - edits / (bound + 1)) * (typed / max(typed, found))
/// ```
///
/// An exact term match is `(1 - 0) * 1 = 1`. Spending an edit strictly lowers
/// the first factor; completing more of the final token strictly lowers the
/// second. Both factors lie in (0,1], so the product does.
pub(crate) fn quality(edits: u32, bound: u32, typed: usize, found: usize) -> f64 {
    let spent = 1.0 - f64::from(edits) / f64::from(bound + 1);
    let completion = if found <= typed {
        1.0
    } else {
        typed as f64 / found as f64
    };
    spent * completion
}

/// The first byte string strictly greater than every extension of `prefix`.
///
/// `None` when there is none -- `prefix` is empty, or every byte is `0xFF` --
/// and then the walk is over.
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut next = prefix.to_vec();
    while let Some(last) = next.pop() {
        if last != 0xFF {
            next.push(last + 1);
            return Some(next);
        }
    }
    None
}

/// The accepted terms of one token, kept by quality under `cap`.
struct Best {
    cap: usize,
    kept: Vec<(String, u64, u32, usize, f64)>,
    dropped: bool,
}

impl Best {
    fn offer(&mut self, term: &str, df: u64, distance: u32, completed: usize, score: f64) {
        if self.kept.len() < self.cap {
            self.kept
                .push((term.to_owned(), df, distance, completed, score));
            return;
        }
        let worst = self
            .kept
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.4.total_cmp(&b.4).then_with(|| b.0.cmp(&a.0)))
            .map(|(at, _)| at);
        self.dropped = true;
        if let Some(at) = worst {
            if self.kept[at].4 < score {
                self.kept[at] = (term.to_owned(), df, distance, completed, score);
            }
        }
    }
}

/// Walk one token over the dictionary of `id`.
///
/// `complete` is true for the final token alone, and is what makes the walk a
/// PREFIX walk: the token may match any prefix of a dictionary term.
fn walk_token(
    db: &Database,
    id: IndexId,
    token: &[char],
    complete: bool,
    cap: usize,
    visited: &mut usize,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Best> {
    let bound = edit_bound(token.len());
    let mut best = Best {
        cap,
        kept: Vec::new(),
        dropped: false,
    };
    let prefix = index_prefix(TERM_STATS, id);
    let n = token.len();
    let mut start = prefix.clone();
    let mut row = vec![0u32; n + 1];
    let mut carry = vec![0u32; n + 1];
    'scan: loop {
        if cancelled() {
            return Err(crate::collections::Error::Cancelled);
        }
        let mut iter = db.store()?.range(&start)?;
        // The inner loop steps; it leaves only to SEEK past a dead subtree
        // (with the key to reopen at) or to end the walk entirely.
        start = loop {
            let Some((key, value)) = iter.peek_ref()? else {
                break 'scan;
            };
            if !has_prefix(key, &prefix) {
                break 'scan;
            }
            if *visited == MAX_VISITED {
                best.dropped = true;
                break 'scan;
            }
            *visited += 1;
            let raw = &key[prefix.len()..];
            if raw.last() != Some(&0) {
                return Err(corrupt("text term statistics key"));
            }
            let term = std::str::from_utf8(&raw[..raw.len() - 1])
                .map_err(|_| corrupt("text term statistics key is not UTF-8"))?
                .to_owned();
            let df = decode_count(value, "text term statistics length")?;
            if df == 0 {
                return Err(corrupt("zero persisted document frequency"));
            }
            // Row zero: the empty term prefix against the whole token.
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = j as u32;
            }
            let mut spent = (complete && row[n] <= bound).then_some(row[n]);
            let mut dead: Option<usize> = None;
            let mut bytes = 0usize;
            let mut chars = 0usize;
            for (at, ch) in term.chars().enumerate() {
                carry.copy_from_slice(&row);
                row[0] = at as u32 + 1;
                for j in 1..=n {
                    let cost = u32::from(token[j - 1] != ch);
                    row[j] = (carry[j] + 1).min(row[j - 1] + 1).min(carry[j - 1] + cost);
                }
                bytes += ch.len_utf8();
                chars = at + 1;
                if complete && row[n] <= bound {
                    spent = Some(match spent {
                        Some(held) => held.min(row[n]),
                        None => row[n],
                    });
                }
                if *row.iter().min().expect("the row has n + 1 cells") > bound {
                    // The prune is sound for the FULL-term rule always, and
                    // for the prefix rule only while nothing has matched yet.
                    // Once some prefix `term[0..p]` has been accepted, every
                    // dictionary term that shares these bytes shares that
                    // prefix too and is accepted as well, so the subtree is
                    // full of answers rather than empty of them and has to be
                    // walked. That is what the visit bound is for.
                    if complete && spent.is_some() {
                        break;
                    }
                    dead = Some(bytes);
                    break;
                }
            }
            let accepted = if complete {
                spent.map(|distance| (distance, term.chars().count()))
            } else if dead.is_none() && row[n] <= bound {
                Some((row[n], chars))
            } else {
                None
            };
            if let Some((distance, found)) = accepted {
                let found = if complete { term.chars().count() } else { found };
                let completed = found.saturating_sub(n);
                best.offer(
                    &term,
                    df,
                    distance,
                    if complete { completed } else { 0 },
                    quality(distance, bound, n, if complete { found } else { n }),
                );
            }
            match dead {
                // Every key whose term begins with these bytes is dead, so the
                // walk seeks past the whole subtree rather than stepping
                // through it.
                Some(at) => {
                    let mut skip = prefix.clone();
                    skip.extend_from_slice(&raw[..at]);
                    match successor(&skip) {
                        Some(next) => break next,
                        None => break 'scan,
                    }
                }
                None => iter.step(),
            }
        };
    }
    Ok(best)
}

/// Expand one `search(col, 'query')` against the dictionary of `id`.
pub(crate) fn expand(
    db: &Database,
    id: IndexId,
    query: &str,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Expansion> {
    let analysis = crate::text_analyzer::analyze_phrase_query(query)
        .map_err(crate::collections::invalid)?;
    let tokens = analysis.sequence;
    let mut expansion = Expansion {
        tokens: tokens.clone(),
        terms: Vec::new(),
        dfs: Vec::new(),
        groups: Vec::new(),
        visited: 0,
        truncated: false,
    };
    if tokens.is_empty() {
        return Ok(expansion);
    }
    // A FAIR share of the term ceiling per token: a query whose first token
    // matches half the dictionary must not leave the last token with nothing,
    // because a group with no term admits no document and the answer would be
    // empty rather than short.
    let cap = (MAX_TERMS / tokens.len()).max(1);
    let last = tokens.len() - 1;
    let mut collected: Vec<Vec<(String, u64, u32, usize, f64)>> = Vec::with_capacity(tokens.len());
    for (at, token) in tokens.iter().enumerate() {
        let chars: Vec<char> = token.chars().collect();
        let best = walk_token(
            db,
            id,
            &chars,
            at == last,
            cap,
            &mut expansion.visited,
            cancelled,
        )?;
        expansion.truncated |= best.dropped;
        collected.push(best.kept);
    }
    let mut terms: Vec<(String, u64)> = Vec::new();
    for group in &collected {
        for (term, df, _, _, _) in group {
            if terms.iter().all(|(held, _)| held != term) {
                terms.push((term.clone(), *df));
            }
        }
    }
    terms.sort_by(|a, b| a.0.cmp(&b.0));
    expansion.dfs = terms.iter().map(|(_, df)| *df).collect();
    expansion.terms = terms.into_iter().map(|(term, _)| term).collect();
    for group in collected {
        let mut accepted: Vec<Accepted> = group
            .into_iter()
            .map(|(term, _, distance, completed, score)| Accepted {
                term: expansion
                    .terms
                    .iter()
                    .position(|held| *held == term)
                    .expect("every accepted term was collected"),
                distance,
                completed,
                score,
            })
            .collect();
        accepted.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.term.cmp(&b.term)));
        expansion.groups.push(accepted);
    }
    Ok(expansion)
}

/// What a caller is told about one expansion without preparing a query.
///
/// `sekejap-lang` reads this while it compiles a `search()` so that a
/// TRUNCATED walk reaches the client as a NOTICE, the way `SHOW EDGES`
/// reports a capped shape rather than returning a short answer that looks
/// complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchExpansion {
    /// Analyzer-v1 tokens the query held.
    pub tokens: usize,
    /// Dictionary terms the walk accepted.
    pub terms: usize,
    /// Dictionary entries the walk stepped over.
    pub visited: usize,
    /// Did a bound stop the walk short?
    pub truncated: bool,
    /// The visit ceiling, so a notice can name the number it hit.
    pub visit_cap: usize,
    /// The accepted-term ceiling.
    pub term_cap: usize,
}
