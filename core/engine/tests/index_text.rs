//! Phase 2 text-index contract. Tokenization and BM25 expectations here are
//! implemented independently of the engine analyzer and scorer.
use sekejap_core::{
    collections::{
        CollectionOptions, Database, EntityId, Error, IndexFamily, IndexId, IndexState,
        TextCandidates, TextHit, TextMatch,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn finish_build(db: &mut Database, index: IndexId, batch: usize) {
    while !db.build_index_step(index, batch).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
}

fn query(
    db: &Database,
    index: IndexId,
    text: &str,
    matching: TextMatch,
    k: usize,
    candidates: TextCandidates<'_>,
    max_examined: usize,
) -> sekejap_core::collections::Result<Vec<TextHit>> {
    db.query_text(index, text, matching, k, candidates, max_examined, || false)
}

fn ascii_terms(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn contains_ordered_phrase(text: &str, phrase: &str) -> bool {
    let tokens = ascii_terms(text);
    let needle = ascii_terms(phrase);
    !needle.is_empty() && tokens.windows(needle.len()).any(|window| window == needle)
}

fn oracle(
    rows: &BTreeMap<EntityId, Option<&str>>,
    query: &str,
    matching: TextMatch,
    candidates: Option<&[EntityId]>,
    k: usize,
) -> Vec<TextHit> {
    let analyzed: BTreeMap<_, _> = rows
        .iter()
        .filter_map(|(id, text)| {
            text.map(|text| {
                let terms = ascii_terms(text);
                (*id, (terms.len() as u32, terms))
            })
        })
        .collect();
    let query: BTreeSet<_> = ascii_terms(query).into_iter().collect();
    let n = analyzed.len() as f64;
    let total: usize = analyzed.values().map(|(_, terms)| terms.len()).sum();
    let average = total as f64 / n;
    let mut dfs = BTreeMap::new();
    for term in &query {
        let df = analyzed
            .values()
            .filter(|(_, terms)| terms.contains(term))
            .count() as f64;
        dfs.insert(term, df);
    }
    let mut hits = Vec::new();
    for (id, (length, terms)) in analyzed {
        if candidates.is_some_and(|ids| ids.binary_search(&id).is_err()) {
            continue;
        }
        let frequencies: BTreeMap<_, _> =
            terms.into_iter().fold(BTreeMap::new(), |mut map, term| {
                *map.entry(term).or_insert(0u32) += 1;
                map
            });
        let matched: Vec<_> = query
            .iter()
            .filter_map(|term| frequencies.get(term).map(|tf| (term, *tf)))
            .collect();
        if matched.is_empty() || (matching == TextMatch::All && matched.len() != query.len()) {
            continue;
        }
        let mut score = 0.0;
        for (term, tf) in matched {
            let df = dfs[term];
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            let tf = f64::from(tf);
            score += idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * f64::from(length) / average));
        }
        hits.push(TextHit { id, score });
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    hits.truncate(k);
    hits
}

fn assert_hits(actual: &[TextHit], expected: &[TextHit]) {
    assert_eq!(
        actual.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        expected.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert!((actual.score - expected.score).abs() <= 1e-14);
    }
}

#[test]
fn tiny_ranking_any_all_and_candidate_filter_match_independent_bm25() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let texts = [
        "rust database database",
        "rust search",
        "database search search",
        "other sole",
        "rust database search",
        "rust",
    ];
    let mut rows = BTreeMap::new();
    let mut ids = Vec::new();
    for (number, text) in texts.into_iter().enumerate() {
        let id = db
            .put(collection, &format!("p{number}"), &json!({"body":text}))
            .unwrap();
        ids.push(id);
        rows.insert(id, Some(text));
    }
    db.commit().unwrap();
    let index = db
        .create_text_index(collection, "body_text", "body")
        .unwrap();
    assert_eq!(db.index_info(index).unwrap().family, IndexFamily::Text);
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { .. }
    ));
    assert!(query(
        &db,
        index,
        "rust database",
        TextMatch::Any,
        6,
        TextCandidates::All,
        6
    )
    .is_err());
    finish_build(&mut db, index, 2);

    for matching in [TextMatch::Any, TextMatch::All] {
        assert_hits(
            &query(
                &db,
                index,
                "rust database rust",
                matching,
                6,
                TextCandidates::All,
                16,
            )
            .unwrap(),
            &oracle(&rows, "rust database rust", matching, None, 6),
        );
    }
    assert!(matches!(
        query(
            &db,
            index,
            "other sole",
            TextMatch::All,
            1,
            TextCandidates::All,
            3,
        ),
        Err(Error::Kernel(kernel::Error::ResourceLimit(_)))
    ));
    assert_eq!(
        query(
            &db,
            index,
            "other sole",
            TextMatch::All,
            1,
            TextCandidates::All,
            4,
        )
        .unwrap()[0]
            .id,
        ids[3]
    );
    let candidates = [ids[1], ids[3], ids[4]];
    assert_hits(
        &query(
            &db,
            index,
            "rust database",
            TextMatch::Any,
            2,
            TextCandidates::SortedUnique(&candidates),
            candidates.len() * 3,
        )
        .unwrap(),
        &oracle(&rows, "rust database", TextMatch::Any, Some(&candidates), 2),
    );
}

