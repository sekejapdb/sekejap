//! ONE property, and only one: at a small scale the two arms of `two_ways`
//! must answer the SAME question. A benchmark whose arms quietly answer
//! different questions is not slow or fast, it is fiction — so this asserts
//! the disagreement set, and that every case family reached the report (a
//! family that silently vanished would take its disagreements with it).
//!
//! THE DISAGREEMENT SET IS EMPTY, and keeping it empty is the whole point.
//! It was not empty when the port landed: six text cases disagreed because of
//! one engine defect, now fixed.
//!
//!   `collections::query::text_score` (src/query.rs) read a document's text
//!   norm from the HEAD row only — `store().get(norm_key(index, sequence))` —
//!   and gave up when it was absent. A late, clean-slate text build takes the
//!   packed path, which writes norms ONLY as segment blocks
//!   (`segments::norm_block_key`, src/text_indexes.rs) and never writes a head
//!   row. `text_indexes::read_norm_cached` knows this and probes head row then
//!   packed block; `text_score` did not. So after a packed text build every
//!   BM25 ranking dropped every candidate, every text filter that was NOT the
//!   candidate driver matched nothing, and every phrase match (which always
//!   refines through `text_score`) returned nothing. A text filter that DRIVES
//!   with Any/All still worked, because the cursor marks it satisfied and
//!   `text_score` is never called — which is why `text/match_all` agreed while
//!   `text/bm25_one_term` did not. `text_score` now uses `read_norm_cached`.
//!
//! Both tests below are active. One asserts the set is empty; the other
//! asserts it is exactly the (now empty) known-defect list, so a NEW
//! disagreement names itself instead of merely failing a count.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// Every family the case list declares. `aggregate` is expected to be present
/// with zero runnable cases — E4 has no aggregate API — and the test asserts
/// that it is REPORTED rather than dropped.
const FAMILIES: [&str; 10] = [
    "filter",
    "project",
    "order",
    "limit",
    "aggregate",
    "text",
    "graph",
    "win",
    "mixed",
    "scan",
];

/// Families that must actually have run at least one case.
const MUST_RUN: [&str; 9] = [
    "filter", "project", "order", "limit", "text", "graph", "win", "mixed", "scan",
];

/// The cases that disagree today. The packed-norm defect that filled this list
/// is fixed, so it is empty — and must stay empty.
const KNOWN_DISAGREEMENTS: [&str; 0] = [];

struct Run {
    stdout: String,
    disagreeing: BTreeSet<String>,
    ran: BTreeMap<String, usize>,
    listed: BTreeMap<String, usize>,
}

fn two_ways(rows: &str) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_two_ways"))
        .arg(rows)
        .output()
        .expect("two_ways runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "two_ways exited {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code()
    );

    let mut disagreeing = BTreeSet::new();
    let mut ran = BTreeMap::new();
    let mut listed = BTreeMap::new();
    for line in stdout.lines() {
        if line.contains("DISAGREE") {
            let mut columns = line.split_whitespace();
            let (Some(family), Some(name)) = (columns.next(), columns.next()) else {
                panic!("malformed case line: {line}");
            };
            assert!(
                FAMILIES.contains(&family),
                "a line outside the case table mentions DISAGREE: {line}"
            );
            disagreeing.insert(name.to_owned());
        }
        // coverage <family> ran=<n> unsupported=<m>
        let Some(rest) = line.strip_prefix("coverage ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let (Some(family), Some(ran_field), Some(unsupported_field)) =
            (parts.next(), parts.next(), parts.next())
        else {
            panic!("malformed coverage line: {line}");
        };
        assert!(
            FAMILIES.contains(&family),
            "unknown family {family} in: {line}"
        );
        let count = |field: &str, prefix: &str| {
            field
                .strip_prefix(prefix)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_else(|| panic!("malformed coverage line: {line}"))
        };
        let ran_count = count(ran_field, "ran=");
        let unsupported_count = count(unsupported_field, "unsupported=");
        ran.insert(family.to_owned(), ran_count);
        listed.insert(family.to_owned(), ran_count + unsupported_count);
    }
    Run {
        stdout,
        disagreeing,
        ran,
        listed,
    }
}

fn assert_every_family_reported(run: &Run) {
    for family in FAMILIES {
        let total = run
            .listed
            .get(family)
            .copied()
            .unwrap_or_else(|| panic!("family {family} never reached the report\n{}", run.stdout));
        assert!(total > 0, "family {family} reported no cases at all");
    }
    for family in MUST_RUN {
        assert!(
            run.ran.get(family).copied().unwrap_or(0) > 0,
            "family {family} ran no case; both arms must answer it at least once\n{}",
            run.stdout
        );
    }
}

#[test]
fn the_two_arms_disagree_only_where_the_engine_is_known_to_be_wrong() {
    let run = two_ways("2000");
    let expected: BTreeSet<String> = KNOWN_DISAGREEMENTS.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        expected,
        run.disagreeing,
        "the disagreement set moved. A NEW name means the two arms stopped asking the same \
         question — the benchmark is comparing answers to different questions and its numbers \
         mean nothing until that is explained.\n{}",
        run.stdout
            .lines()
            .filter(|line| line.contains("DISAGREE"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_every_family_reported(&run);
}

/// The property in its plainest form: no case, anywhere, answers differently
/// in the two arms.
#[test]
fn two_arms_agree_everywhere() {
    let run = two_ways("2000");
    assert!(
        run.disagreeing.is_empty(),
        "the two arms answered different questions: {:?}",
        run.disagreeing
    );
    assert_every_family_reported(&run);
}
