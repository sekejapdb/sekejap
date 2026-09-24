//! 2h: full-text vs an independent oracle. The oracle tokenizes, counts
//! and scores with its OWN inline math -- it never calls the engine's
//! tokenizer, postings, or BM25 (the 2e lesson).

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};
use std::collections::HashMap;

/// One vector field for these tests; to the kernel a field is an
/// opaque u64, so any constant names it.
const VF: u64 = 1;

fn cfg() -> Config {
    Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

const F: u64 = 1; // the field under test ("title")

/// Victorian-generic corpus, per privacy rules.
fn corpus() -> Vec<(u64, &'static str)> {
    vec![
        (1, "The Adventure of the Speckled Band"),
        (2, "A Study in Scarlet"),
        (3, "The Sign of the Four"),
        (4, "The Hound of the Baskervilles"),
        (5, "The Adventure of the Copper Beeches"),
        (6, "Adventures of a Copper Kettle"),
        (7, "The Speckled Trout of Baskerville Creek"),
        (8, "Band of Brothers in Scarlet Cloaks"),
        (9, "Copper Band Resistance Study"),
        (10, "Study of the Speckled Copper Band"),
    ]
}

/// Independent oracle: lowercase alphanumeric split, BM25 with the same
/// constants, computed from raw text alone.
fn oracle_bm25(docs: &[(u64, &str)], query: &str, k: usize) -> Vec<u64> {
    let tok = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect()
    };
    let toks: HashMap<u64, Vec<String>> =
        docs.iter().map(|(id, t)| (*id, tok(t))).collect();
    let n = docs.len() as f64;
    let avg = toks.values().map(|v| v.len()).sum::<usize>() as f64 / n;
    let qterms: Vec<String> = {
        let mut q = tok(query); q.dedup(); q
    };
    let mut scored: Vec<(f64, u64)> = docs.iter().map(|(id, _)| {
        let dts = &toks[id];
        let dl = dts.len() as f64;
        let mut s = 0.0;
        for qt in &qterms {
            let df = toks.values().filter(|d| d.contains(qt)).count() as f64;
            if df == 0.0 { continue; }
            let tf = dts.iter().filter(|t| *t == qt).count() as f64;
            if tf == 0.0 { continue; }
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            s += idf * tf * (1.2 + 1.0) / (tf + 1.2 * (1.0 - 0.75 + 0.75 * dl / avg));
        }
        (s, *id)
    }).filter(|(s, _)| *s > 0.0).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(k);
    scored.into_iter().map(|(_, id)| id).collect()
}

#[test]
fn bm25_ranking_matches_the_independent_oracle() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (id, text) in corpus() { g.index_text(F, id, text).unwrap(); }
    g.commit().unwrap();

    for query in ["speckled band", "copper", "study scarlet", "the adventure",
                  "baskervilles hound", "band band band"] {
        let want = oracle_bm25(&corpus(), query, 5);
        let got: Vec<u64> = g.text_search(F, query, 5).unwrap()
            .into_iter().map(|(id, _)| id).collect();
        assert_eq!(got, want, "query {query:?} ranking diverged from oracle");
    }
}

/// A literal order beside the independent-math oracle.  The scorer's storage
/// path is about to change from decoding whole posting lists to point-reading
/// live statistics and each candidate's own term frequencies.  These exact
/// ids pin the maths while that plumbing moves: a plausible-looking but
/// slightly different order is a regression, not an acceptable approximation.
#[test]
fn bm25_exact_ranked_order_is_pinned() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (id, text) in corpus() { g.index_text(F, id, text).unwrap(); }
    g.commit().unwrap();

    let got: Vec<u64> = g.text_search(F, "speckled copper band", 5).unwrap()
        .into_iter().map(|(id, _)| id).collect();
    assert_eq!(got, vec![10, 9, 1, 7, 6]);

    let candidates: Vec<u64> = corpus().into_iter().map(|(id, _)| id).collect();
    let scores = g.text_score_candidates(F, "speckled copper band", &candidates).unwrap();
    let mut point_ranked: Vec<(u64, f32)> = candidates.into_iter().zip(scores)
        .filter(|(_, score)| *score > 0.0).collect();
    point_ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    assert_eq!(point_ranked.into_iter().take(5).map(|(id, _)| id).collect::<Vec<_>>(),
        vec![10, 9, 1, 7, 6], "candidate point scoring changed the exact ranked order");
}

