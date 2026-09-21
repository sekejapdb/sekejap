//! The two popsim arms must answer the same questions.
//!
//! A two-arm benchmark is only a measurement while both arms return the same
//! rows; the moment they do not, a ratio is a comparison of two different
//! questions. This runs both arms in-process at 5,000 rows — small enough for
//! a test, large enough that every case in the battery has a non-empty answer
//! on at least one of the two spatial radii — and requires every case to agree
//! on its row count. It also requires each arm to report bytes on disk, which
//! is the number the 48M comparison is really about.
//!
//! The binary is included by path rather than duplicated, so the thing tested
//! is the thing the cluster runs.

#[allow(dead_code)]
#[path = "../src/bin/popsim.rs"]
mod popsim;

use popsim::{run_arm, Arm, Options};
use std::{fs, time::Duration};

const ROWS: u64 = 5_000;

#[test]
fn both_arms_answer_the_same_questions() {
    let root = std::env::temp_dir().join(format!("popsim-smoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let arm = |which| {
        let mut options = Options::new(which, ROWS, &root);
        // Three samples is enough to reject a case that cannot run; the
        // battery's numbers are not the point of this test.
        options.reps = 3;
        options.case_budget = Duration::from_secs(30);
        run_arm(&options).expect("arm runs")
    };

    let e4 = arm(Arm::E4);
    let lite = arm(Arm::Sqlite);

    let counts = |report: &serde_json::Value| {
        report["queries"]
            .as_array()
            .expect("queries is an array")
            .iter()
            .map(|q| {
                (
                    q["name"].as_str().expect("name").to_string(),
                    q["rows"].as_u64().expect("rows"),
                )
            })
            .collect::<Vec<_>>()
    };

    let e4_counts = counts(&e4);
    let lite_counts = counts(&lite);
    assert_eq!(
        e4_counts.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        lite_counts.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "the two arms must run the same battery in the same order"
    );
    let names: Vec<_> = e4_counts.iter().map(|(n, _)| n.as_str()).collect();
    for case in [
        "plot_within_box",
        "plot_intersects_radius_poly",
        "plot_contains_point",
        "plot_dwithin_2km",
    ] {
        assert!(
            names.contains(&case),
            "the battery lost the geometry case {case}: {names:?}"
        );
    }
    assert!(
        e4_counts.len() >= 15,
        "the battery lost cases: {}",
        e4_counts.len()
    );
    for ((name, e4_rows), (_, lite_rows)) in e4_counts.iter().zip(&lite_counts) {
        assert_eq!(
            e4_rows, lite_rows,
            "{name}: E4 returned {e4_rows} rows, SQLite returned {lite_rows}"
        );
    }

    // Agreeing counts are not enough for the cases whose order is DEFINED on
    // both sides: `count_all` is key order in both arms, and the two ordered
    // limits share a tie-break. These also pin the key-to-sequence mapping the
    // E4 arm translates every answer through. (`name_top10` is deliberately
    // not here: the two engines rank by different BM25 formulas.)
    let keys_of = |report: &serde_json::Value, case: &str| {
        report["queries"]
            .as_array()
            .expect("queries is an array")
            .iter()
            .find(|q| q["name"] == case)
            .map(|q| q["keys"].clone())
            .unwrap_or_else(|| panic!("case {case} ran"))
    };
    for case in ["point_lookup", "count_all", "oldest_10", "youngest_10"] {
        assert_eq!(
            keys_of(&e4, case),
            keys_of(&lite, case),
            "{case}: the two arms returned different keys"
        );
    }

    // Every row must be reachable in both arms, or an agreement on the
    // selective cases would prove nothing.
    let rows_of = |report: &serde_json::Value, case: &str| {
        report["queries"]
            .as_array()
            .expect("queries is an array")
            .iter()
            .find(|q| q["name"] == case)
            .and_then(|q| q["rows"].as_u64())
            .unwrap_or_else(|| panic!("case {case} ran"))
    };
    assert_eq!(rows_of(&e4, "count_all"), ROWS, "E4 must hold every row");
    assert_eq!(rows_of(&lite, "count_all"), ROWS, "SQLite must hold every row");
    assert!(
        rows_of(&e4, "radius_50km") > 0,
        "the 50 km radius must find somebody, or the spatial case proves nothing"
    );

    for report in [&e4, &lite] {
        let bytes = report["bytes_on_disk"].as_u64().expect("bytes_on_disk");
        assert!(
            bytes > 0,
            "{} reported no bytes on disk",
            report["arm"].as_str().unwrap_or("?")
        );
        assert!(
            report["bytes_per_row"].as_f64().expect("bytes_per_row") > 0.0,
            "bytes per row must be reported"
        );
        assert!(
            report["stages"]["load_s"].as_f64().expect("load_s") > 0.0,
            "the load stage must be timed"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// `--reuse` opens the database an earlier pass built and runs only the
/// queries. It must answer every case with the same rows AND the same keys as
/// the pass that built the file (the reopen found the same collection and
/// indexes by name), and it must not report a load or build it did not do.
#[test]
fn a_reuse_pass_answers_exactly_what_the_build_pass_answered() {
    let root = std::env::temp_dir().join(format!("popsim-reuse-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let dsn = std::env::var("POPSIM_PG_DSN")
        .unwrap_or_else(|_| "postgres://127.0.0.1:55432/postgres".to_string());
    let pg_reachable = postgres::Client::connect(&dsn, postgres::NoTls).is_ok();
    let pass = |which, reuse| {
        let mut options = Options::new(which, ROWS, &root);
        options.reps = 2;
        options.case_budget = Duration::from_secs(30);
        options.reuse = reuse;
        options.dsn = Some(dsn.clone());
        run_arm(&options).expect("arm runs")
    };
    let answers = |report: &serde_json::Value| {
        report["queries"]
            .as_array()
            .expect("queries")
            .iter()
            .map(|q| (q["name"].clone(), q["rows"].clone(), q["keys"].clone()))
            .collect::<Vec<_>>()
    };
    // The Postgres arm reopens the database its build pass named in the
    // report; it joins the loop only where a server answers.
    let mut arms = vec![Arm::E4, Arm::Sqlite];
    if pg_reachable {
        arms.push(Arm::Postgres);
    } else {
        eprintln!("SKIP the postgres reuse pass: no server reachable at {dsn}");
    }
    for which in arms {
        let built = pass(which, false);
        let reused = pass(which, true);
        assert_eq!(answers(&built), answers(&reused), "{which:?}: the reuse pass disagreed");
        assert_eq!(reused["reused"], serde_json::Value::Bool(true));
        assert_eq!(built["reused"], serde_json::Value::Bool(false));
        assert!(reused["stages"]["load_s"].is_null(), "{which:?}: a reuse pass reported a load");
        assert!(reused["stages"]["index_total_s"].is_null());
        assert!(reused["stages"]["open_s"].as_f64().is_some());
        assert!(built["stages"]["load_s"].as_f64().is_some());
    }
    // And a reuse pass over nothing is an error, never a silent rebuild.
    let _ = fs::remove_dir_all(&root);
    let mut options = Options::new(Arm::E4, ROWS, &root);
    options.reuse = true;
    assert!(run_arm(&options).is_err());
    let _ = fs::remove_dir_all(&root);
}

/// Same agreement, E4 against the Postgres/PostGIS arm — but this arm needs a
/// real server, which is not guaranteed to be running wherever this test
/// suite executes. The test SKIPS (prints a note, returns `Ok`) rather than
/// failing when nothing answers on the DSN, so `cargo test` stays green on a
/// machine with no Postgres and still exercises the third arm wherever one is
/// reachable (`POPSIM_PG_DSN` overrides the default local dev DSN).
#[test]
fn postgres_arm_answers_the_same_questions_as_e4() -> Result<(), Box<dyn std::error::Error>> {
    let dsn = std::env::var("POPSIM_PG_DSN")
        .unwrap_or_else(|_| "postgres://127.0.0.1:55432/postgres".to_string());
    if postgres::Client::connect(&dsn, postgres::NoTls).is_err() {
        eprintln!(
            "SKIP postgres_arm_answers_the_same_questions_as_e4: no server reachable at {dsn}"
        );
        return Ok(());
    }

    let root = std::env::temp_dir().join(format!("popsim-smoke-pg-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let arm = |which| {
        let mut options = Options::new(which, ROWS, &root);
        options.reps = 3;
        options.case_budget = Duration::from_secs(30);
        options.dsn = Some(dsn.clone());
        run_arm(&options).expect("arm runs")
    };

    let e4 = arm(Arm::E4);
    let pg = arm(Arm::Postgres);

    let counts = |report: &serde_json::Value| {
        report["queries"]
            .as_array()
            .expect("queries is an array")
            .iter()
            .map(|q| {
                (
                    q["name"].as_str().expect("name").to_string(),
                    q["rows"].as_u64().expect("rows"),
                )
            })
            .collect::<Vec<_>>()
    };

    let e4_counts = counts(&e4);
    let pg_counts = counts(&pg);
    assert_eq!(
        e4_counts.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        pg_counts.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "the two arms must run the same battery in the same order"
    );
    for ((name, e4_rows), (_, pg_rows)) in e4_counts.iter().zip(&pg_counts) {
        assert_eq!(
            e4_rows, pg_rows,
            "{name}: E4 returned {e4_rows} rows, Postgres returned {pg_rows}"
        );
    }

    let keys_of = |report: &serde_json::Value, case: &str| {
        report["queries"]
            .as_array()
            .expect("queries is an array")
            .iter()
            .find(|q| q["name"] == case)
            .map(|q| q["keys"].clone())
            .unwrap_or_else(|| panic!("case {case} ran"))
    };
    for case in ["point_lookup", "count_all", "oldest_10", "youngest_10"] {
        assert_eq!(
            keys_of(&e4, case),
            keys_of(&pg, case),
            "{case}: the two arms returned different keys"
        );
    }

    let bytes = pg["bytes_on_disk"].as_u64().expect("bytes_on_disk");
    assert!(bytes > 0, "postgres reported no bytes on disk");

    let _ = fs::remove_dir_all(&root);
    Ok(())
}
