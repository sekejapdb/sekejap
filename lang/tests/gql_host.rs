//! The host forms inside a GQL body (M6-A, M6-B of
//! `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §3.2): the text match and
//! `bm25`, the `ST_*` predicates and `ST_Distance`, and the vector distances
//! `<->`, `<=>`, `<#>`.
//!
//! What is at risk, and the test that pins it:
//!
//! * a text match reads the node's text index, the one SQL reads, so both
//!   keep the same rows (`a_text_match_keeps_what_sql_matches`), a `$n`
//!   query included; `bm25` scores a matching node above zero
//!   (`bm25_scores_the_matching_nodes`); a label with no text index on the
//!   field is refused naming the index to create, never re-tokenized (Q27,
//!   `a_text_form_without_a_text_index_is_refused`);
//! * the spatial forms keep SQL's unit rules: metres under `::geography`,
//!   and the refusal naming the spelling otherwise
//!   (`the_spatial_forms_answer_in_metres_and_keep_the_unit_rules`);
//! * the vector distances keep pgvector's values and direction, from a
//!   literal or a `$n` (`the_vector_distances_are_pgvectors`), rank a
//!   top-k (`a_vector_distance_ranks_a_top_k`), and a width mismatch is a
//!   data error, `22000` (`a_vector_of_another_width_is_a_data_error`);
//! * `EXPLAIN` prints each form (`explain_prints_the_host_forms`);
//! * a seed node's text, spatial and scalar conjuncts seed the pattern
//!   through their READY indexes, intersected by the engine, and keep
//!   exactly the rows the budgeted FILTER scan keeps (M6-D,
//!   `every_index_answered_conjunct_seeds_the_node`), with `$n` read per
//!   execution and a `NULL` radius admitting no row
//!   (`a_seed_reads_its_parameters_per_execution`); a spatial form with no
//!   index is refused naming a GiST index, not a B-tree
//!   (`an_unindexed_spatial_seed_names_a_gist_index`);
//! * index lineage (M6-E): a later FILTER, a FILTER after NEXT over the
//!   columns that carry the node or its property, and the outer SELECT's
//!   WHERE move into the seed and keep the same rows
//!   (`a_later_conjunct_moves_into_the_seed_by_lineage`); nothing moves past
//!   an operator that chooses among rows (`nothing_moves_past_a_limit`) or out
//!   of a NOT EXISTS body (`nothing_moves_out_of_an_exists_body`);
//! * a limited sort by an `<->` / `<#>` distance of the seed node reads its
//!   seed in the exact index's order and stops early, with the full sort's
//!   answer and less work (M6-F,
//!   `a_top_k_by_distance_reads_the_index_order_and_stops_early`); cosine,
//!   a descending order and an unlimited sort keep the full sort
//!   (`only_an_order_the_index_gives_row_for_row_stops_early`); over a
//!   nullable column the ordered top-k keeps PostgreSQL's order -- numbers,
//!   an all-zero vector's NaN cosine, then the rows with no vector
//!   (`an_ordered_top_k_puts_nan_then_null_last_as_the_scan_does`);
//! * `ef_search` is read when an execution opens: one prepared plan answers
//!   exactly, then APPROXIMATELY under `SET LOCAL ef_search`, then exactly
//!   again after COMMIT (M6-G, Q29,
//!   `ef_search_is_read_when_the_execution_opens`); a column with ONLY an
//!   approximate index is exact without the knob (read unordered, sorted
//!   whole) and approximate through that index with it, as Oracle's
//!   `FETCH APPROX` and Spanner's `APPROX_` functions are opt-in
//!   (`an_approximate_only_column_is_exact_unless_ef_search_is_set`);
//! * a host form types its parameters once, as the wire describes them: a
//!   tsquery TEXT, a coordinate and a radius DOUBLE PRECISION, a distance's
//!   operand VECTOR, two disagreeing uses `42P08`, and a vector bound as
//!   pgvector's text form reads as the vector (M6-I,
//!   `host_form_parameters_are_typed_once`).
//!
//! Workload names are invented, from the README's tourism world: beaches of
//! Bali and the towns near them.

mod bali;