#[test]
fn empty_missing_null_unicode_and_query_work_bounds_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let empty = db.put(collection, "empty", &json!({"body":""})).unwrap();
    let punctuation = db
        .put(collection, "punctuation", &json!({"body":"! -- _"}))
        .unwrap();
    let missing = db.put(collection, "missing", &json!({})).unwrap();
    let null = db
        .put(collection, "null", &json!({"body":Value::Null}))
        .unwrap();
    let unicode = db
        .put(
            collection,
            "unicode",
            &json!({"body":"İstanbul CAFÉ Straße 北京"}),
        )
        .unwrap();
    let index = db.create_text_index(collection, "body", "body").unwrap();
    finish_build(&mut db, index, 16);

    assert_eq!(
        query(
            &db,
            index,
            "İSTANBUL café STRASSE 北京",
            TextMatch::Any,
            8,
            TextCandidates::All,
            8,
        )
        .unwrap()
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>(),
        vec![unicode]
    );
    assert!(
        query(&db, index, "!!!", TextMatch::Any, 8, TextCandidates::All, 0,)
            .unwrap()
            .is_empty()
    );
    let absent = [empty, punctuation, missing, null];
    assert!(matches!(
        query(
            &db,
            index,
            "istanbul",
            TextMatch::Any,
            8,
            TextCandidates::SortedUnique(&absent),
            5,
        ),
        Err(Error::Kernel(kernel::Error::ResourceLimit(_)))
    ));
    assert!(query(
        &db,
        index,
        "istanbul",
        TextMatch::Any,
        8,
        TextCandidates::SortedUnique(&absent),
        6,
    )
    .unwrap()
    .is_empty());
    assert!(matches!(
        query(
            &db,
            index,
            "istanbul",
            TextMatch::Any,
            8,
            TextCandidates::All,
            0,
        ),
        Err(Error::Kernel(kernel::Error::ResourceLimit(_)))
    ));
    let mut calls = 0;
    assert!(matches!(
        db.query_text(
            index,
            "istanbul",
            TextMatch::Any,
            8,
            TextCandidates::All,
            1,
            || {
                calls += 1;
                calls == 2
            },
        ),
        Err(Error::Cancelled)
    ));
    assert!(query(
        &db,
        index,
        &(0..65)
            .map(|number| format!("term{number}"))
            .collect::<Vec<_>>()
            .join(" "),
        TextMatch::Any,
        1,
        TextCandidates::All,
        1,
    )
    .is_err());
}