#[test]
fn indexing_is_immediate_and_survives_crash() {
    let d = tempfile::TempDir::new().unwrap();
    {
        let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
        g.index_text(F, 1, "instant search lands instantly").unwrap();
        // searchable before ANY commit -- the head is just rows
        assert_eq!(g.text_search(F, "instantly", 5).unwrap()[0].0, 1);
        g.commit().unwrap();
        g.index_text(F, 2, "committed later but crashed first").unwrap();
        g.commit().unwrap();
        // crash: no checkpoint
    }
    let g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    assert_eq!(g.text_search(F, "instantly", 5).unwrap()[0].0, 1);
    assert_eq!(g.text_search(F, "crashed", 5).unwrap()[0].0, 2,
               "WAL replay must restore text rows");
}

#[test]
fn a_deleted_document_vanishes_from_results() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (id, text) in corpus() { g.index_text(F, id, text).unwrap(); }
    g.commit().unwrap();
    let before: Vec<u64> = g.text_search(F, "copper", 10).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert!(before.contains(&6));
    assert!(g.delete_text(F, 6).unwrap());
    g.commit().unwrap();
    let after: Vec<u64> = g.text_search(F, "copper", 10).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert!(!after.contains(&6), "dead doc must not surface");
    assert!(after.contains(&5), "other docs unaffected");
}

#[test]
fn deleted_documents_do_not_change_survivor_ranking_or_length_normalisation() {
    let d1 = tempfile::TempDir::new().unwrap();
    let d2 = tempfile::TempDir::new().unwrap();
    let docs = [
        (1, "railway railway short"),
        (2, "railway bridge survey with a much longer description"),
        (3, "railway timetable"),
        (4, "railway deleted document with many many many extra tokens"),
    ];
    let mut churned = Graph::new(Store::create(d1.path(), cfg()).unwrap()).unwrap();
    for (id, text) in docs { churned.index_text(F, id, text).unwrap(); }
    churned.fold_text(F).unwrap();
    churned.delete_text(F, 4).unwrap();

    let mut clean = Graph::new(Store::create(d2.path(), cfg()).unwrap()).unwrap();
    for (id, text) in docs.into_iter().filter(|(id, _)| *id != 4) {
        clean.index_text(F, id, text).unwrap();
    }
    assert_eq!(churned.text_search(F, "railway survey", 10).unwrap(),
               clean.text_search(F, "railway survey", 10).unwrap(),
               "a tombstoned document drifted survivor IDF or average length");
}

#[test]
fn folding_preserves_every_answer_and_drops_the_dead() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for (id, text) in corpus() { g.index_text(F, id, text).unwrap(); }
    g.delete_text(F, 6).unwrap();
    g.commit().unwrap();

    let queries = ["speckled band", "copper", "study scarlet", "the adventure"];
    let before: Vec<Vec<(u64, f64)>> =
        queries.iter().map(|q| g.text_search(F, q, 10).unwrap()).collect();

    g.fold_text(F).unwrap();

    for (q, want) in queries.iter().zip(&before) {
        let got = g.text_search(F, q, 10).unwrap();
        assert_eq!(&got, want, "query {q:?} changed across the fold");
    }
    // the head is gone; one folded segment remains
    assert_eq!(g.text_segments(F).unwrap(), vec![1], "head must be erased after publish");

    // new writes after the fold land in a fresh head and union with the segment
    g.index_text(F, 42, "a brand new speckled document").unwrap();
    let ids: Vec<u64> = g.text_search(F, "speckled", 10).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert!(ids.contains(&42) && ids.contains(&1) && ids.contains(&10),
            "head and folded segment must serve together, got {ids:?}");
}