use bali::{fixture, float, keys, run, sqlstate};
use sekejap_core::collections::{Database, QueryBudget};
use sekejap_lang::{explain_sql, prepare_sql, Param, SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

#[test]
fn a_text_match_keeps_what_sql_matches() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    for (word, reached) in [("sunset", vec!["kuta", "seminyak"]), ("reef", vec!["nusa-dua", "sanur"])] {
        let SqlResult::Rows { rows, .. } = db
            .sql(
                "SELECT _key FROM beach WHERE to_tsvector('simple', about) @@ to_tsquery('simple', $1)",
                &[Param::Text(word.into())],
            )
            .unwrap()
        else {
            panic!("rows");
        };
        let sql: Vec<Vec<SqlValue>> = rows.into_iter().map(|row| row.values).collect();
        let mut sql = keys(&sql);
        sql.sort();
        // Every beach, as SQL does: a budgeted scan until the text index
        // seeds the pattern (M6-D).
        let every = run(
            &db,
            "MATCH (b IS beach) FILTER to_tsvector('simple', b.about) @@ to_tsquery('simple', $1) \
             RETURN b._key AS k ORDER BY k",
            &[Param::Text(word.into())],
        )
        .unwrap();
        assert_eq!(keys(&every), sql, "{word}");
        // Only the beaches near Denpasar, the form in the MATCH's WHERE.
        let near = run(
            &db,
            &format!(
                "MATCH (t IS town WHERE t._key = 'denpasar')-[:near]->(b IS beach) \
                 WHERE to_tsvector('simple', b.about) @@ to_tsquery('simple', '{word}') \
                 RETURN b._key AS k ORDER BY k"
            ),
            &[],
        )
        .unwrap();
        assert_eq!(keys(&near), reached, "{word}");
    }
}

#[test]
fn bm25_scores_the_matching_nodes() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let rows = run(
        &db,
        "MATCH (b IS beach) FILTER to_tsvector('simple', b.about) @@ to_tsquery('simple', 'snorkel') \
         RETURN b._key AS k, bm25(b.about, 'snorkel') AS s ORDER BY s DESC, k",
        &[],
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    let scores: Vec<f64> = rows.iter().map(|row| float(&row[1])).collect();
    assert!(scores.iter().all(|s| s.is_finite() && *s > 0.0), "{scores:?}");
    // The shorter text holds the term at a higher density, so BM25 ranks it
    // first, as its length normalisation says.
    assert_eq!(keys(&rows), ["nusa-dua", "amed"]);
}

#[test]
fn a_text_form_without_a_text_index_is_refused() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        "MATCH (t IS town WHERE to_tsvector('simple', t.name) @@ to_tsquery('simple', 'denpasar')) RETURN t._key AS k",
        "MATCH (t IS town) RETURN bm25(t.name, 'denpasar') AS s",
    ] {
        let text = format!("{}", run(&db, body, &[]).unwrap_err());
        assert!(text.contains("READY text index on town.name"), "{text}");
        assert!(text.contains("CREATE INDEX ON town USING gin"), "{text}");
    }
    // A text form reads a labelled node's field, not any value.
    let text = format!(
        "{}",
        run(&db, "MATCH (b) RETURN bm25(b.about, 'reef') AS s", &[]).unwrap_err()
    );
    assert!(text.contains("no label"), "{text}");
}

