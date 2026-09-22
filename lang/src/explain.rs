//! `EXPLAIN`: the plan the engine built, and the work the run charged.
//!
//! It always RUNS. A plan printed without running says what the driver would
//! be and nothing about the membership sets, which are built by the first
//! page, or about the counters, which are the point. `docs/lang/QL_CONTRACT.md` §6
//! asks EXPLAIN to print which predicates are answered index-side and to
//! label a construct whose definition is a scan; both are below.

use super::compile::{AggregatePlan, Rebind, SelectPlan};
use super::{RunWork, SqlResult2, PAGE};
use sekejap_core::collections::{
    AggregatePlanDescription, Database, FilterAnswer, QueryBudget, QueryPlanDescription,
};

fn answer_text(answer: &FilterAnswer) -> String {
    match answer {
        FilterAnswer::Driver => "index posting: the driving walk certifies it".into(),
        FilterAnswer::MembershipSet => "membership set built from postings".into(),
        FilterAnswer::MembershipUnbuilt => {
            "membership set, not walked yet (no page has run)".into()
        }
        FilterAnswer::MembershipOverflow => {
            "row: the membership walk exceeded its budget and every candidate reads the row".into()
        }
        FilterAnswer::IndexPosting => "index posting, probed per candidate".into(),
        FilterAnswer::CarriedKey => "index posting: the candidate carries the key".into(),
        FilterAnswer::Row => "row".into(),
        FilterAnswer::Folded(into) => {
            format!("nothing: folded into position {into}, which answers both")
        }
        FilterAnswer::GraphFrontier => "traversal frontier, materialised once".into(),
    }
}

/// Run `select` and render its plan and its counters.
pub(super) fn render(
    db: &Database,
    select: &SelectPlan,
    notices: &[String],
    rebind: &Rebind,
) -> SqlResult2<String> {
    let mut work = RunWork::default();
    let (plan, approximation) = select.with_query(db, &mut |prepared| {
        let mut approximation = None;
        loop {
            let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
            work.add(&page);
            if page.approximation.is_some() {
                approximation = page.approximation;
            }
            if page.done || page.rows.is_empty() {
                break;
            }
        }
        Ok((prepared.describe(), approximation))
    })?;
    let mut out = format(db, &plan, &work, approximation, notices);
    out.push_str(&rewrites_and_row_functions(select));
    out.push_str(&rebind_line(rebind));
    Ok(out)
}

/// Whether this compiled statement can be RE-BOUND with new parameters
/// without being compiled again, and when it cannot, what folded a value at
/// prepare. `QL_CONTRACT` §2, the bounded prepared-plan cache: a cache HIT is
/// a rebind, so which of the two a statement is decides what a second
/// execution of it costs.
fn rebind_line(rebind: &Rebind) -> String {
    match rebind.reason() {
        None => "rebind: yes -- every $n is a typed slot, so new parameters are written into this same plan\n".to_owned(),
        Some(reason) => format!(
            "rebind: no -- {reason}; a new parameter list is COMPILED again from the parsed statement (never re-parsed)\n"
        ),
    }
}

