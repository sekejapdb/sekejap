use super::*;

impl Compiler<'_> {
    // ── GRAPH_TABLE ──────────────────────────────────────────────────────

    /// A pattern compiles to ONE bounded traversal filter plus the collection
    /// the far element names. The seed is a key equality, which is a point
    /// lookup, not a scan.
    pub(super) fn graph_table(&mut self, graph: &GraphTable) -> SqlResult2<(CollectionId, OwnedFilter)> {
        let seed_collection = collection(self.db, &graph.seed_collection)?;
        let target = collection(self.db, &graph.target_collection)?;
        // The seed is ALWAYS a typed slot, even when the pattern wrote the
        // key as a constant: what the plan holds is an entity id, and an
        // entity id is the database's and not the text's -- a row deleted
        // and written again is a new one. So every bind resolves the key
        // afresh, which is one point-get, and a prepared traversal can never
        // walk from a seed that no longer exists.
        let seed_fill = Some(SeedFill {
            collection: seed_collection,
            table: graph.seed_collection.clone(),
            key: graph.seed_key.clone(),
        });
        let key = self.binder().text_of(&graph.seed_key)?;
        let seed = self
            .db
            .get(seed_collection, &key)
            .map_err(SqlError::from)?
            .ok_or_else(|| {
                SqlError::engine(format!(
                    "GRAPH_TABLE seed: `{}` has no row at key `{key}`",
                    graph.seed_collection
                ))
            })?
            .id;
        // `base` is the base graph (GRAPH_CONTRACT 3.1: "no context means the
        // base graph"), which is context 0 and is never a NAMED context, so
        // it is resolved here rather than looked up and then special-cased
        // after the lookup has already failed.
        let context = if graph.context.eq_ignore_ascii_case("base") {
            GraphContextId::BASE
        } else {
            self.db
                .graph_context(&graph.context)
                .map_err(SqlError::from)?
                .ok_or_else(|| {
                    SqlError::engine(format!("no graph context named `{}`", graph.context))
                })?
        };
        let edge_type = match &graph.hop.edge_type {
            None => None,
            Some(name) => Some(
                self.db
                    .edge_type(name)
                    .map_err(SqlError::from)?
                    .ok_or_else(|| SqlError::engine(format!("no edge type named `{name}`")))?,
            ),
        };
        if graph.hop.max_depth > MAX_GRAPH_DEPTH {
            return Err(SqlError::unsupported(format!(
                "a quantifier of {} hops: a traversal is bounded by contract and this slice's bound is {MAX_GRAPH_DEPTH}",
                graph.hop.max_depth
            )));
        }
        if target != seed_collection {
            self.notices.push(format!(
                "GRAPH_TABLE walks from `{}` into `{}`: the traversal itself is untyped by collection, and the far element's label is checked by the collection the outer statement selects from",
                graph.seed_collection, graph.target_collection
            ));
        }
        self.notices.push(
            "GRAPH_TABLE: a WHERE written after the pattern is a POST-FILTER on completed matches (QL_CONTRACT §4.3, Tier 1); an inline element WHERE is the PER-HOP prune (GRAPH_CONTRACT 4.3) and compiles to the traversal's own edge and node predicates"
                .to_owned(),
        );
        // The edge element's inline WHERE. Each comparison is against one
        // property of the edge's own inline bag, so it is typed by the value
        // as written -- there is no declared kind to coerce it to, and an
        // edge property bag is untyped JSON (GRAPH_CONTRACT 2.4's declared
        // properties are a later item).
        let mut edge_where = Vec::with_capacity(graph.hop.predicates.len());
        for predicate in &graph.hop.predicates {
            edge_where.push(OwnedEdgePredicate {
                fill: literal_is_bound(&predicate.value).then(|| predicate.value.clone()),
                property: predicate.property.clone(),
                op: match predicate.op {
                    CmpOp::Eq => Cmp::Eq,
                    CmpOp::Ne => Cmp::Ne,
                    CmpOp::Lt => Cmp::Lt,
                    CmpOp::Le => Cmp::Le,
                    CmpOp::Gt => Cmp::Gt,
                    CmpOp::Ge => Cmp::Ge,
                },
                value: self
                    .binder()
                    .edge_value(&predicate.value, &predicate.property)?,
            });
        }
        // The far element's inline WHERE. Compiled exactly as the outer
        // WHERE's predicates are, and then narrowed to the kinds an index
        // answers without a row -- anything else is REFUSED, never demoted to
        // a post-filter, because a post-filter is a different question: it
        // keeps a node in the frontier that §4.3 says must never be expanded.
        let mut node_where = Vec::with_capacity(graph.node_predicates.len());
        for predicate in &graph.node_predicates {
            let filter = self.filter(target, predicate)?;
            match &filter {
                OwnedFilter::Scalar { predicate, .. } => match predicate {
                    OwnedScalarFilter::Eq(_) | OwnedScalarFilter::Range { .. } => {}
                    OwnedScalarFilter::IsNull | OwnedScalarFilter::IsMissing => {
                        return Err(SqlError::Refused {
                            keyword: "inline element WHERE IS NULL".into(),
                            tier: Tier::Three,
                            reason: "QL_CONTRACT §4.3: a per-hop node predicate is answered from index postings (GRAPH_CONTRACT 4.3, `a traversal never reads a row for a predicate on a covered field`). NULL and MISSING share one nullish index key, so only the row tells them apart; write it after COLUMNS as a post-filter on completed matches.",
                        })
                    }
                },
                OwnedFilter::Point { .. } => {}
                _ => {
                    return Err(SqlError::Refused {
                        keyword: "inline element WHERE".into(),
                        tier: Tier::Two,
                        reason: "QL_CONTRACT §4.3: a per-hop node predicate is answered from index postings, so it is a scalar equality, a scalar range or a point predicate (bbox or radius). A text, geometry or JSON predicate is refined from the row, which GRAPH_CONTRACT 4.3 forbids per hop; write it after COLUMNS as a post-filter on completed matches.",
                    })
                }
            }
            node_where.push(filter);
        }
        Ok((
            target,
            OwnedFilter::Graph(OwnedGraph {
                seed,
                seed_fill,
                direction: match graph.hop.direction {
                    GraphDirection::Outgoing => Direction::Outgoing,
                    GraphDirection::Incoming => Direction::Incoming,
                    GraphDirection::Both => Direction::Both,
                },
                context,
                edge_type,
                min_depth: graph.hop.min_depth,
                max_depth: graph.hop.max_depth,
                edge_where,
                node_where,
            }),
        ))
    }

}