#[test]
fn the_spatial_forms_answer_in_metres_and_keep_the_unit_rules() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Within 5 km of Kuta: Kuta itself and Seminyak (about 3.2 km).
    let rows = run(
        &db,
        "MATCH (b IS beach) FILTER ST_DWithin(b.loc, ST_MakePoint(115.1686, -8.7180)::geography, 5000) \
         RETURN b._key AS k, ST_Distance(b.loc, ST_MakePoint(115.1686, -8.7180)::geography) AS m ORDER BY m",
        &[],
    )
    .unwrap();
    assert_eq!(keys(&rows), ["kuta", "seminyak"]);
    assert!(float(&rows[0][1]).abs() < 1e-6, "{rows:?}");
    assert!((3000.0..3400.0).contains(&float(&rows[1][1])), "{rows:?}");
    // The radius as a parameter reads the same.
    let rows = run(
        &db,
        "MATCH (b IS beach) FILTER ST_DWithin(b.loc, ST_MakePoint(115.1686, -8.7180)::geography, $1) \
         RETURN b._key AS k ORDER BY k",
        &[Param::Int(5000)],
    )
    .unwrap();
    assert_eq!(keys(&rows), ["kuta", "seminyak"]);
    // An envelope over the south-east coast, on the flat lon/lat plane.
    let rows = run(
        &db,
        "MATCH (t IS town WHERE t._key = 'denpasar')-[:near]->(b IS beach) \
         WHERE ST_Within(b.loc, ST_MakeEnvelope(115.2, -8.9, 115.3, -8.6, 4326)) \
         RETURN b._key AS k ORDER BY k",
        &[],
    )
    .unwrap();
    assert_eq!(keys(&rows), ["nusa-dua", "sanur"]);
    // The unit rules of QL_CONTRACT §4.4, word for word SQL's.
    for (body, says) in [
        (
            "MATCH (b IS beach) RETURN ST_Distance(b.loc, ST_MakePoint(115.1, -8.7)) AS m",
            "ST_Distance needs geography",
        ),
        (
            "MATCH (b IS beach WHERE ST_DWithin(b.loc, ST_MakePoint(115.1, -8.7), 5000)) RETURN b._key AS k",
            "ST_DWithin needs geography",
        ),
        (
            "MATCH (b IS beach WHERE ST_Within(b.loc, ST_MakeEnvelope(115.2, -8.9, 115.3, -8.6))) RETURN b._key AS k",
            "has no SRID",
        ),
    ] {
        let text = format!("{}", run(&db, body, &[]).unwrap_err());
        assert!(text.contains(says), "`{body}`: {text}");
    }
}

#[test]
fn the_vector_distances_are_pgvectors() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "MATCH (b IS beach WHERE b._key = 'seminyak') \
                RETURN b.emb <-> '[1,0,0]'::vector AS l2, b.emb <=> '[1,0,0]'::vector AS cos, \
                       b.emb <#> $1::vector AS dot";
    let rows = run(&db, body, &[Param::Vector(vec![1.0, 0.0, 0.0])]).unwrap();
    let [l2, cos, dot] = [float(&rows[0][0]), float(&rows[0][1]), float(&rows[0][2])];
    // (0.8, 0.6, 0) against (1, 0, 0): L2 sqrt(0.04 + 0.36), cosine
    // distance 1 - 0.8, and the NEGATIVE inner product, never a similarity.
    assert!((l2 - 0.4f64.sqrt()).abs() < 1e-6, "{l2}");
    assert!((cos - 0.2).abs() < 1e-6, "{cos}");
    assert!((dot + 0.8).abs() < 1e-6, "{dot}");
    // The operators sit with `||`, below `+` as in PostgreSQL: the sum is
    // the distance's operand only when parenthesised.
    let rows = run(
        &db,
        "MATCH (b IS beach WHERE b._key = 'kuta') RETURN 1 - (b.emb <=> '[1,0,0]'::vector) AS similarity",
        &[],
    )
    .unwrap();
    assert!((float(&rows[0][0]) - 1.0).abs() < 1e-6, "{rows:?}");
}

#[test]
fn a_vector_distance_ranks_a_top_k() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let rows = run(
        &db,
        "MATCH (t IS town WHERE t._key = 'denpasar')-[:near]->(b IS beach) \
         RETURN b._key AS k, b.emb <-> $1::vector AS d ORDER BY d LIMIT 2",
        &[Param::Vector(vec![0.0, 1.0, 0.0])],
    )
    .unwrap();
    assert_eq!(keys(&rows), ["sanur", "seminyak"]);
}

#[test]
fn a_vector_of_another_width_is_a_data_error() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (body, params) in [
        ("MATCH (b IS beach) RETURN b.emb <-> '[1,0]'::vector AS d", vec![]),
        ("MATCH (b IS beach) RETURN b.emb <=> $1::vector AS d", vec![Param::Vector(vec![1.0; 4])]),
    ] {
        let error = run(&db, body, &params).unwrap_err();
        assert_eq!(sqlstate(&error), Some("22000"), "`{body}`: {error}");
    }
}

