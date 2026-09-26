//! One execution of a GQL plan, handed out in pages
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §3.5).
//!
//! [`GqlCursor`] owns the instantiated operator tree and borrows the
//! database. That borrow is what makes every page of one answer read ONE
//! snapshot: a writer needs `&mut Database`, and a service reader holds its
//! own snapshot handle.
//!
//! * **Work per page.** Each [`GqlCursor::next_page`] runs under the budget
//!   handed to it, and reports what it spent. The pattern operators hold
//!   nothing across pages but their current input row and one bounded
//!   refill. A blocking operator (aggregate, distinct, sort, a `FOR` list)
//!   holds state across pages: it charges what it still holds again to each
//!   page it runs in, so every page's memory report, and its memory
//!   ceiling, cover what the execution holds.
//! * **Cancellation and the deadline** are checked on every charge, through
//!   the page's meter.
//! * **Poison on error.** A GQL page refused part way through cannot be
//!   rolled back cheaply -- an operator may have consumed input it has not
//!   emitted -- so, unlike a plain query's page, a refused, cancelled or
//!   failed page POISONS the cursor: every later page returns the same
//!   error. The rows already returned are an INCOMPLETE answer, and no page
//!   says `done` after a refusal.
//! * **No resume across executions.** A caller that wants the next page
//!   keeps the cursor alive; there is no continuation token (design Q7).

use super::super::{invalid_query, QueryError, QueryResult, MAX_PAGE_SIZE};
use super::budget::{GqlBudget, GqlMeter, GqlWork};
use super::host::{ExecMeter, GqlHost};
use super::ops::{build, ExecCx, Op};
use super::plan::OpSpec;
use super::value::{BindingRow, BindingValue};
use crate::collections::{Database, Error};

/// One execution of a GQL plan over one database or snapshot.
pub struct GqlCursor<'q> {
    db: &'q Database,
    host: &'q dyn GqlHost,
    params: Vec<BindingValue>,
    root: Op<'q>,
    state: State,
    /// Pages handed out so far, including the one being filled.
    pages: u64,
}

enum State {
    Open,
    Done,
    /// The error that stopped the cursor, repeated to every later page.
    Poisoned(QueryError),
}

/// One page of an execution's rows.
#[derive(Debug)]
pub struct GqlPage {
    pub rows: Vec<BindingRow>,
    /// True when the answer is complete: no row follows. Never set after a
    /// refusal.
    pub done: bool,
    /// What this page spent.
    pub work: GqlWork,
}

impl<'q> GqlCursor<'q> {
    /// An execution of `plan` over `db`, evaluating expressions through
    /// `host` with parameter values `params` (converted and type-checked by
    /// the host). Reads nothing: the first page does the work.
    ///
    /// A plan whose slots do not fit its rows, whose edge predicate has no
    /// edge slot, which names a label or edge type twice, or whose `OFFSET`
    /// or `LIMIT` is not an integer in `0..=i64::MAX` is refused here.
    pub fn open(
        db: &'q Database,
        host: &'q dyn GqlHost,
        plan: &'q OpSpec,
        params: Vec<BindingValue>,
    ) -> QueryResult<Self> {
        let (root, _) = build(plan, &params)?;
        Ok(Self {
            db,
            host,
            params,
            root,
            state: State::Open,
            pages: 0,
        })
    }

    /// Up to `page_rows` (1..=8192) more rows, under `budget`, stopping when
    /// `cancelled` says so.
    pub fn next_page<C: FnMut() -> bool>(
        &mut self,
        page_rows: usize,
        budget: GqlBudget,
        mut cancelled: C,
    ) -> QueryResult<GqlPage> {
        if page_rows == 0 || page_rows > MAX_PAGE_SIZE {
            return Err(invalid_query("a GQL page holds 1..8192 rows"));
        }
        match &self.state {
            State::Poisoned(error) => return Err(repeat(error)),
            State::Done => {
                return Ok(GqlPage {
                    rows: Vec::new(),
                    done: true,
                    work: GqlWork::default(),
                })
            }
            State::Open => {}
        }
        self.pages += 1;
        let mut erased: &mut dyn FnMut() -> bool = &mut cancelled;
        let mut meter: ExecMeter<'_> = GqlMeter::new(budget, &mut erased);
        let mut cx = ExecCx {
            db: self.db,
            host: self.host,
            params: &self.params,
            meter: &mut meter,
            page: self.pages,
            argument: None,
            replay: None,
        };
        let mut rows = Vec::new();
        let pulled = loop {
            if rows.len() == page_rows {
                break Ok(false);
            }
            match self.root.next(&mut cx) {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => break Ok(true),
                Err(error) => break Err(error),
            }
        };
        match pulled {
            Ok(done) => {
                if done {
                    self.state = State::Done;
                }
                Ok(GqlPage {
                    rows,
                    done,
                    work: meter.work(),
                })
            }
            Err(error) => {
                self.state = State::Poisoned(repeat(&error));
                Err(error)
            }
        }
    }
}

/// A copy of `error` for the next page of a poisoned cursor. A kernel error
/// cannot be copied; it repeats as [`Error::Failed`], the error a handle
/// that stopped on a failure gives.
fn repeat(error: &QueryError) -> QueryError {
    match error {
        QueryError::Cancelled => QueryError::Cancelled,
        QueryError::BudgetExceeded {
            resource,
            limit,
            attempted,
        } => QueryError::BudgetExceeded {
            resource: *resource,
            limit: *limit,
            attempted: *attempted,
        },
        QueryError::Database(error) => QueryError::Database(match error {
            Error::InvalidInput(message) => Error::InvalidInput(message.clone()),
            Error::NotFound(what) => Error::NotFound(what),
            Error::AlreadyExists => Error::AlreadyExists,
            Error::ReadOnly => Error::ReadOnly,
            Error::Failed | Error::Kernel(_) => Error::Failed,
            Error::Corrupt(message) => Error::Corrupt(message.clone()),
            Error::Unsupported(message) => Error::Unsupported(message.clone()),
            Error::Cancelled => Error::Cancelled,
            Error::BudgetExceeded {
                resource,
                limit,
                attempted,
            } => Error::BudgetExceeded {
                resource: *resource,
                limit: *limit,
                attempted: *attempted,
            },
        }),
    }
}