#[test]
fn building_live_crud_snapshot_rollback_and_reopen_preserve_presence_rules() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let first = db
        .put(collection, "first", &json!({"body":"alpha"}))
        .unwrap();
    let second = db
        .put(collection, "second", &json!({"body":"beta"}))
        .unwrap();
    db.commit().unwrap();
    let index = db.create_text_index(collection, "body", "body").unwrap();
    db.update(collection, "second", &json!({"body":"gamma"}))
        .unwrap();
    let live = db
        .put(collection, "live", &json!({"body":"alpha gamma"}))
        .unwrap();
    assert!(!db.build_index_step(index, 1).unwrap());
    db.update(collection, "first", &json!({"body":"delta"}))
        .unwrap();
    finish_build(&mut db, index, 1);

    assert_eq!(
        query(
            &db,
            index,
            "gamma",
            TextMatch::Any,
            8,
            TextCandidates::All,
            8,
        )
        .unwrap()
        .iter()
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>(),
        [second, live].into_iter().collect()
    );
    assert!(query(
        &db,
        index,
        "alpha",
        TextMatch::Any,
        8,
        TextCandidates::SortedUnique(&[first]),
        2,
    )
    .unwrap()
    .is_empty());
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();

    db.update(collection, "live", &json!({"body":"rolled back"}))
        .unwrap();
    db.rollback().unwrap();
    assert_eq!(
        query(
            &db,
            index,
            "gamma",
            TextMatch::Any,
            8,
            TextCandidates::SortedUnique(&[live]),
            2,
        )
        .unwrap()[0]
            .id,
        live
    );
    assert!(db.delete(collection, "second").unwrap());
    let replacement = db
        .put(collection, "second", &json!({"body":"epsilon"}))
        .unwrap();
    assert_ne!(replacement, second);
    db.commit().unwrap();
    assert_eq!(
        query(
            &snapshot,
            index,
            "gamma",
            TextMatch::Any,
            8,
            TextCandidates::All,
            8,
        )
        .unwrap()
        .len(),
        2
    );
    drop(snapshot);
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        query(
            &db,
            index,
            "epsilon",
            TextMatch::Any,
            8,
            TextCandidates::All,
            8,
        )
        .unwrap()[0]
            .id,
        replacement
    );
}

#[test]
fn two_fields_and_collections_are_isolated_and_drop_resumes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let a = db
        .create_collection(
            "a",
            vec![("title".into(), Kind::Text), ("body".into(), Kind::Text)],
            Default::default(),
        )
        .unwrap();
    let b = db
        .create_collection("b", vec![("title".into(), Kind::Text)], Default::default())
        .unwrap();
    let aid = db
        .put(a, "a", &json!({"title":"needle","body":"haystack"}))
        .unwrap();
    let bid = db.put(b, "b", &json!({"title":"needle"})).unwrap();
    let title = db.create_text_index(a, "title", "title").unwrap();
    let body = db.create_text_index(a, "body", "body").unwrap();
    let other = db.create_text_index(b, "title", "title").unwrap();
    finish_build(&mut db, title, 1);
    finish_build(&mut db, body, 1);
    finish_build(&mut db, other, 1);
    assert_eq!(
        query(
            &db,
            title,
            "needle",
            TextMatch::Any,
            8,
            TextCandidates::All,
            2
        )
        .unwrap()[0]
            .id,
        aid
    );
    assert!(query(
        &db,
        body,
        "needle",
        TextMatch::Any,
        8,
        TextCandidates::All,
        1
    )
    .unwrap()
    .is_empty());
    assert_eq!(
        query(
            &db,
            other,
            "needle",
            TextMatch::Any,
            8,
            TextCandidates::All,
            2
        )
        .unwrap()[0]
            .id,
        bid
    );

    db.begin_drop_index(title).unwrap();
    assert!(!db.drop_index_step(title, 1).unwrap());
    db.commit().unwrap();
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    while !db.drop_index_step(title, 1).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(db.index_info(title).is_err());
    assert_eq!(
        query(
            &db,
            other,
            "needle",
            TextMatch::Any,
            8,
            TextCandidates::All,
            2
        )
        .unwrap()[0]
            .id,
        bid
    );
}