#[test]
fn explain_prints_the_host_forms() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let plan = explain_sql(
        &db,
        "SELECT * FROM GRAPH_TABLE (base \
         MATCH (b IS beach) FILTER to_tsvector('simple', b.about) @@ to_tsquery('simple', 'reef') \
         AND ST_DWithin(b.loc, ST_MakePoint(115.2, -8.7)::geography, 20000) \
         RETURN b._key AS k, bm25(b.about, 'reef') AS s, b.emb <-> '[0,1,0]'::vector AS d)",
        &[],
    )
    .unwrap();
    for part in ["@@ to_tsquery('simple', 'reef')", "bm25(", "ST_DWITHIN(", "<->"] {
        assert!(plan.contains(part), "{part} is not in:\n{plan}");
    }
}

/// The same statement with its seed node's conjuncts written as a budgeted
/// FILTER after the pattern, in a shape no index answers (`= TRUE` keeps
/// exactly the rows the conjunction is true for), so lineage cannot move it.
fn scanned(conjuncts: &str) -> String {
    format!("MATCH (b IS beach) FILTER ({conjuncts}) = TRUE RETURN b._key AS k ORDER BY k")
}

fn seeded(conjuncts: &str) -> String {
    format!("MATCH (b IS beach WHERE {conjuncts}) RETURN b._key AS k ORDER BY k")
}

fn plan(db: &Database, body: &str, params: &[Param]) -> String {
    explain_sql(db, &format!("SELECT * FROM GRAPH_TABLE (base {body})"), params).unwrap()
}

#[test]
fn every_index_answered_conjunct_seeds_the_node() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = "to_tsvector('simple', b.about) @@ to_tsquery('simple', 'reef')";
    let near = "ST_DWithin(b.loc, ST_MakePoint(115.25, -8.72)::geography, 12000)";
    let within = "ST_Within(b.loc, ST_MakeEnvelope(115.1, -8.75, 115.3, -8.6, 4326))";
    for (conjuncts, expected, indexes) in [
        (text.to_owned(), vec!["amed", "nusa-dua", "sanur"], vec!["beach_about"]),
        (near.to_owned(), vec!["kuta", "nusa-dua", "sanur", "seminyak"], vec!["beach_loc"]),
        (within.to_owned(), vec!["kuta", "sanur", "seminyak"], vec!["beach_loc"]),
        (format!("{text} AND {near}"), vec!["nusa-dua", "sanur"], vec!["beach_about", "beach_loc"]),
        (
            format!("{text} AND {near} AND b.rating >= 5"),
            vec!["nusa-dua"],
            vec!["beach_about", "beach_loc", "beach_rating"],
        ),
    ] {
        let seeded_rows = run(&db, &seeded(&conjuncts), &[]).unwrap();
        let scanned_rows = run(&db, &scanned(&conjuncts), &[]).unwrap();
        assert_eq!(keys(&seeded_rows), expected, "{conjuncts}");
        assert_eq!(keys(&seeded_rows), keys(&scanned_rows), "{conjuncts}");
        let explained = plan(&db, &seeded(&conjuncts), &[]);
        for index in &indexes {
            assert!(explained.contains(&format!("`{index}`")), "{index} not in:\n{explained}");
        }
        assert!(!explained.contains("SCAN of"), "{explained}");
        // Every conjunct is answered by the seed, none re-tested after it.
        assert!(!explained.contains("Filter"), "{explained}");
        if indexes.len() > 1 {
            assert!(explained.contains("intersected"), "{explained}");
        }
    }
}

