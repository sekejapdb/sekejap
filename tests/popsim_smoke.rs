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