#[test]
fn a_fold_bigger_than_one_chunk_stays_exact() {
    // 3000 docs sharing one very common term: that term's rows straddle the
    // 8192-row chunk cut, exercising resume + the delta-rebase merge of one
    // term written across two passes.
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for i in 1..=3000u64 {
        let text = format!("common word plus unique{i} and filler{} bits", i % 7);
        g.index_text(F, i, &text).unwrap();
    }
    g.commit().unwrap();
    let before = g.text_postings(F, "common").unwrap();
    assert_eq!(before.len(), 3000);
    g.fold_text(F).unwrap();
    let after = g.text_postings(F, "common").unwrap();
    assert_eq!(after, before, "chunked fold corrupted the common term's postings");
    assert_eq!(g.text_postings(F, "unique2999").unwrap(), vec![(2999, 1, 7)]);
}

#[test]
fn incremental_document_stats_equal_a_recount_after_churn_and_reopen() {
    let d = tempfile::TempDir::new().unwrap();
    let assert_recounts = |g: &Graph| {
        assert_eq!(g.text_live_stats(F).unwrap(), g.text_recount_stats(F).unwrap());
        for term in ["copper", "lines", "revised", "plain"] {
            assert_eq!(g.text_term_doc_freq(F, term).unwrap().unwrap_or(0),
                g.text_recount_term_doc_freq(F, term).unwrap(), "term {term:?}");
        }
    };
    {
        let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
        g.index_text(F, 1, "one copper line").unwrap();
        g.index_text(F, 2, "two copper lines here").unwrap();
        g.index_text(F, 3, "three plain lines").unwrap();
        assert_recounts(&g);
        g.replace_text(F, 2, Some("two copper lines here"), "two revised lines").unwrap();
        assert_recounts(&g);
        g.delete_text(F, 1).unwrap();
        assert_recounts(&g);
        g.commit().unwrap();
        g.checkpoint().unwrap();
    }
    let g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    assert_eq!(g.text_live_stats(F).unwrap(), (2, 6));
    assert_recounts(&g);
    assert_eq!(g.text_term_doc_freq(F, "copper").unwrap(), Some(0));
}

#[test]
fn leveled_merge_preserves_exact_results_and_bounds_batch_fanout() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    for round in 0..7u64 {
        for i in 0..12u64 {
            let id = round * 100 + i + 1;
            g.index_text(F, id, &format!("common railway round{round} item{i}")).unwrap();
        }
        g.fold_text(F).unwrap();
    }
    for i in 0..12u64 {
        let id = 700 + i + 1;
        g.index_text(F, id, &format!("common railway round7 item{i}")).unwrap();
    }
    let queries = ["common", "railway round3", "item4"];
    let before: Vec<_> = queries.iter().map(|q| g.text_search(F, q, 200).unwrap()).collect();
    g.fold_text(F).unwrap(); // eighth L0 triggers one eight-way L1 merge
    for (query, want) in queries.iter().zip(before) {
        assert_eq!(g.text_search(F, query, 200).unwrap(), want,
            "query {query:?} changed when eight immutable batches merged");
    }
    assert_eq!(g.text_segments(F).unwrap().len(), 1,
        "eight same-level batches must become one next-level batch");
}