#[test]
fn a_seed_reads_its_parameters_per_execution() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = seeded(
        "to_tsvector('simple', b.about) @@ to_tsquery('simple', $1) \
         AND ST_DWithin(b.loc, ST_MakePoint($2, $3)::geography, $4)",
    );
    let mut prepared = None;
    for (params, expected) in [
        (
            vec![Param::Text("sunset".into()), Param::Float(115.1686), Param::Float(-8.7180), Param::Int(5000)],
            vec!["kuta", "seminyak"],
        ),
        (
            vec![Param::Text("snorkel".into()), Param::Float(115.66), Param::Float(-8.347), Param::Int(1000)],
            vec!["amed"],
        ),
        // A NULL radius: ST_DWithin is NULL for every row, so none is kept.
        (
            vec![Param::Text("reef".into()), Param::Float(115.25), Param::Float(-8.72), Param::Null],
            vec![],
        ),
    ] {
        let prepared = match &mut prepared {
            None => prepared.insert(prepare_sql(&db, &format!("SELECT * FROM GRAPH_TABLE (base {body})"), &params).unwrap()),
            Some(prepared) => {
                prepared.bind(&db, &params).unwrap();
                prepared
            }
        };
        assert!(prepared.rebindable());
        let SqlResult::Rows { rows, .. } = prepared.run(&db).unwrap() else {
            panic!("rows");
        };
        let rows: Vec<Vec<SqlValue>> = rows.into_iter().map(|row| row.values).collect();
        assert_eq!(keys(&rows), expected, "{params:?}");
    }
}

#[test]
fn an_unindexed_spatial_seed_names_a_gist_index() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = format!(
        "{}",
        run(
            &db,
            "MATCH (t IS town WHERE ST_DWithin(t.loc, ST_MakePoint(115.2, -8.6)::geography, 9000)) RETURN t._key AS k",
            &[],
        )
        .unwrap_err()
    );
    assert!(text.contains("CREATE INDEX ON town USING gist (loc)"), "{text}");
    // The budgeted scan is still the way to test it on purpose.
    let rows = run(
        &db,
        "MATCH (t IS town) FILTER ST_DWithin(t.loc, ST_MakePoint(115.2, -8.6)::geography, 9000) RETURN t._key AS k",
        &[],
    )
    .unwrap();
    assert_eq!(keys(&rows), ["denpasar"]);
}

#[test]
fn a_later_conjunct_moves_into_the_seed_by_lineage() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = "to_tsvector('simple', b.about) @@ to_tsquery('simple', 'reef')";
    let expected = vec!["nusa-dua", "sanur"];
    let oracle = keys(&run(&db, &scanned(&format!("{text} AND b.rating >= 4")), &[]).unwrap());
    assert_eq!(oracle, expected);
    for (statement, from) in [
        (
            format!("SELECT * FROM GRAPH_TABLE (base MATCH (b IS beach) FILTER {text} AND b.rating >= 4 RETURN b._key AS k ORDER BY k)"),
            "moved from FILTER by lineage",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (base MATCH (b IS beach) RETURN b AS x, b.rating AS r \
             NEXT FILTER r >= 4 AND to_tsvector('simple', x.about) @@ to_tsquery('simple', 'reef') \
             RETURN x._key AS k ORDER BY k)"
                .to_owned(),
            "moved from FILTER by lineage",
        ),
        (
            "SELECT g.k FROM GRAPH_TABLE (base MATCH (b IS beach) FILTER to_tsvector('simple', b.about) @@ to_tsquery('simple', 'reef') \
             RETURN b._key AS k, b.rating AS r) AS g WHERE g.r >= 4 ORDER BY g.k"
                .to_owned(),
            "moved from the outer WHERE by lineage",
        ),
    ] {
        let SqlResult::Rows { rows, .. } = prepare_sql(&db, &statement, &[]).unwrap().run(&db).unwrap() else {
            panic!("rows");
        };
        let rows: Vec<Vec<SqlValue>> = rows.into_iter().map(|row| row.values).collect();
        assert_eq!(keys(&rows), expected, "{statement}");
        let explained = explain_sql(&db, &statement, &[]).unwrap();
        assert!(explained.contains(from), "{from} not in:\n{explained}");
        assert!(explained.contains("`beach_about`") && explained.contains("`beach_rating`"), "{explained}");
        assert!(!explained.contains("SCAN of"), "{explained}");
    }
}

#[test]
fn nothing_moves_past_a_limit() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The first two beaches by key are amed (3) and kuta (4): neither is
    // rated 5. Moved before the LIMIT, the filter would keep two that are.
    let body = "MATCH (b IS beach) RETURN b AS x ORDER BY x._key LIMIT 2 NEXT FILTER x.rating >= 5 RETURN x._key AS k";
    assert!(run(&db, body, &[]).unwrap().is_empty());
    let explained = plan(&db, body, &[]);
    assert!(!explained.contains("by lineage"), "{explained}");
    assert!(explained.contains("SCAN of"), "{explained}");
}