#[test]
fn phrase_is_ordered_contiguous_bounded_and_snapshot_authoritative() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let rows = [
        ("ordered", json!({"body":"Quick, BROWN fox"})),
        ("reversed", json!({"body":"brown quick fox"})),
        ("gap", json!({"body":"quick sly brown fox"})),
        ("repeat", json!({"body":"go go now"})),
        ("separated_repeat", json!({"body":"go now go"})),
        ("unicode", json!({"body":"İstanbul—ROCK rock"})),
        ("empty", json!({"body":""})),
        ("null", json!({"body":Value::Null})),
        ("missing", json!({})),
    ];
    let mut ids = BTreeMap::new();
    for (key, value) in rows {
        ids.insert(key, db.put(collection, key, &value).unwrap());
    }
    db.commit().unwrap();
    let index = db.create_text_index(collection, "body", "body").unwrap();
    finish_build(&mut db, index, 3);

    let hit_ids = |db: &Database, phrase: &str, candidates| {
        query(db, index, phrase, TextMatch::Phrase, 32, candidates, 10_000)
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        hit_ids(&db, "quick---brown", TextCandidates::All),
        [ids["ordered"]].into_iter().collect()
    );
    let ordered_only = [ids["ordered"]];
    assert!(hit_ids(
        &db,
        "brown quick",
        TextCandidates::SortedUnique(&ordered_only)
    )
    .is_empty());
    assert_eq!(
        hit_ids(&db, "go go", TextCandidates::All),
        [ids["repeat"]].into_iter().collect()
    );
    assert_eq!(
        hit_ids(&db, "İSTANBUL rock-rock", TextCandidates::All),
        [ids["unicode"]].into_iter().collect()
    );
    assert!(hit_ids(&db, "!!!", TextCandidates::All).is_empty());
    assert!(query(
        &db,
        index,
        &"term ".repeat(65),
        TextMatch::Phrase,
        1,
        TextCandidates::All,
        10_000,
    )
    .is_err());

    let mut calls = 0;
    assert!(matches!(
        db.query_text(
            index,
            "quick brown",
            TextMatch::Phrase,
            8,
            TextCandidates::All,
            10_000,
            || {
                calls += 1;
                calls == 8
            },
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        hit_ids(&db, "quick brown", TextCandidates::All),
        [ids["ordered"]].into_iter().collect()
    );

    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    db.update(collection, "ordered", &json!({"body":"brown quick fox"}))
        .unwrap();
    assert!(db.delete(collection, "repeat").unwrap());
    let replacement = db
        .put(collection, "repeat", &json!({"body":"go go again"}))
        .unwrap();
    assert_ne!(replacement, ids["repeat"]);
    db.commit().unwrap();

    assert_eq!(
        hit_ids(&snapshot, "quick brown", TextCandidates::All),
        [ids["ordered"]].into_iter().collect()
    );
    assert_eq!(
        hit_ids(&snapshot, "go go", TextCandidates::All),
        [ids["repeat"]].into_iter().collect()
    );
    drop(snapshot);
    assert!(hit_ids(&db, "quick brown", TextCandidates::All).is_empty());
    assert_eq!(
        hit_ids(&db, "go go", TextCandidates::All),
        [replacement].into_iter().collect()
    );
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        hit_ids(&db, "go go", TextCandidates::All),
        [replacement].into_iter().collect()
    );
}

#[test]
fn phrase_query_text_hits_max_examined_mid_scan() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let filler = (0..24)
        .map(|n| format!("w{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let phrase = "needle haystack";
    let mut rows = BTreeMap::new();
    for n in 0..40 {
        let body = match n % 3 {
            0 => format!("{filler} {phrase}"),
            1 => format!("{filler} haystack needle"),
            _ => format!("{filler} needle xxx haystack"),
        };
        let id = db
            .put(collection, &format!("d{n}"), &json!({"body": body}))
            .unwrap();
        rows.insert(id, body);
    }
    db.commit().unwrap();
    let index = db.create_text_index(collection, "body", "body").unwrap();
    finish_build(&mut db, index, 8);

    let expected: BTreeSet<_> = rows
        .iter()
        .filter(|(_, body)| contains_ordered_phrase(body, phrase))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(expected.len(), 14);

    // Term intersection over this corpus is cheap; phrase refinement also
    // spends one primary read plus one unit per document token, so the same
    // budget cannot hide the scan.
    const TIGHT: usize = 200;
    const GENEROUS: usize = 10_000;
    assert!(query(
        &db,
        index,
        phrase,
        TextMatch::All,
        40,
        TextCandidates::All,
        TIGHT,
    )
    .is_ok());
    assert!(matches!(
        query(
            &db,
            index,
            phrase,
            TextMatch::Phrase,
            40,
            TextCandidates::All,
            TIGHT,
        ),
        Err(Error::Kernel(kernel::Error::ResourceLimit(
            "text max_examined exceeded"
        )))
    ));
    let hits = query(
        &db,
        index,
        phrase,
        TextMatch::Phrase,
        40,
        TextCandidates::All,
        GENEROUS,
    )
    .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.id).collect::<BTreeSet<_>>(),
        expected
    );
}
