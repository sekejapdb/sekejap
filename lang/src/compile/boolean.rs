use super::*;

impl Compiler<'_> {
    /// One conjunct of a `WHERE` clause, as a filter.
    ///
    /// The tree is handed to the engine as it was written: `Any` is a union
    /// of membership sets, `All` an intersection and `Not` a complement, and
    /// which leaves an index can answer is the engine's question to refuse,
    /// not this one's. What is decided here is only the SQL spelling --
    /// `<>` is a complement of an equality, `IN` a union of them.
    pub(super) fn where_filter(&mut self, c: CollectionId, expr: &Expr) -> SqlResult2<OwnedFilter> {
        Ok(match expr {
            Expr::Leaf(predicate) => self.filter(c, predicate)?,
            Expr::Not(inner) => OwnedFilter::Not(Box::new(self.where_filter(c, inner)?)),
            Expr::Or(parts) => OwnedFilter::Any(
                parts
                    .iter()
                    .map(|part| self.where_filter(c, part))
                    .collect::<SqlResult2<Vec<_>>>()?,
            ),
            Expr::And(parts) => OwnedFilter::All(
                parts
                    .iter()
                    .map(|part| self.where_filter(c, part))
                    .collect::<SqlResult2<Vec<_>>>()?,
            ),
        })
    }

    /// The outer ids a semi-join names (`docs/lang/QL_CONTRACT.md` §3).
    ///
    /// Two sources, because this database has two kinds of thing a subquery
    /// can name. An EDGE TYPE is one walk of the edge keyspace: `related` is
    /// not a collection here, it is the `related` edges, and its `source`
    /// column is every entity with an outgoing one. A COLLECTION is
    /// §4.8's join shape -- one key lookup per driving row -- with the
    /// driving rows read through an ordinary prepared query, so the
    /// subquery's own cost is a plan a caller can see rather than a hidden
    /// scan.
    ///
    /// Both sources run under the STATEMENT's own budget and cancellation and
    /// both are bounded by `MAX_SEMI_JOIN_IDS`, which is the size of the set
    /// this returns and holds. The edge walk used to have neither the cap nor
    /// the cancel; the collection walk ran under `QueryBudget::unlimited()`.
    pub(super) fn semi_join(
        &mut self,
        c: CollectionId,
        table: &str,
        column: &str,
    ) -> SqlResult2<Vec<EntityId>> {
        // The set is built HERE, while the statement compiles, and what it
        // holds is the database's rows -- not the caller's parameters. A
        // compiled statement that carries one is therefore a constant of its
        // PREPARE and is never rebound: it is compiled again, which builds
        // the set again.
        self.folds_reason(format!(
            "a semi-join over `{table}.{column}` builds its membership set while the statement compiles (QL_CONTRACT §3)"
        ));
        let db = self.db;
        let budget = self.budget;
        if let Some(edge_type) = db.edge_type(table).ok().flatten() {
            let direction = if column.eq_ignore_ascii_case("source") {
                Direction::Outgoing
            } else if column.eq_ignore_ascii_case("destination") {
                Direction::Incoming
            } else {
                return Err(SqlError::unsupported(format!(
                    "a semi-join on `{table}.{column}`: an edge type's columns are `source` and `destination`, which are the two ends the edge keyspace is filed by"
                )));
            };
            // Which of the two the engine took, printed rather than
            // guessed: a file that carries the ENDPOINT SETS answers from one
            // posting per distinct entity, a file written before them takes
            // the seek-per-entity walk of the edge keyspace
            // (`core/engine/src/index/graph/endpoints.rs`).
            self.notices.push(if db.endpoint_sets_present() {
                format!(
                    "semi-join over `{table}.{column}`: endpoint set -- one posting per distinct entity in one range"
                )
            } else {
                format!(
                    "semi-join over `{table}.{column}`: edge walk -- this file carries no endpoint sets, so the walk seeks past each matched entity's edges (Database::backfill_endpoint_sets builds them once)"
                )
            });
            let cancelled = &mut *self.cancelled;
            return Ok(db.edge_endpoints(
                c,
                GraphContextId::BASE,
                edge_type,
                direction,
                MAX_SEMI_JOIN_EDGES,
                MAX_SEMI_JOIN_IDS,
                budget,
                || cancelled(),
            )?);
        }
        let inner = collection(db, table)?;
        let fields = [column];
        let mut prepared = db
            .prepare_query(QueryRequest {
                collection: inner,
                filters: &[],
                order: QueryOrder::Driver,
                projection: Projection::Fields(&fields),
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .map_err(SqlError::from)?;
        let cancelled = &mut *self.cancelled;
        let mut ids: Vec<EntityId> = Vec::new();
        loop {
            let page = prepared
                .next_page(1024, budget, || cancelled())
                .map_err(SqlError::from)?;
            for row in &page.rows {
                match row.projected.first() {
                    Some((_, ProjectedValue::Value(serde_json::Value::String(key)))) => {
                        if let Some(entity) = db.get(c, key).map_err(SqlError::from)? {
                            ids.push(entity.id);
                        }
                    }
                    // A row whose projected column is NULL or absent names no
                    // outer row, which is SQL's own answer: `x IN (SELECT c
                    // ...)` is never TRUE because of a NULL `c`.
                    None
                    | Some((_, ProjectedValue::Missing))
                    | Some((_, ProjectedValue::Null))
                    | Some((_, ProjectedValue::Value(serde_json::Value::Null))) => {}
                    // Anything else is a column this join cannot use, and
                    // silently dropping every row of it returned the EMPTY
                    // set with no diagnostic. An outer row is named by its
                    // external key, which is text.
                    Some((_, ProjectedValue::Value(other))) => {
                        let kind = match other {
                            serde_json::Value::Bool(_) => "a boolean",
                            serde_json::Value::Number(_) => "a number",
                            serde_json::Value::Array(_) => "an array",
                            serde_json::Value::Object(_) => "an object",
                            serde_json::Value::Null | serde_json::Value::String(_) => {
                                unreachable!("null and text are answered above")
                            }
                        };
                        return Err(SqlError::unsupported(format!(
                            "a semi-join over `{table}` projects `{column}`, which holds {kind}: an outer row is named by its external key, which is text, so there is no id this column could name"
                        )));
                    }
                }
            }
            if ids.len() > MAX_SEMI_JOIN_IDS {
                return Err(SqlError::engine(format!(
                    "a semi-join over `{table}` named more than {MAX_SEMI_JOIN_IDS} outer rows: the set is held in memory and is bounded, not spilled"
                )));
            }
            if page.done {
                break;
            }
        }
        ids.sort_unstable_by_key(|id| id.sequence);
        ids.dedup();
        Ok(ids)
    }

    /// True when a tsquery is a bare `!term` -- one leading `!` and no other
    /// operator -- which is the one negated tsquery this slice compiles.
    ///
    /// A SHAPE test, so it reads the value without recording a fold; the
    /// negated arm itself records one, because whether the plan is a
    /// complement is then decided by the parameter's VALUE.
    pub(super) fn negated_tsquery(&self, query: &TsQuery) -> SqlResult2<bool> {
        if !query.tsquery_syntax {
            return Ok(false);
        }
        let text = self.binder().text_of(&query.source)?;
        let trimmed = text.trim();
        Ok(trimmed.starts_with('!')
            && !trimmed[1..].contains('!')
            && !trimmed.contains('|')
            && !trimmed.contains('&'))
    }
}