#[test]
fn nothing_moves_out_of_an_exists_body() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Every beach has a town near it; kept are those rated under 5. Moved out
    // to the outer seed, the body's FILTER would keep only the rated-5
    // beaches and then drop them all.
    let body = "MATCH (b IS beach) WHERE NOT EXISTS { MATCH (b)<-[:near]-(t IS town) FILTER b.rating >= 5 } \
                RETURN b._key AS k ORDER BY k";
    assert_eq!(keys(&run(&db, body, &[]).unwrap()), ["amed", "kuta", "sanur"]);
    assert!(!plan(&db, body, &[]).contains("by lineage"));
}

/// The least `primary_reads` ceiling `sql` completes under, and its rows.
fn least_reads(db: &Database, sql: &str) -> (u64, Vec<Vec<SqlValue>>) {
    let prepared = prepare_sql(db, sql, &[]).unwrap();
    for ceiling in 1..10_000u64 {
        let budget = QueryBudget {
            primary_reads: ceiling,
            ..QueryBudget::unlimited()
        };
        let mut rows = Vec::new();
        let ended = prepared.for_each_row_with(db, 100, budget, &mut || false, &mut |row| {
            rows.push(row.values.clone());
            Ok(())
        });
        if ended.is_ok() {
            return (ceiling, rows);
        }
    }
    panic!("`{sql}` never completed");
}

#[test]
fn a_top_k_by_distance_reads_the_index_order_and_stops_early() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // From (0, 1, 0), Seminyak and Nusa Dua TIE for second under both
    // distances (0.8 squared L2, -0.6 inner product): the tie-break key
    // decides, which the early stop must not cut off, since it stops only
    // at a row strictly worse than the worst it keeps.
    for (op, expected) in [
        ("<->", ["sanur", "nusa-dua"]),
        ("<#>", ["sanur", "nusa-dua"]),
        ("<=>", ["sanur", "nusa-dua"]),
    ] {
        let body = |key: &str| {
            format!(
                "SELECT * FROM GRAPH_TABLE (base MATCH (b IS beach) \
                 RETURN b._key AS k, b.emb {op} '[0,1,0]'::vector AS d ORDER BY {key}, k LIMIT 2)"
            )
        };
        let (ordered, scanned) = (body("d"), body("d * 1"));
        let explained = explain_sql(&db, &ordered, &[]).unwrap();
        assert!(explained.contains("`beach_emb` on beach (ordered by"), "{explained}");
        assert!(explained.contains("stops at the first row past"), "{explained}");
        assert!(!explained.contains("SCAN of"), "{explained}");
        let plain = explain_sql(&db, &scanned, &[]).unwrap();
        assert!(plain.contains("SCAN of") && !plain.contains("stops at the first row past"), "{plain}");
        let (ordered_reads, ordered_rows) = least_reads(&db, &ordered);
        let (scanned_reads, scanned_rows) = least_reads(&db, &scanned);
        assert_eq!(ordered_rows, scanned_rows, "{op}");
        assert_eq!(keys(&ordered_rows), expected, "{op}");
        assert!(ordered_reads < scanned_reads, "{op}: {ordered_reads} reads ordered, {scanned_reads} scanned");
    }
}

#[test]
fn only_an_order_the_index_gives_row_for_row_stops_early() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        // descending: the far end of the index order
        "MATCH (b IS beach) RETURN b._key AS k, b.emb <-> '[0,1,0]'::vector AS d ORDER BY d DESC LIMIT 2",
        // no limit: there is nothing to stop at
        "MATCH (b IS beach) RETURN b._key AS k, b.emb <-> '[0,1,0]'::vector AS d ORDER BY d",
        // a key that is not the node's own
        "MATCH (t IS town)-[:near]->(b IS beach) RETURN b._key AS k, b.emb <-> '[0,1,0]'::vector AS d ORDER BY d LIMIT 2",
    ] {
        let explained = plan(&db, body, &[]);
        assert!(!explained.contains("stops at the first row past"), "{body}:\n{explained}");
        assert!(!explained.contains("(ordered by"), "{body}:\n{explained}");
    }
}