/// The music-library shape: short titles, typed prefix, typos. Oracle = brute
/// force over the corpus with an independent edit-distance function.
#[test]
fn instant_search_prefix_and_typos() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let artists = [
        (1u64, "Ludwig van Beethoven"),
        (2, "Johann Sebastian Bach"),
        (3, "Wolfgang Amadeus Mozart"),
        (4, "Johannes Brahms"),
        (5, "Franz Schubert"),
        (6, "Bedrich Smetana"),
        (7, "The Beatles"),
        (8, "Beach House"),
    ];
    for (id, name) in artists { g.index_text(F, id, name).unwrap(); }
    g.commit().unwrap();

    // typing "beetho" -> prefix expansion must find Beethoven
    let ids: Vec<u64> = g.text_search_instant(F, "beetho", 5).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert_eq!(ids, vec![1], "prefix beetho must reach beethoven, got {ids:?}");

    // typo: "beethovan" (9 chars -> budget 2) exact-word search
    let ids: Vec<u64> = g.text_search_instant(F, "beethovan", 5).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert!(ids.contains(&1), "one typo must still find beethoven, got {ids:?}");

    // two tokens, AND semantics: "johann ba" -> Bach, not Brahms
    let ids: Vec<u64> = g.text_search_instant(F, "johann ba", 5).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert_eq!(ids.first(), Some(&2), "johann + prefix ba must rank Bach first, got {ids:?}");

    // short tokens get NO typo budget: "bxch" (4 chars) finds nothing
    let ids = g.text_search_instant(F, "bxch", 5).unwrap();
    assert!(ids.is_empty(), "4-char tokens have zero typo budget, got {ids:?}");

    // survives a fold identically
    g.fold_text(F).unwrap();
    let ids: Vec<u64> = g.text_search_instant(F, "beetho", 5).unwrap()
        .into_iter().map(|(i, _)| i).collect();
    assert_eq!(ids, vec![1], "instant search must survive the fold");
}

/// The fuzzy walk agrees with a brute-force independent Levenshtein.
#[test]
fn fuzzy_terms_match_bruteforce_edit_distance() {
    fn lev(a: &str, b: &str) -> u32 {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut row: Vec<u32> = (0..=b.len() as u32).collect();
        for i in 1..=a.len() {
            let mut prev = row[0]; row[0] = i as u32;
            for j in 1..=b.len() {
                let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
                let cur = (row[j] + 1).min(row[j-1] + 1).min(prev + cost);
                prev = row[j]; row[j] = cur;
            }
        }
        row[b.len()]
    }
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let words = ["band", "bend", "bind", "bond", "brand", "bland", "grand",
                 "banned", "sand", "hand", "handle", "candle"];
    for (i, w) in words.iter().enumerate() { g.index_text(F, i as u64 + 1, w).unwrap(); }
    g.commit().unwrap();
    for query in ["band", "bnad", "grande", "cndle"] {
        for max in [1u32, 2] {
            let want: Vec<String> = words.iter()
                .filter(|w| lev(query, w) <= max).map(|w| w.to_string()).collect();
            let mut got: Vec<String> = g.text_fuzzy_terms(F, query, max, 100).unwrap()
                .into_iter().map(|(t, _)| t).collect();
            got.sort();
            let mut want = want; want.sort();
            assert_eq!(got, want, "query {query:?} max_edits {max}");
        }
    }
}

/// The hybrid-RAG shape: full-text narrows to candidates, vectors
/// rank the survivors. One flow, two families, exact result checked
/// against doing both steps by hand.
#[test]
fn text_candidates_feed_vector_rescore() {
    use kernel::graph::Metric;
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    // 200 "datasets": half about rivers, half about railways; each carries
    // a 32-dim embedding whose first lane encodes its topic strength.
    for i in 1..=200u64 {
        let (topic, strength) = if i % 2 == 0 { ("river flow measurements", (i % 50) as f32) }
                                else { ("railway freight records", (i % 50) as f32) };
        g.index_text(F, i, &format!("{topic} volume {i}")).unwrap();
        let mut v = vec![0.05f32; 32];
        v[0] = strength;
        g.set_vec(VF, i, &v).unwrap();
    }
    g.commit().unwrap();

    // step 1: text narrows to river datasets; step 2: nearest to a strong
    // query vector among THOSE only.
    let cands: Vec<u64> = g.text_search(F, "river measurements", 200).unwrap()
        .into_iter().map(|(id, _)| id).collect();
    assert!(cands.iter().all(|id| id % 2 == 0), "text stage must only pass rivers");
    let mut q = vec![0.05f32; 32];
    q[0] = 49.0;
    let top = g.rescore(VF, &cands, &q, Metric::L2, 5).unwrap();
    // hand oracle: even ids with strength nearest 49
    for (id, _) in &top {
        assert_eq!(id % 2, 0);
        let strength = (id % 50) as f32;
        assert!((strength - 49.0).abs() <= 3.0,
                "id {id} strength {strength} should be near 49");
    }
    assert_eq!(top.len(), 5);
}
