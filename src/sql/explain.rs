//! `EXPLAIN`: the plan the engine built, and the work the run charged.
//!
//! It always RUNS. A plan printed without running says what the driver would
//! be and nothing about the membership sets, which are built by the first
//! page, or about the counters, which are the point. `docs/QL_CONTRACT.md` §6
//! asks EXPLAIN to print which predicates are answered index-side and to
//! label a construct whose definition is a scan; both are below.

use super::compile::SelectPlan;
use super::{RunWork, SqlResult2, PAGE};
use crate::collections::{Database, FilterAnswer, QueryBudget, QueryPlanDescription};

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
    Ok(format(db, &plan, &work, approximation, notices))
}

fn format(
    db: &Database,
    plan: &QueryPlanDescription,
    work: &RunWork,
    approximation: Option<crate::collections::ApproximationDiagnostics>,
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
         vector_locators={} vector_sidecars={} vector_lanes={} key_postings={} output_bytes={}\n",
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