#[test]
fn ef_search_is_read_when_the_execution_opens() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let sql = "SELECT * FROM GRAPH_TABLE (base MATCH (b IS beach) \
               RETURN b._key AS k, b.emb <-> '[0,1,0]'::vector AS d ORDER BY d, k LIMIT 3)";
    let prepared = prepare_sql(&db, sql, &[]).unwrap();
    let answer = |db: &Database| -> Vec<String> {
        let SqlResult::Rows { rows, .. } = prepared.run(db).unwrap() else {
            panic!("rows");
        };
        keys(&rows.into_iter().map(|row| row.values).collect::<Vec<_>>())
    };
    let exact = ["sanur", "nusa-dua", "seminyak"];
    assert_eq!(answer(&db), exact);
    assert!(explain_sql(&db, sql, &[]).unwrap().contains("ordered by (b.emb <-> '[0,1,0]'), exact"));
    // A shortlist of one bounds the whole answer: the SAME plan, opened in
    // a transaction that asked for it, is approximate.
    db.sql("SET LOCAL ef_search = 1", &[]).unwrap();
    assert_eq!(answer(&db), ["sanur"]);
    let explained = explain_sql(&db, sql, &[]).unwrap();
    assert!(explained.contains("APPROXIMATE (ef=1) through `beach_emb_quantized`"), "{explained}");
    // COMMIT ends what LOCAL set: exact again.
    db.sql("COMMIT", &[]).unwrap();
    assert_eq!(answer(&db), exact);
}

#[test]
fn host_form_parameters_are_typed_once() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let sql = "SELECT * FROM GRAPH_TABLE (base MATCH (b IS beach WHERE \
               to_tsvector('simple', b.about) @@ to_tsquery('simple', $1) \
               AND ST_DWithin(b.loc, ST_MakePoint($2, $3)::geography, $4)) \
               RETURN b._key AS k, b.emb <-> $5::vector AS d, b.emb <#> $6 AS e ORDER BY k)";
    let params = |vector: Param| {
        vec![
            Param::Text("reef".into()),
            Param::Float(115.25),
            Param::Float(-8.72),
            Param::Int(12000),
            vector.clone(),
            vector,
        ]
    };
    let mut prepared = prepare_sql(&db, sql, &params(Param::Vector(vec![0.0, 1.0, 0.0]))).unwrap();
    assert_eq!(
        prepared.param_types(),
        [
            Some("TEXT"),
            Some("DOUBLE PRECISION"),
            Some("DOUBLE PRECISION"),
            Some("DOUBLE PRECISION"),
            Some("VECTOR"),
            Some("VECTOR"),
        ]
    );
    let answer = |prepared: &sekejap_lang::PreparedSql| {
        let SqlResult::Rows { rows, .. } = prepared.run(&db).unwrap() else { panic!("rows") };
        rows.into_iter().map(|row| row.values).collect::<Vec<_>>()
    };
    let as_vector = answer(&prepared);
    assert_eq!(keys(&as_vector), ["nusa-dua", "sanur"]);
    // pgvector's text form binds the same vector.
    prepared.bind(&db, &params(Param::Text("[0,1,0]".into()))).unwrap();
    assert_eq!(answer(&prepared), as_vector);
    // A value of another kind is refused naming the parameter.
    let error = prepared.bind(&db, &params(Param::Bool(true))).and_then(|()| prepared.run(&db).map(|_| ()));
    let text = format!("{}", error.unwrap_err());
    assert!(text.contains("$5") && text.contains("VECTOR"), "{text}");
    // One parameter, one type: a tsquery and a radius disagree.
    let error = prepare_sql(
        &db,
        "SELECT * FROM GRAPH_TABLE (base MATCH (b IS beach WHERE \
         to_tsvector('simple', b.about) @@ to_tsquery('simple', $1) \
         AND ST_DWithin(b.loc, ST_MakePoint(115.2, -8.7)::geography, $1)) RETURN b._key AS k)",
        &[Param::Text("reef".into())],
    )
    .err()
    .expect("refused");
    assert_eq!(sqlstate(&error), Some("42P08"), "{error}");
}

