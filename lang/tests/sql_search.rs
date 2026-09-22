//! `search(col, 'query')` and `search_score()` as SQL
//! (`docs/lang/QL_CONTRACT.md` §4.6), against the 2,000-row fixture the other
//! SQL suites share.
//!
//! The oracle is a brute-force computation over the fixture's OWN rows, held
//! in this process: it tokenizes each document the way the fixture wrote it,
//! walks the whole vocabulary itself, and applies the two stated rules --
//! `edit_bound` by token length, and the LAST token as a prefix. A statement
//! that agrees with the engine because both are wrong still fails here.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use sekejap_core::collections::{CollectionOptions, Database, EntityId, QueryBudget};
use sekejap_core::Kind;
use sekejap_lang::{prepare_sql, SqlDatabase, SqlError, SqlResult, SqlValue, Tier};
use serde_json::json;
use std::collections::BTreeSet;
use tempfile::TempDir;

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

fn sql_ids(db: &mut Database, text: &str) -> Vec<EntityId> {
    match db.sql(text, &[]).unwrap() {
        SqlResult::Rows { rows, .. } => rows.iter().map(|row| row.id).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn sql_rows(db: &mut Database, text: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match db.sql(text, &[]).unwrap() {
        SqlResult::Rows { columns, rows } => {
            (columns, rows.into_iter().map(|row| row.values).collect())
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

// ── the oracle ────────────────────────────────────────────────────────────

fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The stated rule: no edit up to four characters, one up to eight, two
/// beyond. Written out here so the test fails if the engine changes it.
fn edit_bound(chars: usize) -> u32 {
    match chars {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

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

fn prefix_distance(term: &[char], token: &[char]) -> u32 {
    (0..=term.len())
        .map(|at| distance(&term[..at], token))
        .min()
        .expect("the range holds the empty prefix")
}

/// The external keys `search(text, query)` must name over the fixture.
fn oracle_keys(f: &fixture::Fixture, query: &str) -> BTreeSet<String> {
    let dictionary: BTreeSet<String> = f
        .rows
        .iter()
        .flat_map(|row| tokens(&row.text))
        .collect();
    let written = tokens(query);
    let last = written.len().saturating_sub(1);
    let groups: Vec<BTreeSet<String>> = written
        .iter()
        .enumerate()
        .map(|(at, token)| {
            let token: Vec<char> = token.chars().collect();
            let bound = edit_bound(token.len());
            dictionary
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
        .collect();
    if groups.is_empty() {
        return BTreeSet::new();
    }
    f.rows
        .iter()
        .filter(|row| {
            let held = tokens(&row.text);
            groups
                .iter()
                .all(|group| group.iter().any(|term| held.contains(term)))
        })
        .map(|row| row.key.clone())
        .collect()
}

fn keys_of(f: &fixture::Fixture, ids: &[EntityId]) -> BTreeSet<String> {
    ids.iter()
        .map(|id| f.rows[(id.sequence - 1) as usize].key.clone())
        .collect()
}

// ── the predicate ─────────────────────────────────────────────────────────

#[test]
fn a_typo_tolerant_search_names_the_rows_a_brute_force_walk_of_the_vocabulary_names() {
    let (_dir, mut f) = open();
    // As written, with one substitution, with one deletion, and truncated so
    // the final token has to complete.
    for query in [
        "kebun",
        "kebin",
        "kebu",
        "sekolah",
        "sekolzh",
        "sekol",
        "jembatan kopi",
        "jembztan kopi",
        "kebun sekol",
    ] {
        let statement =
            format!("SELECT _id FROM place WHERE search(text, '{query}') ORDER BY _id");
        let ids = sql_ids(&mut f.db, &statement);
        let found = keys_of(&f, &ids);
        assert_eq!(
            found,
            oracle_keys(&f, query),
            "search({query:?}) named different rows than the brute-force oracle"
        );
        assert!(!found.is_empty(), "search({query:?}) named no row at all");
    }
}

#[test]
fn the_last_token_of_a_search_completes_as_a_prefix_and_an_earlier_token_does_not() {
    let (_dir, mut f) = open();
    // `sekol` is five characters: one edit, which does not reach `sekolah`.
    // Written last it completes; written first it must not.
    let completing = sql_ids(&mut f.db, "SELECT _id FROM place WHERE search(text, 'sekol')");
    assert!(!completing.is_empty());
    assert_eq!(keys_of(&f, &completing), oracle_keys(&f, "sekol"));
    let earlier = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE search(text, 'sekol kopi')",
    );
    assert!(
        earlier.is_empty(),
        "an earlier token must match a whole term, not a prefix"
    );
    assert!(oracle_keys(&f, "sekol kopi").is_empty());
}

#[test]
fn a_search_composes_with_a_scalar_filter_beside_it_and_inside_an_or() {
    let (_dir, mut f) = open();
    let beside = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = 'depot' AND search(text, 'kebu')",
    );
    let expected: BTreeSet<String> = oracle_keys(&f, "kebu")
        .into_iter()
        .filter(|key| {
            f.rows
                .iter()
                .find(|row| row.key == *key)
                .is_some_and(|row| row.kind == "depot")
        })
        .collect();
    assert_eq!(keys_of(&f, &beside), expected);
    assert!(!expected.is_empty());

    // A `search()` leaf inside a disjunction HAS a membership set: the text
    // merge is the authority for the documents it admits, exactly as it is
    // for `Any` and `All`, so the union is built from postings alone.
    let disjunction = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE search(text, 'kebu') OR kind = 'depot'",
    );
    let union: BTreeSet<String> = oracle_keys(&f, "kebu")
        .into_iter()
        .chain(
            f.rows
                .iter()
                .filter(|row| row.kind == "depot")
                .map(|row| row.key.clone()),
        )
        .collect();
    assert_eq!(keys_of(&f, &disjunction), union);
}

// ── the score ─────────────────────────────────────────────────────────────

#[test]
fn search_score_is_one_for_an_exact_match_and_falls_with_the_edits_and_the_completion() {
    let (_dir, mut f) = open();
    let best = |f: &mut fixture::Fixture, query: &str| -> f64 {
        let (columns, rows) = sql_rows(
            &mut f.db,
            &format!(
                "SELECT _id, search_score() AS score FROM place \
                 WHERE search(text, '{query}') ORDER BY search_score() DESC LIMIT 1"
            ),
        );
        assert_eq!(columns, vec!["_id", "score"]);
        match rows[0][1] {
            SqlValue::Float(value) => value,
            ref other => panic!("a search score is a float, got {other:?}"),
        }
    };
    let exact = best(&mut f, "kebun");
    assert!(
        (exact - 1.0).abs() < 1e-12,
        "an exact term match scores 1.0, found {exact}"
    );
    let edited = best(&mut f, "kebin");
    assert!(
        edited < exact,
        "one edit ({edited}) scores strictly less than none ({exact})"
    );
    let long = best(&mut f, "kebu");
    let short = best(&mut f, "keb");
    assert!(
        long > short,
        "completing one character ({long}) scores strictly more than completing two ({short})"
    );
    assert!(short > 0.0, "a matched row scores above zero, found {short}");
}

#[test]
fn a_search_score_order_descends_and_reaches_every_row_the_predicate_admits() {
    let (_dir, mut f) = open();
    let (_, rows) = sql_rows(
        &mut f.db,
        "SELECT _id, search_score() AS score FROM place \
         WHERE search(text, 'kebu') ORDER BY search_score() DESC",
    );
    let scores: Vec<f64> = rows
        .iter()
        .map(|row| match row[1] {
            SqlValue::Float(value) => value,
            ref other => panic!("a search score is a float, got {other:?}"),
        })
        .collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    assert!(scores.iter().all(|score| *score > 0.0 && *score <= 1.0));
    assert_eq!(scores.len(), oracle_keys(&f, "kebu").len());
}

#[test]
fn a_search_score_blends_with_bm25_and_a_vector_distance_in_one_order_expression() {
    let (_dir, mut f) = open();
    let vector: Vec<String> = f.rows[3].emb.iter().map(|lane| lane.to_string()).collect();
    let literal = format!("[{}]", vector.join(","));
    let ids = sql_ids(
        &mut f.db,
        &format!(
            "SELECT _id FROM place WHERE search(text, 'kebun') \
             ORDER BY 0.5 * search_score() + 0.3 * bm25(text, 'kebun') \
             + 0.2 * (1 - (emb <=> '{literal}')) DESC LIMIT 10"
        ),
    );
    assert_eq!(ids.len(), 10);
    assert!(keys_of(&f, &ids).is_subset(&oracle_keys(&f, "kebun")));
}

#[test]
fn search_score_without_a_search_in_the_statement_is_refused_by_name() {
    let (_dir, mut f) = open();
    let error = f
        .db
        .sql(
            "SELECT _id FROM place WHERE kind = 'depot' ORDER BY search_score() DESC",
            &[],
        )
        .unwrap_err();
    match error {
        SqlError::Refused {
            keyword,
            tier,
            reason,
        } => {
            assert_eq!(keyword, "search_score");
            assert_eq!(tier, Tier::Two);
            assert!(reason.contains("search()"), "{reason}");
        }
        other => panic!("expected a refusal by name, got {other:?}"),
    }
    // And two `search()` predicates leave it with no single one to score.
    let error = f
        .db
        .sql(
            "SELECT _id FROM place WHERE search(text, 'kebun') AND search(text, 'kopi') \
             ORDER BY search_score() DESC",
            &[],
        )
        .unwrap_err();
    assert!(
        matches!(&error, SqlError::Refused { keyword, .. } if keyword == "search_score"),
        "{error:?}"
    );
}

#[test]
fn explain_names_the_search_driver_its_expanded_terms_and_the_score_leaf() {
    let (_dir, f) = open();
    let text = f
        .db
        .sql_explain(
            "SELECT _id FROM place WHERE search(text, 'kebu') ORDER BY search_score() DESC LIMIT 5",
            &[],
        )
        .unwrap();
    // The driver is the text merge over the EXPANDED terms, and the plan says
    // how many tokens became how many dictionary terms rather than printing a
    // count a reader would take for the query's own.
    assert!(text.contains("search, 1 token(s) expanded to"), "{text}");
    assert!(text.contains("order: score"), "{text}");
    assert!(text.contains("search_score place_text"), "{text}");
    assert!(!text.contains("TRUNCATED"), "{text}");
}

// ── the bounds ────────────────────────────────────────────────────────────

#[test]
fn a_truncated_dictionary_walk_reaches_the_client_as_a_notice_that_says_so() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("db"),
        kernel::store::Config {
            budget_bytes: 1 << 20,
            io: kernel::io::IoMode::Buffered,
            sync: kernel::store::SyncMode::Full,
        },
    )
    .unwrap();
    let docs = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    // A dictionary far wider than the visit bound, every term reachable from
    // a nine-character token by completing a prefix.
    for n in 0..6_000usize {
        db.put(
            docs,
            &format!("p{n}"),
            &json!({ "body": format!("aaaaaaaaa{n:05}") }),
        )
        .unwrap();
    }
    db.commit().unwrap();
    let index = db.create_text_index(docs, "body_text", "body").unwrap();
    while !db.build_index_step(index, 256).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();

    let prepared = prepare_sql(
        &db,
        "SELECT _id FROM docs WHERE search(body, 'aaaaaaaaa')",
        &[],
    )
    .unwrap();
    let notice = prepared
        .notices()
        .iter()
        .find(|line| line.starts_with("search() stopped at its bound"))
        .unwrap_or_else(|| panic!("no truncation notice in {:?}", prepared.notices()));
    assert!(notice.contains("dictionary entries visited"), "{notice}");
    assert!(notice.contains("not every term within the edit bound"), "{notice}");
    // The statement still ANSWERS -- the notice is what makes the short list
    // honest, not an error that hides it.
    let SqlResult::Rows { rows, .. } = db
        .sql("SELECT _id FROM docs WHERE search(body, 'aaaaaaaaa')", &[])
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert!(!rows.is_empty());

    // A query the dictionary cannot reach prunes instead of truncating, so
    // an ordinary search carries no notice.
    let quiet = prepare_sql(&db, "SELECT _id FROM docs WHERE search(body, 'kopi')", &[]).unwrap();
    assert!(
        !quiet
            .notices()
            .iter()
            .any(|line| line.starts_with("search() stopped")),
        "{:?}",
        quiet.notices()
    );
}

#[test]
fn a_search_answers_under_a_query_budget_and_a_refusal_names_its_resource() {
    let (_dir, f) = open();
    let statement = "SELECT _id FROM place WHERE search(text, 'kebu') ORDER BY search_score() DESC";
    let prepared = prepare_sql(&f.db, statement, &[]).unwrap();
    let mut seen = 0usize;
    prepared
        .for_each_row_with(
            &f.db,
            256,
            QueryBudget::unlimited(),
            &mut || false,
            &mut |_| {
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
    assert!(seen > 0, "an unbounded search answers");
    let mut budget = QueryBudget::unlimited();
    budget.text_postings = 2;
    let refused = prepared
        .for_each_row_with(&f.db, 256, budget, &mut || false, &mut |_| Ok(()))
        .unwrap_err();
    match &refused {
        // A budget refusal reaches SQL as the engine's own message, which
        // NAMES the resource that ran out.
        SqlError::Engine(message) => assert!(
            message.contains("TextPostings"),
            "a bounded search must name its resource, got {message}"
        ),
        other => panic!("a bounded search refuses by resource, got {other:?}"),
    }
}
