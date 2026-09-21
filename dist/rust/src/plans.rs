//! The bounded prepared-plan cache, and the reusable [`Statement`] a caller
//! prepares by hand. `docs/lang/QL_CONTRACT.md` §2, `docs/dist/RUST_API.md` §3.
//!
//! One statement compiled once and answered many times is the whole point,
//! and it is the same mechanism twice. [`Statement`] is the explicit form: a
//! caller prepares, then binds each parameter list. [`PlanCache`] is the
//! implicit one: `Db::query` looks its statement text up here, and a HIT is
//! a REBIND -- the compiled plan's typed slots are refilled and nothing is
//! parsed or compiled.
//!
//! ## The three ceilings, fixed at open
//!
//! Law 1 is that no call holds an unbounded amount, so the cache states what
//! it holds rather than growing to the workload:
//!
//! * [`PLAN_CACHE_ENTRIES`] -- how many compiled statements are kept at once.
//! * [`PLAN_CACHE_BYTES`] -- the total statement TEXT held, summed.
//! * [`PLAN_CACHE_STATEMENT_BYTES`] -- the longest statement cached at all; a
//!   statement longer than this is compiled every time and never stored, so
//!   one enormous statement cannot evict every useful one.
//!
//! Eviction is least-recently-used and happens on insert, until both the
//! entry count and the byte total are inside their ceilings.
//!
//! ## The catalog generation
//!
//! A cache key is the statement text AND the catalog generation this handle
//! has seen. `Db` bumps that generation whenever it changes the catalog --
//! a `CREATE`, an `ALTER`, a `DROP`, an index build -- so a plan compiled
//! against a layout that no longer exists is never served: its key cannot be
//! hit again, and it ages out.

use crate::error::{Error, Result};
use crate::rows::{expect_affected, expect_rows, params_of, Row, Rows};
use crate::Db;
use sekejap_lang::{parse_sql, prepare_sql, Param, PreparedSql};
use serde_json::Value;
use std::sync::Arc;

/// How many compiled statements the cache holds at once.
pub const PLAN_CACHE_ENTRIES: usize = 64;
/// The total statement TEXT the cache holds, summed over its entries.
pub const PLAN_CACHE_BYTES: usize = 256 * 1024;
/// The longest statement that is cached at all.
pub const PLAN_CACHE_STATEMENT_BYTES: usize = 8 * 1024;

/// What the plan cache has done, for a caller that wants to see whether its
/// statements are being reused. `Db::cache_stats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Compiled statements held right now.
    pub entries: usize,
    /// Statement text held right now, in bytes.
    pub bytes: usize,
    /// Lookups that found a compiled plan. Every hit is a REBIND.
    pub hits: u64,
    /// Lookups that did not, and therefore compiled.
    pub misses: u64,
    /// Entries dropped to stay inside a ceiling.
    pub evictions: u64,
    /// Statements never stored because they are longer than
    /// [`PLAN_CACHE_STATEMENT_BYTES`].
    pub too_long: u64,
    /// The ceilings this cache was opened with.
    pub entry_ceiling: usize,
    pub byte_ceiling: usize,
    pub statement_ceiling: usize,
}

struct Entry {
    sql: Arc<str>,
    generation: u64,
    prepared: PreparedSql,
}

/// The cache itself. `Db` holds one behind a `Mutex`.
pub(crate) struct PlanCache {
    /// Least-recently-used first, so eviction pops the front.
    entries: Vec<Entry>,
    bytes: usize,
    stats: CacheStats,
}

impl PlanCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            bytes: 0,
            stats: CacheStats {
                entry_ceiling: PLAN_CACHE_ENTRIES,
                byte_ceiling: PLAN_CACHE_BYTES,
                statement_ceiling: PLAN_CACHE_STATEMENT_BYTES,
                ..CacheStats::default()
            },
        }
    }

    pub(crate) fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            bytes: self.bytes,
            ..self.stats
        }
    }

    /// Take the plan for `sql` OUT of the cache, so the caller owns it for
    /// the length of one execution and gives it back afterwards. Taking
    /// rather than borrowing is what keeps a rebind -- which MUTATES the
    /// plan's slots -- from racing a second caller running the same text.
    pub(crate) fn take(&mut self, sql: &str, generation: u64) -> Option<PreparedSql> {
        let at = self
            .entries
            .iter()
            .position(|entry| entry.generation == generation && &*entry.sql == sql)?;
        let entry = self.entries.remove(at);
        self.bytes -= entry.sql.len();
        self.stats.hits += 1;
        Some(entry.prepared)
    }

    pub(crate) fn missed(&mut self, sql: &str) {
        self.stats.misses += 1;
        if sql.len() > PLAN_CACHE_STATEMENT_BYTES {
            self.stats.too_long += 1;
        }
    }

    /// Give a plan back, as the most recently used entry, and evict until
    /// both ceilings hold.
    pub(crate) fn give(&mut self, sql: &str, generation: u64, prepared: PreparedSql) {
        if sql.len() > PLAN_CACHE_STATEMENT_BYTES {
            return;
        }
        // A concurrent miss may have compiled the same text meanwhile.
        if let Some(at) = self
            .entries
            .iter()
            .position(|entry| entry.generation == generation && &*entry.sql == sql)
        {
            let old = self.entries.remove(at);
            self.bytes -= old.sql.len();
        }
        self.bytes += sql.len();
        self.entries.push(Entry {
            sql: Arc::from(sql),
            generation,
            prepared,
        });
        while self.entries.len() > PLAN_CACHE_ENTRIES
            || (self.bytes > PLAN_CACHE_BYTES && self.entries.len() > 1)
        {
            let evicted = self.entries.remove(0);
            self.bytes -= evicted.sql.len();
            self.stats.evictions += 1;
        }
    }

    /// Drop every entry. A catalog change bumps the generation instead, so
    /// this is for a caller that wants the memory back.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// One statement, prepared by hand and bound many times.