/// The two sections `docs/lang/QL_CONTRACT.md` §4.1 and §4.2 ask EXPLAIN for.
///
/// They answer different questions and the contract keeps them apart: a RANGE
/// REWRITE is index-side, folded at prepare, and costs the candidates its
/// range admits; a ROW FUNCTION is evaluated over values a returned row
/// already carries, and costs one evaluation per row RETURNED. A reader who
/// wants to know why a statement is cheap or dear reads which list its
/// functions are in.
fn rewrites_and_row_functions(select: &SelectPlan) -> String {
    let mut out = String::new();
    out.push_str("range rewrites: ");
    if select.rewrites.is_empty() {
        out.push_str("none\n");
    } else {
        out.push_str("\n");
        for line in &select.rewrites {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out.push_str("row functions: ");
    if select.row_functions.is_empty() {
        out.push_str("none\n");
    } else {
        out.push_str("\n");
        for line in &select.row_functions {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

/// Run `aggregate` and render its plan, its shape and its counters.
///
/// It runs, for the reason the row EXPLAIN runs: the membership sets and the
/// groups seen are facts about a walk that happened, not about a plan.
pub(super) fn render_aggregate(
    db: &Database,
    aggregate: &AggregatePlan,
    notices: &[String],
    rebind: &Rebind,
) -> SqlResult2<String> {
    let mut work = RunWork::default();
    let plan = aggregate.with_aggregate(db, &mut |prepared| {
        loop {
            let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
            work.add_groups(&page);
            if page.done || page.groups.is_empty() {
                break;
            }
        }
        Ok(prepared.describe())
    })?;
    let mut out = format_aggregate(&plan);
    out.push_str(&format(db, &plan.query, &work, None, notices));
    out.push_str(&rebind_line(rebind));
    Ok(out)
}

/// The aggregate's own half of the plan: the shape, the group key, where each
/// accumulator reads its input, and how many groups the walk opened.
fn format_aggregate(plan: &AggregatePlanDescription) -> String {
    let mut out = String::new();
    out.push_str(&format!("shape: {}\n", plan.shape.written()));
    out.push_str(match plan.shape {
        sekejap_core::collections::AggregateShape::Streaming => "  note:  the group key IS the driving index's own value, so the groups arrive contiguous: ONE accumulator set is alive, the key comes off the posting with no row read, and a page stops and resumes at a group boundary\n",
        sekejap_core::collections::AggregateShape::Hashed => "  note:  the group key is not the driving walk's own order, so every group is open at once and none is final until the walk ends; the memory that costs is bounded by the `groups` budget below, and there is no spill\n",
        sekejap_core::collections::AggregateShape::Skip => "  note:  the group has NO accumulators, so nothing but the EXISTENCE of each value matters: the walk seeks to the successor of the current value's key prefix, one descent per DISTINCT VALUE, and reads neither the postings between them nor any row\n",
        sekejap_core::collections::AggregateShape::PostingJoin => "  note:  the group key is the driving index's own value and every other accumulator has its own numeric scalar index, so NO ROW IS READ: two index passes, the first turning each value's run of postings into a group with a bounded id bitmap, the second folding each (value, id) into the group whose bitmap claims the id\n",
    });
    if !plan.passes.is_empty() {
        out.push_str("passes:\n");
        for pass in &plan.passes {
            out.push_str(&format!("  {pass}\n"));
        }
    }
    if let Some(reason) = &plan.fell_back {
        out.push_str(&format!("fell back: {reason}\n"));
    }
    // A whole-collection `count(*)` is the one aggregate whose answer can be
    // READ instead of walked. Every other aggregate prints no `count:` line,
    // because for them there is nothing to choose between.
    if let Some(count) = plan.count {
        out.push_str(&format!("count: {}\n", count.written()));
        out.push_str(match count {
            sekejap_core::collections::CountSource::LiveRecord => "  note:  the collection carries a LIVE ROW COUNT record, maintained by the write path inside the same transaction as the rows, so the answer is ONE get: no candidate is walked, no posting is read and no row is touched\n",
            sekejap_core::collections::CountSource::Walk => "  note:  this collection has no live row-count record -- a database written before the feature, or one the backfill has not reached -- so the count is the complete enumeration of the external-key mapping keyspace, one step per row\n",
        });
    }
    match &plan.group {
        None => out.push_str("group: none -- one group over every candidate\n"),
        Some((key, source)) => out.push_str(&format!("group: {key} -> {source}\n")),
    }
    out.push_str("accumulators:\n");
    if plan.accumulators.is_empty() {
        out.push_str("  none -- a group with no accumulators is DISTINCT\n");
    }
    for (accumulator, source) in &plan.accumulators {
        out.push_str(&format!("  {accumulator} -> {source}\n"));
    }
    if plan.having.is_empty() {
        out.push_str("having: none\n");
    } else {
        out.push_str("having:\n");
        for predicate in &plan.having {
            out.push_str(&format!("  {predicate} (applied to a finished group, before paging)\n"));
        }
    }
    out.push_str(&format!(
        "groups: {} seen, cap {} accumulator set(s) held at once\n",
        plan.groups_seen, plan.groups_cap
    ));
    out.push_str(&format!(
        "group limit: {}\n",
        plan.total_limit
            .map_or_else(|| "none".to_owned(), |n| n.to_string())
    ));
    out
}

/// The driver-and-filters half of a plan, for the two predicated WRITES.
///
/// The same lines `format` prints for a SELECT, minus the counters: a write
/// EXPLAIN does not run, so there are no counters to print and the
/// membership sets read "not walked yet". Shared so the two cannot drift.
pub(crate) fn format_write_plan(plan: &QueryPlanDescription) -> String {
    let mut out = String::new();
    out.push_str(&format!("driver: {:?}\n", plan.driver));
    out.push_str(&format!("  walks: {}\n", plan.driver_detail));
    if plan.driver_is_a_scan {
        out.push_str(
            "  note:  this driver is a SCAN by definition (QL_CONTRACT §6): its work is proportional to the collection, not to the candidates a predicate admits\n",
        );
    }
    if plan.filters.is_empty() {
        out.push_str("filters: none\n");
    } else {
        out.push_str("filters:\n");
        for filter in &plan.filters {
            out.push_str(&format!(
                "  [{}] {} {} -> {}\n",
                filter.position,
                filter.family,
                filter.detail,
                answer_text(&filter.answer)
            ));
        }
    }
    out
}

fn format(
    db: &Database,
    plan: &QueryPlanDescription,
    work: &RunWork,
    approximation: Option<sekejap_core::collections::ApproximationDiagnostics>,
    notices: &[String],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("driver: {:?}\n", plan.driver));
    out.push_str(&format!("  walks: {}\n", plan.driver_detail));
    if plan.driver_is_a_scan {
        out.push_str(
            "  note:  this driver is a SCAN by definition (QL_CONTRACT §6): its work is proportional to the collection, not to the candidates a predicate admits\n",
        );
    }
    if plan.filters.is_empty() {
        out.push_str("filters: none\n");
    } else {
        out.push_str("filters:\n");
        for filter in &plan.filters {
            out.push_str(&format!(
                "  [{}] {}{} {} -> {}\n",
                filter.position,
                filter.family,
                match (&filter.index, &filter.field) {
                    (Some(index), Some(field)) => format!(" {index} ({field})"),
                    (Some(index), None) => format!(" {index}"),
                    _ => String::new(),
                },
                filter.detail,
                answer_text(&filter.answer),
            ));
        }
    }
    out.push_str(&format!(
        "order: {} -- {}{}\n",
        plan.order_kind,
        plan.order_detail,
        if plan.order_reads_row {
            " (the ranking reads the row)"
        } else {
            ""
        }
    ));
    if !plan.score_leaves.is_empty() {
        out.push_str("score leaves:\n");
        for leaf in &plan.score_leaves {
            out.push_str(&format!("  {leaf}\n"));
        }
    }
    out.push_str(&format!(
        "limit: {}\n",
        plan.total_limit
            .map_or_else(|| "none".to_owned(), |n| n.to_string())
    ));
    out.push_str(&format!(
        "projection: {}\n",
        if plan.projection.is_empty() {
            "ids only (no row is read for the answer itself)".to_owned()
        } else {
            plan.projection.join(", ")
        }
    ));
    if let Some(diagnostics) = approximation {
        out.push_str(&format!(
            "approximation: {:?}, ef={}, examined={}, reranked={}\n",
            diagnostics.method, diagnostics.ef, diagnostics.examined, diagnostics.reranked
        ));
    }
    let w = &work.work;
    out.push_str(&format!("rows: {} in {} page(s)\n", work.rows, work.pages));
    out.push_str(&format!(
        "work: candidates={} primary_reads={} row_decodes={} scalar_postings={} \
         spatial_postings={} text_postings={} text_tokens={} graph_edges={} graph_visited={} \
         vector_locators={} vector_sidecars={} vector_lanes={} key_postings={} groups={} \
         output_bytes={}\n",
        w.candidates,
        w.primary_reads,
        w.row_decodes,
        w.scalar_postings,
        w.spatial_postings,
        w.text_postings,
        w.text_tokens,
        w.graph_edges,
        w.graph_visited,
        w.vector_locators,
        w.vector_sidecars,
        w.vector_lanes,
        w.key_postings,
        w.groups,
        w.output_bytes,
    ));
    if let Ok(accesses) = db.pool_accesses() {
        out.push_str(&format!("pool accesses (process total): {accesses}\n"));
    }
    for notice in notices {
        out.push_str(&format!("notice: {notice}\n"));
    }
    out
}
