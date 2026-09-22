use super::*;

impl Compiler<'_> {
    // ── SELECT ───────────────────────────────────────────────────────────

    pub(super) fn select(&mut self, mut statement: SelectStmt) -> SqlResult2<SelectPlan> {
        // A CONSTANT at the top of the WHERE is folded here rather than
        // compiled into a filter: `WHERE 1=1` is no predicate at all, and
        // `WHERE 1<>1` is a statement that returns no rows, which is a
        // `LIMIT 0` -- a plan whose driver is never stepped. Neither
        // emulates anything; the truth value was decided by the parser and
        // there is nothing left for an index to answer. (pgjdbc writes
        // `SELECT * FROM t WHERE 1<>1 LIMIT 1` to learn a result's COLUMNS
        // without fetching a row.) A constant nested inside an `OR` or a
        // `NOT` is not this shape and is refused by name in
        // `predicates.rs`, where the leaf is compiled.
        let mut always_false = false;
        statement.predicates.retain(|expr| match expr {
            Expr::Leaf(Predicate::Constant(value)) => {
                always_false |= !*value;
                false
            }
            _ => true,
        });
        if always_false {
            self.notices.push(
                "a constant FALSE predicate: the statement returns no rows, so it compiles to LIMIT 0 -- no driver is stepped and no row is read"
                    .to_owned(),
            );
            statement.limit = Some(0);
        }
        let statement = statement;
        let (c, graph_filter, graph_columns) = match &statement.source {
            Source::Table(name) => (collection(self.db, name)?, None, None),
            Source::All => {
                return Err(SqlError::Refused {
                    keyword: "FROM ALL".into(),
                    tier: Tier::Two,
                    reason: super::dml::FROM_ALL,
                })
            }
            Source::Graph(graph) => {
                let (target, filter) = self.graph_table(graph)?;
                (target, Some(filter), Some(graph.columns.clone()))
            }
        };

        let mut filters: Vec<OwnedFilter> = Vec::new();
        if let Some(filter) = graph_filter {
            filters.push(filter);
        }
        for expr in &statement.predicates {
            filters.push(self.where_filter(c, expr)?);
        }

        // An alias a `COLUMNS` entry gave to an EDGE property. `ORDER BY` and
        // the select list resolve against this before they look for a column
        // of the far node, because the two namespaces are distinct and the
        // pattern is what bound the edge one.
        let edge_aliases: Vec<(String, String)> = graph_columns
            .iter()
            .flatten()
            .filter_map(|(item, alias)| match item {
                GraphColumn::Edge(property) => Some((alias.clone(), property.clone())),
                GraphColumn::Node(_) => None,
            })
            .collect();

        let order = match &statement.order {
            None => OwnedOrder::Driver,
            Some(OrderKey::Column { column, descending }) => {
                match edge_aliases
                    .iter()
                    .find(|(alias, _)| alias == column)
                    .map(|(_, property)| property.clone())
                {
                    Some(property) => OwnedOrder::Edge {
                        property,
                        direction: if *descending {
                            SortDirection::Descending
                        } else {
                            SortDirection::Ascending
                        },
                    },
                    None => self.order(c, &OrderKey::Column {
                        column: column.clone(),
                        descending: *descending,
                    })?,
                }
            }
            Some(key) => self.order(c, key)?,
        };

        // The select list. `_id` is free (a row carries its id); a named
        // column is a projected field; an expression is this statement's own
        // ranking value.
        // `COLUMNS` entries that read the EDGE become projection fields under
        // the `@edge.` spelling the engine resolves from the traversal; the
        // rest are ordinary select items over the far node's row. Both keep
        // the position the statement wrote them in.
        let items: Vec<(GraphColumn, Option<String>)> = match graph_columns {
            Some(columns) => columns
                .into_iter()
                .map(|(item, alias)| (item, Some(alias)))
                .collect(),
            None => statement
                .items
                .clone()
                .into_iter()
                .map(|(item, alias)| (GraphColumn::Node(item), alias))
                .collect(),
        };
        let mut columns = Vec::new();
        let mut outputs = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        let mut functions: Vec<CompiledRow> = Vec::new();
        let push_field = |field: String, fields: &mut Vec<String>| -> Output {
            match fields.iter().position(|existing| *existing == field) {
                Some(at) => Output::Field(at),
                None => {
                    fields.push(field);
                    Output::Field(fields.len() - 1)
                }
            }
        };
        for (item, alias) in items {
            let item = match item {
                GraphColumn::Node(item) => item,
                GraphColumn::Edge(property) => {
                    // `@edge.<property>` is the engine's projection spelling
                    // for the reaching edge (`query::EDGE_FIELD_PREFIX`); it
                    // reads no row and collides with no declared field.
                    let field = format!("{EDGE_FIELD_PREFIX}{property}");
                    columns.push(alias.unwrap_or_else(|| property.clone()));
                    outputs.push(push_field(field, &mut fields));
                    continue;
                }
            };
            match item {
                SelectItem::Star => {
                    for field in self.declared_fields(c)? {
                        columns.push(field.clone());
                        // `SELECT *` prints a declared TIMESTAMPTZ/DATE the
                        // same way `SELECT born_ts` does: the declared type
                        // belongs to the COLUMN, not to the spelling that
                        // named it, so the two cannot disagree.
                        match self.time_column(c, &field)? {
                            None => outputs.push(push_field(field, &mut fields)),
                            Some(declared) => {
                                let Output::Field(at) = push_field(field.clone(), &mut fields)
                                else {
                                    unreachable!("push_field returns a field position");
                                };
                                self.row_functions.push(format!(
                                    "{field} -> ISO-8601 text (declared {declared}, stored Int microseconds)"
                                ));
                                functions.push(CompiledRow::Iso {
                                    arg: Box::new(CompiledRow::Field { at, time: true }),
                                    date_only: declared == "DATE",
                                });
                                outputs.push(Output::Row(functions.len() - 1));
                            }
                        }
                    }
                }
                SelectItem::Id => {
                    columns.push(alias.clone().unwrap_or_else(|| ID_COLUMN.to_owned()));
                    outputs.push(Output::Id);
                }
                SelectItem::Key => {
                    columns.push(alias.clone().unwrap_or_else(|| KEY_COLUMN.to_owned()));
                    outputs.push(Output::Key);
                }
                SelectItem::Column(name) => {
                    self.kind_of(c, &name)?;
                    columns.push(alias.clone().unwrap_or_else(|| name.clone()));
                    // A declared TIMESTAMPTZ/DATE is stored as an integer and
                    // PRINTS as an ISO-8601 string (QL_CONTRACT §4.2): the
                    // declared type in the catalog descriptor is what says so,
                    // and the conversion is a row function like any other.
                    match self.time_column(c, &name)? {
                        None => outputs.push(push_field(name.clone(), &mut fields)),
                        Some(declared) => {
                            let Output::Field(at) = push_field(name.clone(), &mut fields) else {
                                unreachable!("push_field returns a field position");
                            };
                            self.row_functions.push(format!(
                                "{name} -> ISO-8601 text (declared {declared}, stored Int microseconds)"
                            ));
                            functions.push(CompiledRow::Iso {
                                arg: Box::new(CompiledRow::Field { at, time: true }),
                                date_only: declared == "DATE",
                            });
                            outputs.push(Output::Row(functions.len() - 1));
                        }
                    }
                }
                SelectItem::Function(expr) => {
                    let compiled = self.row_function(c, &expr, &mut fields)?;
                    let written = expr.written();
                    self.row_functions.push(format!(
                        "{written} -> evaluated over this row's projected values, after the index-side stage (cost is proportional to the rows RETURNED)"
                    ));
                    columns.push(alias.clone().unwrap_or(written));
                    functions.push(compiled);
                    outputs.push(Output::Row(functions.len() - 1));
                }
                SelectItem::OrderValue(_) | SelectItem::Divided { .. } => {
                    let what = match &item {
                        SelectItem::Divided { column, divisor } => {
                            format!("`{column} / {divisor}`")
                        }
                        SelectItem::OrderValue(what) => what.clone(),
                        _ => unreachable!("this arm took both"),
                    };
                    if statement.order.is_none() {
                        return Err(SqlError::unsupported(format!(
                            "{what} in a select list: the only expression a row can report here is this statement's own ranking value, and this statement has no ORDER BY"
                        )));
                    }
                    columns.push(alias.clone().unwrap_or_else(|| "score".to_owned()));
                    outputs.push(Output::OrderValue);
                }
                // `Compiler::aggregate` has already taken every statement
                // that names one; reaching here would mean this path was
                // asked to return ROWS for a folded answer.
                SelectItem::Aggregate { function, .. } => {
                    return Err(SqlError::unsupported(format!(
                        "{}() in a select list that is not a folded answer",
                        function.written()
                    )))
                }
            }
        }

        if outputs.iter().any(|output| *output == Output::Key) {
            self.notices.push(format!(
                "selecting `{KEY_COLUMN}` costs one `get_by_id` per returned row: a page cannot project the reserved field the external key lives in, so the key is fetched after the walk. `{ID_COLUMN}` is free"
            ));
        }
        // `docs/core/GRAPH_CONTRACT.md` §4.2: the reaching edge is carried only by
        // the traversal's own candidate stream, so a statement that reads it
        // must run on the graph driver. When a `_key` predicate is a POST-
        // FILTER beside such a traversal, the traversal keeps the driver and
        // the key range is answered from the external key the row carries
        // (`plan.rs`, `filters.rs`) -- the alternative would be an answer
        // ordered by nothing with every edge column `Missing`.
        let reads_the_edge = matches!(order, OwnedOrder::Edge { .. })
            || fields
                .iter()
                .any(|field| field.starts_with(EDGE_FIELD_PREFIX));
        let drives_the_graph = reads_the_edge
            && filters
                .iter()
                .any(|filter| matches!(filter, OwnedFilter::Graph(_)));
        let driver = if drives_the_graph {
            CandidateDriver::Auto
        } else if filters
            .iter()
            .any(|filter| matches!(filter, OwnedFilter::Key { .. }))
        {
            // A key filter is meaningful only under the driver that
            // certifies it from the mapping entry itself.
            CandidateDriver::Keys
        } else {
            CandidateDriver::Auto
        };

        Ok(SelectPlan {
            collection: c,
            columns,
            outputs,
            fields,
            filters,
            order,
            limit: statement.limit,
            driver,
            text: String::new(),
            functions,
            rewrites: std::mem::take(&mut self.rewrites),
            row_functions: std::mem::take(&mut self.row_functions),
        })
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    fn order(&mut self, c: CollectionId, key: &OrderKey) -> SqlResult2<OwnedOrder> {
        Ok(match key {
            OrderKey::Column { column, descending } => {
                if column == ID_COLUMN {
                    if *descending {
                        return Err(SqlError::unsupported(
                            "ORDER BY _id DESC: QueryOrder::EntityId ascends; the primary tree is walked forwards",
                        ));
                    }
                    return Ok(OwnedOrder::EntityId);
                }
                let index = self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?;
                OwnedOrder::Scalar {
                    index,
                    direction: if *descending {
                        SortDirection::Descending
                    } else {
                        SortDirection::Ascending
                    },
                }
            }
            OrderKey::Distance {
                column,
                point,
                descending,
            } => {
                if *descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY <-> DESC: QueryOrder::Distance refuses Descending at prepare -- there is no reverse KNN walk, and treating DESC as ASC would answer a different question",
                    ));
                }
                let index = self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?;
                OwnedOrder::Distance {
                    index,
                    center: self.binder().point_of(point)?,
                    fill: point_is_bound(point).then(|| point.clone()),
                }
            }
            OrderKey::Vector {
                column,
                query,
                op,
                descending,
            } => {
                if *descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY <=> DESC: a vector order ascends by distance; the nearest is first",
                    ));
                }
                let metric = match op {
                    VecOp::Cosine => VectorMetric::Cosine,
                    VecOp::L2 => VectorMetric::SquaredL2,
                    VecOp::NegativeDot => VectorMetric::NegativeDot,
                };
                let vector = self.binder().vector_of(query)?;
                let fill = literal_is_bound(query).then(|| query.clone());
                self.vector_order(c, column, vector, fill, metric)?
            }
            OrderKey::Bm25 {
                column,
                query,
                descending,
            } => {
                if !*descending {
                    self.notices.push(
                        "ORDER BY a text rank ASC: BM25 ranks best-first, so this page returns the WORST matches first"
                            .to_owned(),
                    );
                }
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (text, matching, fill) = self.tsquery_slot(query)?;
                if !*descending {
                    return Err(SqlError::unsupported(
                        "ORDER BY ts_rank_cd(...) ASC: QueryOrder::Bm25 ranks best-first and has no ascending form; write DESC",
                    ));
                }
                OwnedOrder::Bm25 {
                    index,
                    query: text,
                    matching,
                    fill,
                }
            }
            OrderKey::Score { expr, descending } => OwnedOrder::Score {
                expr: self.score(c, expr)?,
                direction: if *descending {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                },
            },
        })
    }

    /// Exact or approximate, decided by the indexes the column has and by a
    /// `SET LOCAL` the session saw -- never by a planner knob.
    fn vector_order(
        &mut self,
        c: CollectionId,
        column: &str,
        query: Vec<f32>,
        fill: Option<Literal>,
        metric: VectorMetric,
    ) -> SqlResult2<OwnedOrder> {
        let indexes = self.db.list_indexes(c).map_err(SqlError::from)?;
        let ready = |family: IndexFamily| -> Option<IndexId> {
            indexes
                .iter()
                .find(|info| {
                    info.field == column && info.family == family && info.state == IndexState::Ready
                })
                .map(|info| info.id)
        };
        let exact = ready(IndexFamily::ExactVector);
        let quantized = ready(IndexFamily::QuantizedVector);
        let ef = EF_SEARCH.with(Cell::get);
        match (exact, quantized, ef) {
            (Some(index), _, None) => Ok(OwnedOrder::ExactVector {
                index,
                query,
                metric,
                fill,
            }),
            (_, Some(index), ef) => {
                let ef = ef.unwrap_or(DEFAULT_EF);
                self.notices.push(format!(
                    "ORDER BY a vector distance on `{column}` is APPROXIMATE (ef={ef}): the quantized index answers it, and the shortlist bounds the whole result set of this prepared query"
                ));
                Ok(OwnedOrder::ApproximateVector {
                    index,
                    query,
                    metric,
                    ef,
                    fill,
                })
            }
            (Some(index), None, Some(ef)) => {
                self.notices.push(format!(
                    "SET LOCAL ef_search = {ef} was seen but `{column}` has only an exact vector index; the answer is exact and the shortlist bound is unused"
                ));
                Ok(OwnedOrder::ExactVector {
                    index,
                    query,
                    metric,
                    fill,
                })
            }
            (None, None, _) => Err(SqlError::engine(format!(
                "no vector index on `{column}`: a vector order names either the exact family (page-order sidecar scan) or the quantized one (compact scan then f32 rerank)"
            ))),
        }
    }

    fn score(&mut self, c: CollectionId, node: &ScoreNode) -> SqlResult2<OwnedScore> {
        Ok(match node {
            ScoreNode::Lit(value) => OwnedScore::Lit(*value),
            ScoreNode::Column(column) => {
                let kind = self.kind_of(c, column)?;
                if matches!(kind, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "`{column}` is TEXT: a Score leaf is numeric, and a text scalar is refused at prepare (`ScoreExpr::Scalar`, src/query/mod.rs)"
                    )));
                }
                OwnedScore::Scalar {
                    index: self.index_for(c, column, IndexFamily::Scalar, "a scalar index")?,
                }
            }
            ScoreNode::Bm25 { column, query } => {
                let index = self.index_for(c, column, IndexFamily::Text, "a text index")?;
                let (text, matching, fill) = self.tsquery_slot(query)?;
                OwnedScore::Bm25 {
                    index,
                    query: text,
                    matching,
                    fill,
                }
            }
            ScoreNode::SearchScore => match &self.search_leaf {
                Some(SearchLeaf::One { index, query }) => OwnedScore::SearchScore {
                    index: *index,
                    query: query.clone(),
                },
                Some(SearchLeaf::Several) => {
                    return Err(SqlError::Refused {
                        keyword: "search_score".into(),
                        tier: Tier::Two,
                        reason: "QL_CONTRACT §4.6: search_score() is the Score leaf of ONE search() predicate, and this statement writes more than one; there is no spelling that says which, so it is refused rather than bound to whichever compiled last.",
                    })
                }
                None => {
                    return Err(SqlError::Refused {
                        keyword: "search_score".into(),
                        tier: Tier::Two,
                        reason: "QL_CONTRACT §4.6: search_score() scores the search() predicate of its own statement; with no search() in the WHERE there is nothing to score, and a number with no predicate behind it would mean nothing.",
                    })
                }
            },
            ScoreNode::VecDistance { column, query, op } => {
                let index =
                    self.index_for(c, column, IndexFamily::ExactVector, "an exact vector index")?;
                OwnedScore::VectorDistance {
                    index,
                    query: self.binder().vector_of(query)?,
                    fill: literal_is_bound(query).then(|| query.clone()),
                    metric: match op {
                        VecOp::Cosine => VectorMetric::Cosine,
                        VecOp::L2 => VectorMetric::SquaredL2,
                        VecOp::NegativeDot => VectorMetric::NegativeDot,
                    },
                }
            }
            ScoreNode::Distance { column, point } => OwnedScore::Distance {
                index: self.index_for(c, column, IndexFamily::SpatialPoint, "a point index")?,
                center: self.binder().point_of(point)?,
                fill: point_is_bound(point).then(|| point.clone()),
            },
            ScoreNode::Add(a, b) => OwnedScore::Add(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Sub(a, b) => OwnedScore::Sub(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Mul(a, b) => OwnedScore::Mul(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Div(a, b) => OwnedScore::Div(
                Box::new(self.score(c, a)?),
                Box::new(self.score(c, b)?),
            ),
            ScoreNode::Neg(a) => OwnedScore::Neg(Box::new(self.score(c, a)?)),
        })
    }

}