///
/// ## Why preparing is lazy
///
/// This engine's compiler FOLDS at prepare: a §4.1 / §4.2 rewrite computes
/// its index range from the written value, a `GRAPH_TABLE` seed resolves its
/// key, a semi-join builds its set. Compiling therefore needs the first
/// parameter list, and [`Db::prepare`] cannot have one. So [`Db::prepare`]
/// PARSES -- a syntax error is reported there and then -- and the compile
/// happens on the first `*_with` call. Every call after that is a REBIND:
/// the compiled plan's typed slots are refilled, and nothing is parsed or
/// compiled. [`Statement::rebindable`] says whether that held; when it did
/// not, a bind compiles again from the statement parsed once, which is still
/// one parse for the life of the handle.
pub struct Statement<'a> {
    db: &'a Db,
    sql: String,
    compiled: Option<PreparedSql>,
    binds: u64,
    compiles: u64,
}

impl<'a> Statement<'a> {
    pub(crate) fn new(db: &'a Db, sql: &str) -> Result<Self> {
        parse_sql(sql)?;
        Ok(Self {
            db,
            sql: sql.to_owned(),
            compiled: None,
            binds: 0,
            compiles: 0,
        })
    }

    /// The statement as written.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The columns this statement returns, once it has been bound at least
    /// once. Empty before that, and for anything that is not a SELECT.
    pub fn columns(&self) -> &[String] {
        self.compiled.as_ref().map_or(&[], PreparedSql::columns)
    }

    /// True once this statement has been compiled and its every `$n` landed
    /// in a typed slot, so a further bind compiles nothing. `None` before
    /// the first bind, which is what compiles it.
    pub fn rebindable(&self) -> Option<bool> {
        self.compiled.as_ref().map(PreparedSql::rebindable)
    }

    /// Why a bind has to compile again, when it does.
    pub fn rebind_refusal(&self) -> Option<String> {
        self.compiled.as_ref().and_then(PreparedSql::rebind_refusal)
    }

    /// How many times this statement has been bound, and how many of those
    /// binds had to compile. The second number is one for a rebindable
    /// statement, however many times it is run.
    pub fn counters(&self) -> (u64, u64) {
        (self.binds, self.compiles)
    }

    /// Run this statement as a row-returning one and assemble the answer.
    pub fn query_with(&mut self, params: &[Value]) -> Result<Rows> {
        let params = params_of(params);
        let sql = self.sql.clone();
        self.with_bound(&params, |db, prepared| {
            expect_rows(prepared.run(db)?, &sql)
        })
    }

    /// Run this statement as a writing one and commit. Returns the rows it
    /// moved.
    ///
    /// A write compiles and runs under the writer's borrow, and its document
    /// is folded at compile, so a writing statement is never rebindable: this
    /// saves the PARSE and nothing else, and says so through
    /// [`Statement::rebindable`].
    pub fn execute_with(&mut self, params: &[Value]) -> Result<u64> {
        let params = params_of(params);
        let Statement {
            db,
            sql,
            compiled,
            binds,
            compiles,
        } = self;
        *binds += 1;
        db.write(|database| {
            match compiled.as_mut() {
                Some(prepared) => {
                    if !prepared.rebindable() {
                        *compiles += 1;
                    }
                    prepared.bind(database, &params)?;
                }
                None => {
                    *compiles += 1;
                    *compiled = Some(prepare_sql(database, sql, &params)?);
                }
            }
            let prepared = compiled.as_ref().expect("just bound");
            expect_affected(prepared.run_mut(database)?, sql)
        })
    }

    /// Page this statement and hand each row to `body`, holding one page at
    /// a time. Returns the rows handed over.
    pub fn stream_with(
        &mut self,
        params: &[Value],
        page_rows: usize,
        body: &mut dyn FnMut(&Row) -> Result<()>,
    ) -> Result<u64> {
        let params = params_of(params);
        let page_rows = page_rows.max(1);
        self.with_bound(&params, |db, prepared| {
            let columns = Arc::new(prepared.columns().to_vec());
            let mut seen = 0u64;
            let mut stopped: Option<Error> = None;
            prepared.for_each_row(db, page_rows, &mut |row| {
                if stopped.is_some() {
                    return Ok(());
                }
                let row = Row {
                    columns: Arc::clone(&columns),
                    id: row.id,
                    values: row.values.clone(),
                };
                match body(&row) {
                    Ok(()) => {
                        seen += 1;
                        Ok(())
                    }
                    Err(e) => {
                        stopped = Some(e);
                        Ok(())
                    }
                }
            })?;
            match stopped {
                Some(e) => Err(e),
                None => Ok(seen),
            }
        })
    }

    /// Compile-or-rebind, then run `body` on the bound plan.
    fn with_bound<T>(
        &mut self,
        params: &[Param],
        body: impl FnOnce(&sekejap_core::collections::Database, &PreparedSql) -> Result<T>,
    ) -> Result<T> {
        let Statement {
            db,
            sql,
            compiled,
            binds,
            compiles,
        } = self;
        *binds += 1;
        db.read(|database| {
            match compiled.as_mut() {
                Some(prepared) => {
                    if !prepared.rebindable() {
                        *compiles += 1;
                    }
                    prepared.bind(database, params)?;
                }
                None => {
                    *compiles += 1;
                    *compiled = Some(prepare_sql(database, sql, params)?);
                }
            }
            let prepared = compiled.as_ref().expect("just bound");
            body(database, prepared)
        })
    }
}
