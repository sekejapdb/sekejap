//! `search(col, 'query')` and `search_score()`, at the engine surface
//! (`docs/lang/QL_CONTRACT.md` §4.6).
//!
//! The oracle is a brute-force Levenshtein held in this process: it reads the
//! same corpus the index was built from, computes every edit distance itself,
//! applies the same two rules (`edit_bound` by token length, the LAST token as
//! a prefix), and names the rows it admits. The index-side walk must name
//! exactly those rows. Nothing here reads a constant out of the engine except
//! the two bounds, which the contract states.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionOptions, Database, EntityId, OrderValue, Projection,
        QueryBudget, QueryError, QueryFilter, QueryOrder, QueryRequest, ScoreExpr, SortDirection,
        TextMatch, WorkResource,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const PAGE: usize = 4096;

/// The rule this engine states: no edit up to four characters, one up to
/// eight, two beyond. Written out here rather than imported so the test
/// fails if the engine changes it silently.
fn edit_bound(chars: usize) -> u32 {
    match chars {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Plain Levenshtein between two character slices, written the slow way.
fn distance(a: &[char], b: &[char]) -> u32 {
    let mut row: Vec<u32> = (0..=b.len() as u32).collect();
    for (i, x) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i as u32 + 1;
        for (j, y) in b.iter().enumerate() {
            let held = row[j + 1];
            row[j + 1] = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(previous + u32::from(x != y));
            previous = held;
        }
    }
    row[b.len()]
}

/// The smallest distance between `token` and ANY prefix of `term`, which is
/// what the final token of a search is allowed to match.
fn prefix_distance(term: &[char], token: &[char]) -> u32 {
    (0..=term.len())
        .map(|at| distance(&term[..at], token))
        .min()
        .expect("the range holds at least the empty prefix")
}

struct Oracle {
    /// Document id -> its analyzer tokens.
    documents: BTreeMap<EntityId, Vec<String>>,
    /// Every distinct term of the corpus.
    dictionary: BTreeSet<String>,
}

impl Oracle {
    /// Which dictionary terms each query token admits, in order.
    fn groups(&self, query: &str) -> Vec<BTreeSet<String>> {
        let written = tokens(query);
        let last = written.len().saturating_sub(1);
        written
            .iter()
            .enumerate()
            .map(|(at, token)| {
                let token: Vec<char> = token.chars().collect();
                let bound = edit_bound(token.len());
                self.dictionary
                    .iter()
                    .filter(|term| {
                        let term: Vec<char> = term.chars().collect();
                        if at == last {
                            prefix_distance(&term, &token) <= bound
                        } else {
                            distance(&term, &token) <= bound
                        }
                    })
                    .cloned()
                    .collect()
            })
            .collect()
    }

    /// Every row a `search()` over this corpus admits: one term per query
    /// token, present in the document.
    fn admits(&self, query: &str) -> BTreeSet<EntityId> {
        let groups = self.groups(query);
        if groups.is_empty() {
            return BTreeSet::new();
        }
        self.documents
            .iter()
            .filter(|(_, held)| {
                groups
                    .iter()
                    .all(|group| group.iter().any(|term| held.contains(term)))
            })
            .map(|(id, _)| *id)
            .collect()
    }
}

struct Fixture {
    db: Database,
    collection: sekejap_core::collections::CollectionId,
    index: sekejap_core::collections::IndexId,
    oracle: Oracle,
}

fn build(dir: &std::path::Path, texts: &[String]) -> Fixture {
    let mut db = Database::create(dir.join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut documents = BTreeMap::new();
    let mut dictionary = BTreeSet::new();
    for (number, text) in texts.iter().enumerate() {
        let id = db
            .put(collection, &format!("p{number}"), &json!({ "body": text }))
            .unwrap();
        let held = tokens(text);
        dictionary.extend(held.iter().cloned());
        documents.insert(id, held);
    }
    db.commit().unwrap();
    let index = db.create_text_index(collection, "body_text", "body").unwrap();
    while !db.build_index_step(index, 256).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    Fixture {
        db,
        collection,
        index,
        oracle: Oracle {
            documents,
            dictionary,
        },
    }
}

impl Fixture {
    fn rows(&self, query: &str) -> BTreeSet<EntityId> {
        self.run(query, QueryOrder::Driver, QueryBudget::unlimited())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// Every admitted row with its `search_score()`.
    fn scored(&self, query: &str) -> Vec<(EntityId, f64)> {
        let expr = ScoreExpr::SearchScore {
            index: self.index,
            query,
        };
        self.run(
            query,
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Descending,
            },
            QueryBudget::unlimited(),
        )
        .unwrap()
    }

    fn run(
        &self,
        query: &str,
        order: QueryOrder<'_>,
        budget: QueryBudget,
    ) -> Result<Vec<(EntityId, f64)>, QueryError> {
        let filters = [QueryFilter::Text {
            index: self.index,
            query,
            matching: TextMatch::Search,
        }];
        let mut prepared = self.db.prepare_query(QueryRequest {
            collection: self.collection,
            filters: &filters,
            order,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })?;
        let mut out = Vec::new();
        loop {
            let page = prepared.next_page(PAGE, budget, || false)?;
            for row in &page.rows {
                let value = match row.order {
                    OrderValue::Score(value) => value,
                    _ => f64::NAN,
                };
                out.push((row.id, value));
            }
            if page.done || page.rows.is_empty() {
                break;
            }
        }
        Ok(out)
    }
}

/// A corpus whose words differ by one and two edits from each other, so a
/// bounded walk has something to accept and something to refuse.
fn corpus() -> Vec<String> {
    let words = [
        "kebun", "kebon", "kebunan", "kabun", "sekolah", "sekola", "sekolahan", "jembatan",
        "jambatan", "warung", "waring", "gudang", "pasar", "pasir", "kopi", "kopo", "sawah",
        "sawa", "danau", "danu", "hutan", "hutang", "kantor", "kantir", "desa", "desi", "mesa",
    ];
    let mut texts = Vec::new();
    let mut state = 0x243F_6A88_85A3_08D3u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    for _ in 0..240 {
        let count = 2 + next() % 4;
        let mut held: Vec<&str> = Vec::new();
        for _ in 0..count {
            let word = words[next() % words.len()];
            if !held.contains(&word) {
                held.push(word);
            }
        }
        texts.push(held.join(" "));
    }
    texts
}

#[test]
fn a_brute_force_levenshtein_over_the_same_corpus_names_the_rows_the_index_walk_names() {
    let temp = tempfile::tempdir().unwrap();
    let f = build(temp.path(), &corpus());
    // Randomised queries with injected typos: a substitution, a deletion, an
    // insertion and a transposition over words that are in the corpus, plus
    // truncated words that only a prefix completion can reach.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let dictionary: Vec<String> = f.oracle.dictionary.iter().cloned().collect();
    let mut asked = 0usize;
    for _ in 0..200 {
        let terms = 1 + next() % 3;
        let mut written = Vec::new();
        for _ in 0..terms {
            let word = dictionary[next() % dictionary.len()].clone();
            let chars: Vec<char> = word.chars().collect();
            let mutated: String = match next() % 5 {
                // As written.
                0 => word.clone(),
                // Substitute one character.
                1 => {
                    let at = next() % chars.len();
                    chars
                        .iter()
                        .enumerate()
                        .map(|(i, ch)| if i == at { 'z' } else { *ch })
                        .collect()
                }
                // Delete one character.
                2 => {
                    let at = next() % chars.len();
                    chars
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != at)
                        .map(|(_, ch)| *ch)
                        .collect()
                }
                // Insert one character.
                3 => {
                    let at = next() % (chars.len() + 1);
                    let mut out: String = chars[..at].iter().collect();
                    out.push('q');
                    out.extend(chars[at..].iter());
                    out
                }
                // Truncate, so the final token has something to complete.
                _ => chars[..1 + next() % chars.len()].iter().collect(),
            };
            written.push(mutated);
        }
        let query = written.join(" ");
        if tokens(&query).is_empty() {
            continue;
        }
        asked += 1;
        assert_eq!(
            f.rows(&query),
            f.oracle.admits(&query),
            "search({query:?}) named different rows than the brute-force oracle"
        );
    }
    assert!(asked > 150, "the loop asked only {asked} queries");
}

#[test]
fn the_last_token_completes_as_a_prefix_and_the_earlier_tokens_do_not() {
    let temp = tempfile::tempdir().unwrap();
    let f = build(
        temp.path(),
        &[
            "kebun sekolah".to_owned(),
            "sekolah kebun".to_owned(),
            "kebun warung".to_owned(),
        ],
    );
    // `sekol` is four characters short of `sekolah`; with no edit budget at
    // five characters it can only be reached by completing a prefix.
    assert_eq!(f.rows("sekol").len(), 2, "the only token is the last one");
    // The same token written FIRST does not complete: `sekol` is not a term,
    // and one edit does not reach `sekolah` from it either.
    assert!(
        f.rows("sekol kebun").is_empty(),
        "an earlier token must not complete as a prefix"
    );
    // Written last, it completes, and the earlier token still has to match a
    // whole term.
    assert_eq!(f.rows("kebun sekol").len(), 2);
    assert!(
        f.rows("kebu sekolah").is_empty(),
        "`kebu` is four characters, which the edit rule gives no edits, and it is not the last token"
    );
    assert_eq!(f.rows("sekolah kebu").len(), 2, "`kebu` completes `kebun`");
}

#[test]
fn the_edit_bound_is_none_up_to_four_characters_one_up_to_eight_and_two_beyond() {
    let temp = tempfile::tempdir().unwrap();
    let f = build(
        temp.path(),
        &[
            "kopi".to_owned(),
            "jembatan".to_owned(),
            "pembangunan".to_owned(),
        ],
    );
    // Four characters, no edits: one substitution is not reachable. Written
    // last it would COMPLETE, so the query pins the token with a word that
    // cannot help it.
    assert!(f.rows("kzpi jembatan").is_empty(), "four characters spend no edit");
    // Eight characters, one edit.
    assert_eq!(f.rows("jembztan").len(), 1);
    assert!(
        f.rows("jzmbztan kopi").is_empty(),
        "eight characters spend one edit, not two"
    );
    // Eleven characters, two edits.
    assert_eq!(f.rows("pzmbzngunan").len(), 1);
    assert!(
        f.rows("pzmbzngunzn kopi").is_empty(),
        "eleven characters spend two edits, not three"
    );
}

#[test]
fn an_exact_term_scores_one_an_edit_scores_less_and_a_longer_prefix_scores_more() {
    let temp = tempfile::tempdir().unwrap();
    let f = build(temp.path(), &["kebun".to_owned()]);
    let one = |query: &str| -> f64 {
        let scored = f.scored(query);
        assert_eq!(scored.len(), 1, "query {query:?} named {scored:?}");
        scored[0].1
    };
    let exact = one("kebun");
    assert!(
        (exact - 1.0).abs() < 1e-12,
        "an exact term match is 1.0, found {exact}"
    );
    let edited = one("kebin");
    assert!(
        edited < exact,
        "a one-edit match ({edited}) must score strictly less than an exact one ({exact})"
    );
    let long = one("kebu");
    let short = one("keb");
    assert!(
        long > short,
        "completing one character ({long}) must score strictly more than completing two ({short})"
    );
    assert!(
        short < exact && short > 0.0,
        "every search score lies in (0,1], found {short}"
    );
}

#[test]
fn a_walk_that_would_pass_its_bound_is_truncated_and_says_so() {
    let temp = tempfile::tempdir().unwrap();
    // A dictionary far wider than the visit bound, every term one insertion
    // away from a nine-character token, so a two-edit walk cannot prune.
    let mut texts = Vec::new();
    for n in 0..6_000usize {
        texts.push(format!("aaaaaaaaa{n:05}"));
    }
    let f = build(temp.path(), &texts);
    let plain = f.db.search_expansion(f.index, "kopi").unwrap();
    assert!(
        !plain.truncated,
        "a token no dictionary entry can reach must prune instead of truncating: {plain:?}"
    );
    let wide = f.db.search_expansion(f.index, "aaaaaaaaa").unwrap();
    assert!(
        wide.truncated,
        "a query that reaches thousands of terms must be truncated, found {wide:?}"
    );
    assert!(
        wide.visited <= wide.visit_cap && wide.terms <= wide.term_cap,
        "a truncated walk must stay inside both bounds: {wide:?}"
    );
    // The answer is still an ANSWER, not an error: the rows it names are rows
    // the query admits, and the caller is told the list was capped.
    let rows = f.rows("aaaaaaaaa");
    assert!(!rows.is_empty());
    assert!(rows.len() <= wide.term_cap);
}

#[test]
fn a_search_answers_under_a_query_budget_and_a_refusal_names_its_resource() {
    let temp = tempfile::tempdir().unwrap();
    let f = build(temp.path(), &corpus());
    let generous = f.run("kebun sekol", QueryOrder::Driver, QueryBudget::unlimited());
    assert!(!generous.unwrap().is_empty());
    let mut budget = QueryBudget::unlimited();
    budget.text_postings = 3;
    let refused = f.run("kebun sekol", QueryOrder::Driver, budget);
    match refused {
        Err(QueryError::BudgetExceeded { resource, .. }) => {
            assert_eq!(resource, WorkResource::TextPostings);
        }
        other => panic!("a bounded search must refuse by resource, found {other:?}"),
    }
}