#[test]
fn an_ordered_top_k_puts_nan_then_null_last_as_the_scan_does() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    db.sql("CREATE TABLE shell (emb VECTOR(2)) WITH (index: none)", &[]).unwrap();
    for (key, emb) in [("a", Some(vec![1.0, 0.0])), ("b", Some(vec![0.0, 1.0])), ("z", Some(vec![0.0, 0.0])), ("c", None)] {
        match emb {
            Some(emb) => db.sql("INSERT INTO shell (_key, emb) VALUES ($1, $2)", &[Param::Text(key.into()), Param::Vector(emb)]),
            None => db.sql("INSERT INTO shell (_key) VALUES ($1)", &[Param::Text(key.into())]),
        }
        .unwrap();
    }
    db.sql("COMMIT", &[]).unwrap();
    db.sql("CREATE INDEX shell_emb ON shell USING exact (emb)", &[]).unwrap();
    for (limit, expected) in [(3, vec!["a", "b", "z"]), (4, vec!["a", "b", "z", "c"])] {
        let body = |key: &str| {
            format!("MATCH (s IS shell) RETURN s._key AS k, s.emb <=> '[1,0]'::vector AS d ORDER BY {key}, k LIMIT {limit}")
        };
        let (ordered, scanned) = (body("d"), body("d * 1"));
        assert!(plan(&db, &ordered, &[]).contains("stops at the first row past"));
        let rows = run(&db, &ordered, &[]).unwrap();
        assert_eq!(keys(&rows), expected);
        assert_eq!(keys(&rows), keys(&run(&db, &scanned, &[]).unwrap()));
        assert!(float(&rows[2][1]).is_nan(), "the zero vector's cosine distance is NaN");
        if limit == 4 {
            assert_eq!(rows[3][1], SqlValue::Null, "no vector, no distance");
        }
    }
}

#[test]
fn an_approximate_only_column_is_exact_unless_ef_search_is_set() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    db.sql("CREATE TABLE tide (emb VECTOR(2)) WITH (index: none)", &[]).unwrap();
    for (n, emb) in [[1.0, 0.0], [0.9, 0.1], [0.5, 0.5], [0.0, 1.0]].into_iter().enumerate() {
        db.sql(
            "INSERT INTO tide (_key, emb) VALUES ($1, $2)",
            &[Param::Text(format!("t{n}")), Param::Vector(emb.to_vec())],
        )
        .unwrap();
    }
    db.sql("COMMIT", &[]).unwrap();
    db.sql("CREATE INDEX tide_emb_q ON tide USING quantized (emb)", &[]).unwrap();
    let sql = "SELECT * FROM GRAPH_TABLE (base MATCH (t IS tide) \
               RETURN t._key AS k, t.emb <-> '[1,0]'::vector AS d ORDER BY d, k LIMIT 3)";
    let prepared = prepare_sql(&db, sql, &[]).unwrap();
    let answer = |db: &Database| -> Vec<String> {
        let SqlResult::Rows { rows, .. } = prepared.run(db).unwrap() else { panic!("rows") };
        keys(&rows.into_iter().map(|row| row.values).collect::<Vec<_>>())
    };
    let exact = ["t0", "t1", "t2"];
    assert_eq!(answer(&db), exact, "no knob: exact, every row sorted");
    let plain = explain_sql(&db, sql, &[]).unwrap();
    assert!(plain.contains("the column has no exact index, so the rows are read unordered and sorted whole"), "{plain}");
    assert!(plain.contains("when SET LOCAL ef_search is set, fed in the seed's index order"), "{plain}");
    db.sql("SET LOCAL ef_search = 1", &[]).unwrap();
    assert_eq!(answer(&db), ["t0"], "under the knob: the one-row shortlist of the quantized index");
    let asked = explain_sql(&db, sql, &[]).unwrap();
    assert!(asked.contains("APPROXIMATE (ef=1) through `tide_emb_q`"), "{asked}");
    db.sql("COMMIT", &[]).unwrap();
    assert_eq!(answer(&db), exact, "after the transaction: exact again");
}
