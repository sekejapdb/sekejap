use super::*;

impl Parser {
    // ── GRAPH_TABLE ──────────────────────────────────────────────────────

    pub(super) fn graph_table(&mut self) -> SqlResult2<GraphTable> {
        self.expect_word("GRAPH_TABLE")?;
        self.expect(&Tok::LParen)?;
        let context = self.name()?;
        self.expect_word("MATCH")?;
        // A pattern MODE word stands exactly here in SQL/PGQ -- `MATCH TRAIL
        // (...)`, `MATCH ANY SHORTEST (...)` -- so the table is asked before
        // the `(` is demanded, and the word is refused as ITSELF: `ALL
        // SHORTEST` used to come back named `ANY SHORTEST` because one hand
        // written `if` covered both (§4.3, §7 item 10).
        self.guard_here()?;
        if matches!(self.word().as_deref(), Some("ANY" | "ALL" | "SHORTEST")) {
            // A path-selector word the table has no two-word row for. It is
            // the shortest-path atomic it is asking for either way.
            return Err(refuse::refuse("ANY SHORTEST"));
        }
        // (a:coll WHERE a.key = $1)
        self.expect(&Tok::LParen)?;
        let seed_variable = self.name()?;
        let seed_collection = self.element_label()?;
        if !self.eat_word("WHERE") {
            return Err(SqlError::unsupported(format!(
                "GRAPH_TABLE ({seed_variable}:{seed_collection}) has no starting key. A pattern starts at ONE row named by its key -- `({seed_variable}:{seed_collection} WHERE {seed_variable}._key = '...')` -- and walks out from it; starting at every row of `{seed_collection}` would read the whole table, which sekejap does not do silently (QL_CONTRACT §6)"
            )));
        }
        // `name()` reads `a._key` and keeps the last segment, so the element
        // variable in front of the field is already accounted for.
        let field = self.name()?;
        if !super::is_key_column(&field) {
            return Err(SqlError::Refused {
                keyword: "inline element WHERE".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §4.3 and §5 deviation 2: an inline element WHERE prunes per hop (GRAPH_CONTRACT 4.3). Only a key equality seeds a traversal in this slice; any other inline predicate is the per-hop prune, which is Tier 2.",
            });
        }
        self.expect(&Tok::Eq)?;
        let seed_key = self.literal()?;
        if self.word().as_deref() == Some("AND") {
            return Err(SqlError::Refused {
                keyword: "inline element WHERE".into(),
                tier: super::Tier::Two,
                reason: "QL_CONTRACT §4.3 and §5 deviation 2: an inline element WHERE prunes per hop (GRAPH_CONTRACT 4.3). Only a key equality seeds in this slice.",
            });
        }
        self.expect(&Tok::RParen)?;
        let (hop, edge_variable) = self.graph_hop()?;
        self.expect(&Tok::LParen)?;
        let target_variable = self.name()?;
        let target_collection = self.element_label()?;
        // The far element's inline WHERE: the per-hop node prune of
        // GRAPH_CONTRACT 4.3, not a post-filter on completed matches. It is
        // written in the same predicate grammar the outer WHERE uses, and
        // `Compiler::graph_table` refuses any of them that an index cannot
        // answer without opening the row.
        let node_predicates = if self.eat_word("WHERE") {
            self.conjunction()?
        } else {
            Vec::new()
        };
        self.expect(&Tok::RParen)?;
        self.expect_word("COLUMNS")?;
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        loop {
            // A COLUMNS entry is qualified by the element it reads: `b.name`
            // is a field of the far NODE, `r.weight` a property of the EDGE
            // the pattern bound. The two namespaces are told apart by the
            // variable, which is why this keeps it.
            let at = self.here();
            let (qualifier, field) = self.qualified_name()?;
            // Case-insensitively, as every other name comparison in this
            // parser is: `R.weight` against a pattern that bound `r` is the
            // EDGE's property, not a row field that does not exist.
            let edge = qualifier
                .as_deref()
                .zip(edge_variable.as_deref())
                .is_some_and(|(written, bound)| written.eq_ignore_ascii_case(bound));
            // Every other qualifier must be the FAR node's. A match row
            // carries the far node and the edge that reached it, and nothing
            // of the seed, so `a.name` over a pattern seeded at `a` used to
            // read the FAR node's `name` and return it under the seed's
            // label: a wrong answer that looked right. It is refused now,
            // and so is a variable the pattern never bound.
            if let Some(written) = qualifier.as_deref().filter(|_| !edge) {
                if written.eq_ignore_ascii_case(&seed_variable) {
                    return Err(SqlError::unsupported(format!(
                        "COLUMNS ({written}.{field}): `{written}` is the starting node, and a match row carries only the far node `{target_variable}` and the edge it was reached by. The starting row is the one named by its key, so read it with its own SELECT; returning its fields per match is not in this slice"
                    )));
                }
                if !written.eq_ignore_ascii_case(&target_variable) {
                    return Err(SqlError::syntax(
                        format!(
                            "COLUMNS ({written}.{field}): `{written}` is not a variable of this pattern; it binds `{seed_variable}`, `{target_variable}`{}",
                            edge_variable
                                .as_deref()
                                .map(|e| format!(" and the edge `{e}`"))
                                .unwrap_or_default()
                        ),
                        at,
                    ));
                }
            }
            let item = if edge {
                GraphColumn::Edge(field.clone())
            } else if field == super::ID_COLUMN {
                GraphColumn::Node(SelectItem::Id)
            } else if super::is_key_column(&field) {
                GraphColumn::Node(SelectItem::Key)
            } else {
                GraphColumn::Node(SelectItem::Column(field.clone()))
            };
            let alias = if self.eat_word("AS") {
                self.name()?
            } else {
                field
            };
            columns.push((item, alias));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        self.expect(&Tok::RParen)?;
        Ok(GraphTable {
            context,
            seed_collection,
            seed_key,
            hop,
            target_collection,
            node_predicates,
            columns,
        })
    }

    /// `a.b` as BOTH halves, where [`Self::name`] keeps only the last.
    ///
    /// A `GRAPH_TABLE` pattern binds two namespaces -- the far node's fields
    /// and the edge's properties -- and the element variable is the only
    /// thing that tells them apart.
    fn qualified_name(&mut self) -> SqlResult2<(Option<String>, String)> {
        let at = self.here();
        let mut parts = Vec::new();
        loop {
            let part = match self.peek().clone() {
                Tok::Word(word) => {
                    if let Some(error) = self.listed(&word.to_ascii_uppercase()) {
                        return Err(error);
                    }
                    self.bump();
                    word
                }
                Tok::Quoted(word) => {
                    self.bump();
                    word
                }
                other => {
                    return Err(SqlError::syntax(
                        format!("expected a name, found `{}`", other.written()),
                        at,
                    ))
                }
            };
            parts.push(part);
            if !self.eat(&Tok::Dot) {
                break;
            }
        }
        if parts.len() > 2 {
            // Not a schema-qualified table: this grammar is a `GRAPH_TABLE`
            // element name, where the qualifier is the pattern variable and
            // the tail is the field or the edge property.
            return Err(SqlError::syntax(
                format!(
                    "a GRAPH_TABLE element name is `<variable>.<name>` or `<name>`, found {} parts",
                    parts.len()
                ),
                at,
            ));
        }
        let field = parts.pop().expect("at least one name part");
        Ok((parts.pop(), field))
    }

    /// `:label` or `IS label`, both spellings SQL/PGQ allows.
    fn element_label(&mut self) -> SqlResult2<String> {
        if self.eat(&Tok::Colon) || self.eat_word("IS") {
            let label = self.name()?;
            if matches!(self.peek(), Tok::Pipe) {
                return Err(SqlError::Refused {
                    keyword: "label alternation".into(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.3: label alternation `a|b` is a multi-type hop (two ranges per hop); not built in this slice.",
                });
            }
            return Ok(label);
        }
        Err(SqlError::unsupported(
            "an element pattern names its collection (`(a:place)` or `(a IS place)`): a traversal walks one collection's rows",
        ))
    }

    /// One hop, and the variable the edge element bound (if any), which is
    /// what a `COLUMNS` entry and an `ORDER BY` name the edge by.
    fn graph_hop(&mut self) -> SqlResult2<(GraphHop, Option<String>)> {
        let incoming = self.eat(&Tok::BackArrow);
        if !incoming {
            self.expect(&Tok::Minus)?;
        }
        let mut edge_type = None;
        let mut edge_variable = None;
        let mut predicates = Vec::new();
        if self.eat(&Tok::LBracket) {
            if !matches!(self.peek(), Tok::Colon) {
                edge_variable = Some(self.name()?);
            }
            if self.eat(&Tok::Colon) || self.eat_word("IS") {
                let name = self.name()?;
                if matches!(self.peek(), Tok::Pipe) {
                    return Err(SqlError::Refused {
                        keyword: "label alternation".into(),
                        tier: super::Tier::Two,
                        reason: "QL_CONTRACT §4.3: label alternation `a|b` is a multi-type hop (two ranges per hop); not built in this slice.",
                    });
                }
                edge_type = Some(name);
            }
            // The edge element's inline WHERE: the per-hop edge prune of
            // GRAPH_CONTRACT 4.3, read out of the edge's own inline bag as
            // the frontier expands.
            if self.eat_word("WHERE") {
                loop {
                    let (qualifier, property) = self.qualified_name()?;
                    if let Some(qualifier) = qualifier.as_deref() {
                        // An ANONYMOUS edge element bound no variable, so
                        // every qualifier names something else. Swallowing it
                        // would compile `c.born > 1990` into an EDGE
                        // predicate on a property called `born`, which no bag
                        // carries, and the statement would return zero rows
                        // instead of refusing.
                        let Some(bound) = edge_variable.as_deref() else {
                            return Err(SqlError::unsupported(format!(
                                "`{qualifier}.{property}` in an edge element's inline WHERE: this edge element bound no variable, so the predicate belongs to `{qualifier}`'s own element WHERE"
                            )));
                        };
                        if !qualifier.eq_ignore_ascii_case(bound) {
                            return Err(SqlError::unsupported(format!(
                                "an edge element's inline WHERE names its own element (`{bound}`), not `{qualifier}`: a predicate on another element is that element's own inline WHERE"
                            )));
                        }
                    }
                    let op = match self.peek() {
                        Tok::Eq => CmpOp::Eq,
                        Tok::Ne => CmpOp::Ne,
                        Tok::Lt => CmpOp::Lt,
                        Tok::Le => CmpOp::Le,
                        Tok::Gt => CmpOp::Gt,
                        Tok::Ge => CmpOp::Ge,
                        other => {
                            return Err(SqlError::syntax(
                                format!(
                                    "an edge property predicate compares with =, <>, <, <=, > or >=, found `{}`",
                                    other.written()
                                ),
                                self.here(),
                            ))
                        }
                    };
                    self.bump();
                    let value = self.literal()?;
                    predicates.push(EdgePredicate { property, op, value });
                    if self.word().as_deref() == Some("OR") {
                        return Err(SqlError::unsupported(
                            "OR inside an edge element's WHERE: an edge predicate is decoded from the edge's own inline bag per hop, and a union is a set over a keyspace (GRAPH_CONTRACT 4.3)",
                        ));
                    }
                    if !self.eat_word("AND") {
                        break;
                    }
                }
            }
            self.expect(&Tok::RBracket)?;
        }
        // The arrow head decides the direction. After the bracket the text
        // reads `->` (one token) for an outgoing hop and `-` for an
        // undirected one; `<-[..]-` is the incoming form.
        let outgoing = if incoming {
            if !self.eat(&Tok::Minus) {
                self.expect(&Tok::Arrow)?;
            }
            false
        } else if self.eat(&Tok::Arrow) {
            true
        } else {
            self.expect(&Tok::Minus)?;
            self.eat(&Tok::Gt)
        };
        let direction = match (incoming, outgoing) {
            (true, _) => GraphDirection::Incoming,
            (false, true) => GraphDirection::Outgoing,
            (false, false) => GraphDirection::Both,
        };
        let (min_depth, max_depth) = self.quantifier()?;
        Ok((
            GraphHop {
                edge_type,
                direction,
                min_depth,
                max_depth,
                predicates,
            },
            edge_variable,
        ))
    }

    fn quantifier(&mut self) -> SqlResult2<(usize, usize)> {
        if self.eat(&Tok::LBrace) {
            let min = match self.bump() {
                Tok::Num(n, true) => n as usize,
                other => {
                    return Err(SqlError::syntax(
                        format!("expected a depth, found `{}`", other.written()),
                        self.here(),
                    ))
                }
            };
            let max = if self.eat(&Tok::Comma) {
                if matches!(self.peek(), Tok::RBrace) {
                    super::MAX_GRAPH_DEPTH
                } else {
                    match self.bump() {
                        Tok::Num(n, true) => n as usize,
                        other => {
                            return Err(SqlError::syntax(
                                format!("expected a depth, found `{}`", other.written()),
                                self.here(),
                            ))
                        }
                    }
                }
            } else {
                min
            };
            self.expect(&Tok::RBrace)?;
            return Ok((min.max(1), max));
        }
        if self.eat(&Tok::Plus) {
            return Ok((1, super::MAX_GRAPH_DEPTH));
        }
        if self.eat(&Tok::Question) {
            return Ok((1, 1));
        }
        Ok((1, 1))
    }

}
