use super::*;

impl Parser {
    // ── WHERE ────────────────────────────────────────────────────────────

    /// A flat conjunction, which is what a GRAPH_TABLE element's inline
    /// `WHERE` is: per-hop predicates are answered index-side one node at a
    /// time (`GRAPH_CONTRACT` 4.3), and a union over the whole collection is
    /// not a per-hop question. `OR` and `NOT` there are refused, naming that.
    pub(super) fn conjunction(&mut self) -> SqlResult2<Vec<Predicate>> {
        let mut out = Vec::new();
        loop {
            let mut negated = false;
            out.push(self.predicate_negatable(&mut negated)?);
            if negated {
                return Err(SqlError::unsupported(
                    "a negated predicate inside a graph pattern element: a per-hop predicate is answered from one node's postings, and a complement is a set over the whole collection (GRAPH_CONTRACT 4.3)",
                ));
            }
            if self.word().as_deref() == Some("OR") {
                return Err(SqlError::unsupported(
                    "OR inside a graph pattern element: a per-hop predicate is answered from one node's postings, and a union is a set over the whole collection (GRAPH_CONTRACT 4.3)",
                ));
            }
            if !self.eat_word("AND") {
                break;
            }
        }
        Ok(out)
    }

    // ── the boolean tree (docs/lang/QL_CONTRACT.md §3) ────────────────────────
    //
    // `AND` is the conjunction a filter list already is, so EVERY `And` in
    // the tree is flattened into one `Vec<Expr>` and only the shapes that
    // cannot be -- an `Or`, or an `And` under a `Not` -- keep their node.
    // Flattening only the TOP one made redundant parentheses matter: `a AND
    // (b AND ST_Intersects(...))` left a nested conjunction whose geometry
    // leaf `compile_set_expr` then refused, while the identical `a AND b AND
    // ST_Intersects(...)` compiled. Parentheses that change nothing about
    // the meaning now change nothing about the plan.

    pub(super) fn where_clause(&mut self) -> SqlResult2<Vec<Expr>> {
        let mut out = Vec::new();
        flatten_and(self.disjunction()?, &mut out);
        Ok(out)
    }

    fn disjunction(&mut self) -> SqlResult2<Expr> {
        let mut parts = vec![self.boolean_and()?];
        while self.eat_word("OR") {
            parts.push(self.boolean_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap_or(Expr::And(Vec::new()))
        } else {
            Expr::Or(parts)
        })
    }

    fn boolean_and(&mut self) -> SqlResult2<Expr> {
        let mut parts = vec![self.boolean_unary()?];
        while self.eat_word("AND") {
            parts.push(self.boolean_unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap_or(Expr::And(Vec::new()))
        } else {
            Expr::And(parts)
        })
    }

    fn boolean_unary(&mut self) -> SqlResult2<Expr> {
        self.deeper()?;
        let result = self.boolean_unary_inner();
        self.shallower();
        result
    }

    fn boolean_unary_inner(&mut self) -> SqlResult2<Expr> {
        if self.eat_word("NOT") {
            return Ok(Expr::Not(Box::new(self.boolean_unary()?)));
        }
        if matches!(self.peek(), Tok::LParen) {
            self.bump();
            let inner = self.disjunction()?;
            if !self.eat(&Tok::RParen) {
                return Err(SqlError::syntax(
                    format!(
                        "expected `)` to close a WHERE group, found `{}`",
                        self.peek().written()
                    ),
                    self.here(),
                ));
            }
            return Ok(inner);
        }
        if self.word().as_deref() == Some("EXISTS") {
            self.bump();
            return Ok(Expr::Leaf(self.exists_subquery()?));
        }
        let mut negated = false;
        let predicate = self.predicate_negatable(&mut negated)?;
        let leaf = Expr::Leaf(predicate);
        Ok(if negated {
            Expr::Not(Box::new(leaf))
        } else {
            leaf
        })
    }

    /// `EXISTS (SELECT 1 FROM t2 WHERE t2.<column> = <outer>._key)`.
    ///
    /// One shape, because one shape is what the semi-join atomic answers: the
    /// subquery's rows name outer keys through one column, and the set of
    /// outer ids they name is the filter. A correlated subquery over anything
    /// else has no atomic and is refused where it is written.
    fn exists_subquery(&mut self) -> SqlResult2<Predicate> {
        if !self.eat(&Tok::LParen) {
            return Err(SqlError::syntax(
                format!("expected `(` after EXISTS, found `{}`", self.peek().written()),
                self.here(),
            ));
        }
        self.expect_word("SELECT")?;
        // `SELECT 1`, `SELECT *` or a column: the subquery's select list is
        // never read -- EXISTS asks whether a row is there.
        match self.peek() {
            Tok::Star => {
                self.bump();
            }
            Tok::Num(_, _) => {
                self.bump();
            }
            _ => {
                let _ = self.name()?;
            }
        }
        self.expect_word("FROM")?;
        let table = self.name()?;
        self.expect_word("WHERE")?;
        let column = self.name()?;
        if !self.eat(&Tok::Eq) {
            return Err(SqlError::unsupported(
                "an EXISTS subquery whose correlation is not a key equality: the semi-join atomic is a membership set of the outer ids one column names",
            ));
        }
        let outer = self.name()?;
        if !super::is_key_column(&outer) {
            return Err(SqlError::unsupported(
                "an EXISTS subquery correlated on a column other than the outer key: the semi-join atomic maps the subquery's values through the external-key mapping",
            ));
        }
        if !self.eat(&Tok::RParen) {
            return Err(SqlError::syntax(
                format!(
                    "expected `)` to close an EXISTS subquery, found `{}`",
                    self.peek().written()
                ),
                self.here(),
            ));
        }
        Ok(Predicate::Semi { table, column })
    }

    fn predicate_negatable(&mut self, negated: &mut bool) -> SqlResult2<Predicate> {
        self.deeper()?;
        let result = self.predicate_inner(negated);
        self.shallower();
        result
    }

    fn predicate_inner(&mut self, negated: &mut bool) -> SqlResult2<Predicate> {
        // A §4.1 / §4.2 function at the head of a predicate is a RANGE
        // REWRITE, not a row test: the function folds into scalar index
        // bounds at compile time (QL_CONTRACT §4.1, §4.2).
        if let Some(predicate) = self.function_predicate()? {
            return Ok(predicate);
        }
        self.guard_word()?;
        if let Some(word) = self.word() {
            match word.as_str() {
                "ST_DWITHIN" | "ST_INTERSECTS" | "ST_WITHIN" | "ST_CONTAINS" | "ST_COVERS"
                | "ST_CROSSES" => return self.spatial_predicate(),
                "TO_TSVECTOR" => return self.text_predicate(),
                // `search(col, 'query')`, guarded on the parenthesis so a
                // collection whose column is called `search` still means the
                // column.
                "SEARCH" if *self.peek_at(1) == Tok::LParen => {
                    return self.search_predicate()
                }
                _ => {}
            }
        }
        // `1 <> 1`, `1 = 1`: a predicate with no column, folded to its truth
        // value here. pgjdbc writes `WHERE 1<>1 LIMIT 1` to learn a result's
        // columns without fetching a row.
        if let Some(predicate) = self.constant_predicate()? {
            return Ok(predicate);
        }
        let at = self.here();
        let column = self.name()?;
        // `payload->>'status' = 'live'`: the ONE JSON path form that becomes
        // an index range. It is read here rather than refused here because
        // `refuse::TABLE` still lists `->>` and `guard_operator` still
        // refuses it in every other position -- a SELECT list, an ORDER BY, a
        // GROUP BY. `->`, `#>` and `#>>` are not read anywhere: they fall
        // through to the guard below and are refused by name.
        if matches!(self.peek(), Tok::LongArrow) {
            self.bump();
            let member = self.json_member()?;
            let what = format!("{column}->>'{member}'");
            // Only a COMPARISON continues the Tier-1 shape. `->>` standing
            // anywhere else -- a bare boolean leaf, a function argument --
            // is the row function that is not built, and is refused by NAME
            // from the table rather than reported as a missing token.
            if !matches!(
                self.peek(),
                Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
            ) {
                return Err(refuse::refuse("->>"));
            }
            let op = self.comparison(&what)?;
            if op != CmpOp::Eq {
                return Err(SqlError::unsupported(format!(
                    "{what} {} v: the expression index over a JSON member stores the member's TEXT, and QL_CONTRACT §4.1 rewrites the EQUALITY over such an expression to an index equality. An ordering comparison over an extracted value is not that, and there is no atomic that answers it index-side",
                    op.written()
                )));
            }
            let value = self.literal()?;
            return Ok(Predicate::TextFn {
                column,
                shape: TextShape::JsonEq { member, value },
            });
        }
        // A cast on the left of a predicate (`plot::geometry`) is PostGIS's
        // way of choosing the planar overload; the unit semantics here come
        // from the predicate itself, so the cast is read and dropped. A cast
        // to DATE is NOT dropped: `t::date = 'lit'` is one day's range.
        let cast_to_date = self.date_cast()?;
        self.guard_operator()?;
        if cast_to_date {
            let op = self.comparison(&format!("{column}::date"))?;
            return Ok(Predicate::Time {
                column,
                shape: TimeShape::CastDate {
                    op,
                    value: self.literal()?,
                },
            });
        }
        // `col LIKE 'abc%'` is a text-key prefix range over `col`'s own
        // btree; any other pattern needs the trigram family (§3).
        if self.word().as_deref().map(str::to_ascii_uppercase).as_deref() == Some("LIKE") {
            self.bump();
            let value = self.literal()?;
            return Ok(Predicate::TextFn {
                column,
                shape: TextShape::Prefix {
                    value,
                    written: "LIKE",
                },
            });
        }
        if let Some(word) = self.word() {
            match word.as_str() {
                "BETWEEN" => {
                    self.bump();
                    let lower_clock = self.clock_value()?;
                    let lower = match lower_clock {
                        Some(value) => value,
                        None => TimeValue::Lit(self.literal()?),
                    };
                    self.expect_word("AND")?;
                    let upper_clock = self.clock_value()?;
                    let upper = match upper_clock {
                        Some(value) => value,
                        None => TimeValue::Lit(self.literal()?),
                    };
                    // The clock on either side makes this a §4.2 rewrite; two
                    // plain literals stay the Tier-1 Range they always were.
                    if let (TimeValue::Lit(lower), TimeValue::Lit(upper)) = (&lower, &upper) {
                        return Ok(if super::is_key_column(&column) {
                            Predicate::KeyBetween {
                                lower: lower.clone(),
                                upper: upper.clone(),
                            }
                        } else {
                            Predicate::Between {
                                column,
                                lower: lower.clone(),
                                upper: upper.clone(),
                            }
                        });
                    }
                    return Ok(Predicate::Time {
                        column,
                        shape: TimeShape::ClockBetween { lower, upper },
                    });
                }
                "IS" => {
                    self.bump();
                    let inverted = self.eat_word("NOT");
                    if self.eat_word("NULL") {
                        return Ok(Predicate::IsNull {
                            column,
                            negated: inverted,
                        });
                    }
                    if self.eat_word("MISSING") {
                        if inverted {
                            return Err(SqlError::unsupported(
                                "IS NOT MISSING: a row whose field is present and NULL sits on the same nullish index key as a missing one, so the complement of MISSING cannot be proved from postings",
                            ));
                        }
                        return Ok(Predicate::IsMissing { column });
                    }
                    return Err(SqlError::syntax(
                        "expected NULL or MISSING after IS",
                        self.here(),
                    ));
                }
                "IN" => {
                    self.bump();
                    return self.in_predicate(column);
                }
                "NOT" if self.word_at(1).as_deref() == Some("IN") => {
                    self.bump();
                    self.bump();
                    *negated = true;
                    return self.in_predicate(column);
                }
                other => {
                    if let Some(error) = self.listed(other) {
                        return Err(error);
                    }
                }
            }
        }
        let op = match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            Tok::VecCosine | Tok::VecL2 | Tok::VecDot => {
                return Err(SqlError::Refused {
                    keyword: self.peek().written(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.5: a distance as a FILTER (`emb <=> $v < 0.3`) is an ef-bounded approximate membership set. As an ORDER BY the same operator is Tier 1.",
                })
            }
            other => {
                return Err(SqlError::syntax(
                    format!("expected a comparison after `{column}`, found `{}`", other.written()),
                    at,
                ))
            }
        };
        self.bump();
        if let Some(value) = self.clock_value()? {
            return Ok(Predicate::Time {
                column,
                shape: TimeShape::Clock { op, value },
            });
        }
        let value = self.literal()?;
        Ok(if super::is_key_column(&column) {
            Predicate::KeyCompare { op, value }
        } else {
            Predicate::Compare { column, op, value }
        })
    }

    /// `col IN (v1, v2, ...)`, or `_key IN (SELECT t2.<column> FROM t2)`.
    ///
    /// The list is a union of equalities on one index, which is one
    /// membership set; the subquery form is the same semi-join `EXISTS`
    /// writes the other way round.
    fn in_predicate(&mut self, column: String) -> SqlResult2<Predicate> {
        if !self.eat(&Tok::LParen) {
            return Err(SqlError::syntax(
                format!("expected `(` after IN, found `{}`", self.peek().written()),
                self.here(),
            ));
        }
        if self.word().as_deref() == Some("SELECT") {
            if !super::is_key_column(&column) {
                return Err(SqlError::unsupported(
                    "IN (SELECT ...) on a column other than the key: the semi-join atomic maps the subquery's values through the external-key mapping, which only the key column names",
                ));
            }
            self.bump();
            let subject = self.name()?;
            self.expect_word("FROM")?;
            let table = self.name()?;
            if !self.eat(&Tok::RParen) {
                return Err(SqlError::syntax(
                    format!(
                        "expected `)` to close IN (SELECT ...), found `{}`",
                        self.peek().written()
                    ),
                    self.here(),
                ));
            }
            return Ok(Predicate::Semi {
                table,
                column: subject,
            });
        }
        let mut values = Vec::new();
        loop {
            values.push(self.literal()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if !self.eat(&Tok::RParen) {
            return Err(SqlError::syntax(
                format!("expected `)` to close IN, found `{}`", self.peek().written()),
                self.here(),
            ));
        }
        if values.is_empty() {
            return Err(SqlError::syntax("IN needs at least one value", self.here()));
        }
        Ok(if super::is_key_column(&column) {
            Predicate::KeyInList { values }
        } else {
            Predicate::InList { column, values }
        })
    }
    /// Read the casts on the left of a predicate, reporting whether the last
    /// one was `::date`.
    fn date_cast(&mut self) -> SqlResult2<bool> {
        let mut to_date = false;
        while matches!(self.peek(), Tok::Cast) {
            let at = self.here();
            self.bump();
            let Some(word) = self.word() else {
                return Err(SqlError::syntax("expected a type after `::`", at));
            };
            self.bump();
            to_date = false;
            match word.to_ascii_uppercase().as_str() {
                "DATE" => to_date = true,
                "GEOGRAPHY" | "GEOMETRY" | "VECTOR" | "TEXT" | "FLOAT8" | "INT" | "INTEGER"
                | "BIGINT" | "REAL" | "TIMESTAMPTZ" | "TIMESTAMP" => {}
                "DOUBLE" => self.expect_word("PRECISION")?,
                other => {
                    return Err(SqlError::unsupported(format!(
                        "cast `::{other}` has no Tier-1 meaning here"
                    )))
                }
            }
        }
        Ok(to_date)
    }

    fn text_predicate(&mut self) -> SqlResult2<Predicate> {
        let column = self.tsvector()?;
        if !self.eat(&Tok::Matches) {
            return Err(SqlError::syntax(
                format!(
                    "expected `@@` after to_tsvector, found `{}`",
                    self.peek().written()
                ),
                self.here(),
            ));
        }
        let query = self.tsquery()?;
        Ok(Predicate::Text { column, query })
    }

    /// `to_tsvector('simple', col)` -- one declared field, because a text
    /// index spans one (battle50k deviation 1).
    fn tsvector(&mut self) -> SqlResult2<String> {
        self.expect_word("TO_TSVECTOR")?;
        self.expect(&Tok::LParen)?;
        self.simple_config()?;
        self.expect(&Tok::Comma)?;
        let column = self.name()?;
        if matches!(self.peek(), Tok::Concat) {
            return Err(refuse::refuse("||"));
        }
        self.expect(&Tok::RParen)?;
        Ok(column)
    }

    fn tsquery(&mut self) -> SqlResult2<TsQuery> {
        self.expect_word("TO_TSQUERY")?;
        self.expect(&Tok::LParen)?;
        self.simple_config()?;
        self.expect(&Tok::Comma)?;
        let source = self.literal()?;
        self.expect(&Tok::RParen)?;
        Ok(TsQuery {
            source,
            tsquery_syntax: true,
            fuzzy: false,
        })
    }

    /// `search(col, 'query')` -- the typo-tolerant predicate of
    /// `docs/lang/QL_CONTRACT.md` §4.6. No configuration argument: analyzer
    /// v1 is language-neutral and `'simple'` is the only configuration this
    /// engine has, so a second argument would be a knob that changes nothing.
    fn search_predicate(&mut self) -> SqlResult2<Predicate> {
        self.expect_word("SEARCH")?;
        self.expect(&Tok::LParen)?;
        let column = self.name()?;
        self.expect(&Tok::Comma)?;
        let source = self.literal()?;
        self.expect(&Tok::RParen)?;
        Ok(Predicate::Text {
            column,
            query: TsQuery {
                source,
                tsquery_syntax: false,
                fuzzy: true,
            },
        })
    }

    fn spatial_predicate(&mut self) -> SqlResult2<Predicate> {
        let name = self.word().expect("caller checked the word");
        let at = self.here();
        self.bump();
        let predicate = match name.as_str() {
            "ST_DWITHIN" => SpatialPredicate::DWithin,
            "ST_INTERSECTS" => SpatialPredicate::Intersects,
            "ST_WITHIN" => SpatialPredicate::Within,
            "ST_CONTAINS" => SpatialPredicate::Contains,
            other => {
                return Err(SqlError::Refused {
                    keyword: other.to_owned(),
                    tier: super::Tier::Two,
                    reason: "QL_CONTRACT §4.4: ST_Covers and ST_Crosses are Tier 1 as ROW functions; as index-side predicates the filter atomics are Intersects, Within, Contains and DWithin.",
                })
            }
        };
        self.expect(&Tok::LParen)?;
        let column = self.name()?;
        self.optional_cast()?;
        self.expect(&Tok::Comma)?;
        let argument = self.geo_argument()?;
        let metres = if predicate == SpatialPredicate::DWithin {
            self.expect(&Tok::Comma)?;
            let metres = self.literal()?;
            // PostGIS's fourth argument chooses the spheroid; E4's radius is
            // spheroidal and has no planar twin.
            if self.eat(&Tok::Comma) {
                match self.word().as_deref() {
                    Some("TRUE") => {
                        self.bump();
                    }
                    Some("FALSE") => {
                        return Err(SqlError::unsupported(
                            "ST_DWithin(..., false): use_spheroid = false is a planar distance; PointFilter::Radius and GeometryFilter::DWithin are spheroidal (docs/core/SPATIAL_FUNCTIONS.md) and there is no planar-distance atomic",
                        ))
                    }
                    _ => {
                        return Err(SqlError::syntax(
                            "ST_DWithin's fourth argument is use_spheroid",
                            self.here(),
                        ))
                    }
                }
            }
            Some(metres)
        } else {
            None
        };
        self.expect(&Tok::RParen)?;
        let _ = at;
        Ok(Predicate::Spatial {
            predicate,
            column,
            argument,
            metres,
        })
    }

    /// `::geography` / `::geometry` / `::vector`, read and dropped: the unit
    /// semantics of a predicate here come from the predicate, not the cast
    /// (`GeometryFilter`'s own documentation, `src/query/mod.rs`).
    fn optional_cast(&mut self) -> SqlResult2<()> {
        while self.eat(&Tok::Cast) {
            let at = self.here();
            let Some(word) = self.word() else {
                return Err(SqlError::syntax("expected a type after `::`", at));
            };
            self.bump();
            match word.as_str() {
                "GEOGRAPHY" | "GEOMETRY" | "VECTOR" | "TEXT" | "FLOAT8" | "DOUBLE" | "INT"
                | "INTEGER" | "BIGINT" | "REAL" => {}
                other => {
                    return Err(SqlError::unsupported(format!(
                        "cast `::{other}` has no Tier-1 meaning here"
                    )))
                }
            }
            if word == "DOUBLE" {
                self.expect_word("PRECISION")?;
            }
        }
        Ok(())
    }

    fn geo_argument(&mut self) -> SqlResult2<GeoArg> {
        self.deeper()?;
        let result = self.geo_argument_inner();
        self.shallower();
        result
    }

    fn geo_argument_inner(&mut self) -> SqlResult2<GeoArg> {
        self.guard_word()?;
        let argument = match self.word().as_deref() {
            Some("ST_SETSRID") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let inner = self.geo_argument()?;
                self.expect(&Tok::Comma)?;
                match self.bump() {
                    Tok::Num(n, _) if n == 4326.0 => {}
                    other => {
                        return Err(SqlError::unsupported(format!(
                            "ST_SetSRID(..., {}): storage is WGS84; ST_Transform is Tier 2",
                            other.written()
                        )))
                    }
                }
                self.expect(&Tok::RParen)?;
                inner
            }
            Some("ST_MAKEPOINT") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let lon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let lat = self.literal()?;
                self.expect(&Tok::RParen)?;
                GeoArg::Point(PointArg { lon, lat })
            }
            Some("ST_POINT") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let lon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let lat = self.literal()?;
                self.expect(&Tok::RParen)?;
                GeoArg::Point(PointArg { lon, lat })
            }
            Some("ST_MAKEENVELOPE") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let minlon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let minlat = self.literal()?;
                self.expect(&Tok::Comma)?;
                let maxlon = self.literal()?;
                self.expect(&Tok::Comma)?;
                let maxlat = self.literal()?;
                if self.eat(&Tok::Comma) {
                    match self.bump() {
                        Tok::Num(n, _) if n == 4326.0 => {}
                        other => {
                            return Err(SqlError::unsupported(format!(
                                "ST_MakeEnvelope(..., {}): storage is WGS84",
                                other.written()
                            )))
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                GeoArg::Envelope {
                    minlon,
                    minlat,
                    maxlon,
                    maxlat,
                }
            }
            Some("ST_GEOMFROMGEOJSON") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let json = self.literal()?;
                self.expect(&Tok::RParen)?;
                GeoArg::GeoJson(json)
            }
            _ => GeoArg::GeoJson(self.literal()?),
        };
        self.optional_cast()?;
        Ok(argument)
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    pub(super) fn order_key(&mut self) -> SqlResult2<OrderKey> {
        let expression = self.expression(0)?;
        let descending = if self.eat_word("DESC") {
            true
        } else {
            let _ = self.eat_word("ASC");
            false
        };
        if self.word().as_deref() == Some("NULLS") {
            return Err(SqlError::unsupported(
                "NULLS FIRST / NULLS LAST: `compare_rank` fixes where a nullish key sorts, and it is not a per-query choice",
            ));
        }
        Ok(match expression {
            PExpr::Column(column) => OrderKey::Column { column, descending },
            PExpr::Bm25 { column, query } => OrderKey::Bm25 {
                column,
                query,
                descending,
            },
            PExpr::Distance { column, right, op } => match *right {
                PExpr::Geo(GeoArg::Point(point)) => {
                    if op != VecOp::L2 {
                        return Err(SqlError::unsupported(
                            "the PostGIS KNN operator is `<->`; `<=>` and `<#>` are pgvector's and take a vector",
                        ));
                    }
                    OrderKey::Distance {
                        column,
                        point,
                        descending,
                    }
                }
                PExpr::Geo(_) => {
                    return Err(SqlError::unsupported(
                        "ORDER BY <-> takes a point: the nearest walk starts at one centre",
                    ))
                }
                PExpr::Param(n) => OrderKey::Vector {
                    column,
                    query: Literal::Param(n),
                    op,
                    descending,
                },
                PExpr::Str(text) => OrderKey::Vector {
                    column,
                    query: Literal::Str(text),
                    op,
                    descending,
                },
                other => {
                    return Err(SqlError::unsupported(format!(
                        "the right side of a distance operator is a vector literal, a parameter or a point; found {other:?}"
                    )))
                }
            },
            other => OrderKey::Score {
                expr: lower(other)?,
                descending,
            },
        })
    }

    /// A precedence-climbing expression parser. Level 0 is `+`/`-`, level 1
    /// `*`/`/`, level 2 the distance operators, level 3 a primary.
    pub(super) fn expression(&mut self, level: usize) -> SqlResult2<PExpr> {
        self.deeper()?;
        let result = self.expression_inner(level);
        self.shallower();
        result
    }

    fn expression_inner(&mut self, level: usize) -> SqlResult2<PExpr> {
        if level >= 3 {
            return self.primary();
        }
        let mut left = self.expression(level + 1)?;
        loop {
            self.guard_operator()?;
            let node = match (level, self.peek()) {
                (0, Tok::Plus) => {
                    self.bump();
                    PExpr::Add(Box::new(left), Box::new(self.expression(1)?))
                }
                (0, Tok::Minus) => {
                    self.bump();
                    PExpr::Sub(Box::new(left), Box::new(self.expression(1)?))
                }
                (1, Tok::Star) => {
                    self.bump();
                    PExpr::Mul(Box::new(left), Box::new(self.expression(2)?))
                }
                (1, Tok::Slash) => {
                    self.bump();
                    PExpr::Div(Box::new(left), Box::new(self.expression(2)?))
                }
                (2, Tok::VecCosine | Tok::VecL2 | Tok::VecDot) => {
                    let op = match self.peek() {
                        Tok::VecCosine => VecOp::Cosine,
                        Tok::VecL2 => VecOp::L2,
                        _ => VecOp::NegativeDot,
                    };
                    self.bump();
                    let PExpr::Column(column) = left else {
                        return Err(SqlError::unsupported(
                            "a distance operator takes an indexed column on its left: the index is what the walk reads",
                        ));
                    };
                    let right = if matches!(
                        self.word().as_deref(),
                        Some("ST_SETSRID") | Some("ST_MAKEPOINT") | Some("ST_POINT")
                    ) {
                        PExpr::Geo(self.geo_argument()?)
                    } else {
                        self.expression(3)?
                    };
                    PExpr::Distance {
                        column,
                        right: Box::new(right),
                        op,
                    }
                }
                _ => return Ok(left),
            };
            left = node;
        }
    }

    fn primary(&mut self) -> SqlResult2<PExpr> {
        self.guard_operator()?;
        if self.eat(&Tok::Minus) {
            return Ok(PExpr::Neg(Box::new(self.expression(2)?)));
        }
        if self.eat(&Tok::Plus) {
            return self.expression(2);
        }
        if self.eat(&Tok::LParen) {
            let inner = self.expression(0)?;
            self.expect(&Tok::RParen)?;
            self.optional_cast()?;
            return Ok(inner);
        }
        let at = self.here();
        match self.peek().clone() {
            Tok::Num(value, _) => {
                self.bump();
                Ok(PExpr::Num(value))
            }
            Tok::Str(text) => {
                self.bump();
                self.optional_cast()?;
                Ok(PExpr::Str(text))
            }
            Tok::Param(n) => {
                self.bump();
                self.optional_cast()?;
                Ok(PExpr::Param(n))
            }
            Tok::Word(_) | Tok::Quoted(_) => {
                self.guard_word()?;
                match self.word().as_deref() {
                    Some("TS_RANK_CD") | Some("TS_RANK") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        let column = self.tsvector()?;
                        self.expect(&Tok::Comma)?;
                        let query = self.tsquery()?;
                        // ts_rank_cd's optional normalisation argument picks
                        // a document-length correction Postgres applies to
                        // its own formula; BM25 has its own (QL_CONTRACT §5
                        // deviation 5) and cannot be steered by it.
                        if self.eat(&Tok::Comma) {
                            return Err(SqlError::unsupported(
                                "ts_rank_cd's normalisation argument: the ranking here is BM25 (QL_CONTRACT §5 deviation 5) and has no ts_rank normalisation knob",
                            ));
                        }
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::Bm25 { column, query })
                    }
                    // `search_score()`: no arguments, because the
                    // statement's own `search()` predicate is what it scores.
                    Some("SEARCH_SCORE") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::SearchScore)
                    }
                    Some("BM25") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        let column = self.name()?;
                        self.expect(&Tok::Comma)?;
                        let source = self.literal()?;
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::Bm25 {
                            column,
                            query: TsQuery {
                                source,
                                tsquery_syntax: false,
                                fuzzy: false,
                            },
                        })
                    }
                    Some("ST_DISTANCE") => {
                        self.bump();
                        self.expect(&Tok::LParen)?;
                        let column = self.name()?;
                        self.optional_cast()?;
                        self.expect(&Tok::Comma)?;
                        let point = match self.geo_argument()? {
                            GeoArg::Point(point) => point,
                            _ => {
                                return Err(SqlError::unsupported(
                                    "ST_Distance in a ranking takes a point: `ScoreExpr::Distance` is geodesic metres from one centre",
                                ))
                            }
                        };
                        self.expect(&Tok::RParen)?;
                        Ok(PExpr::StDistance { column, point })
                    }
                    Some("ST_SETSRID") | Some("ST_MAKEPOINT") | Some("ST_POINT")
                    | Some("ST_MAKEENVELOPE") | Some("ST_GEOMFROMGEOJSON") => {
                        Ok(PExpr::Geo(self.geo_argument()?))
                    }
                    Some("TRUE") => {
                        self.bump();
                        Ok(PExpr::Num(1.0))
                    }
                    Some("FALSE") => {
                        self.bump();
                        Ok(PExpr::Num(0.0))
                    }
                    _ => {
                        if matches!(self.peek_at(1), Tok::LParen) {
                            let name = self.word().unwrap_or_default();
                            return Err(match self.listed(&name) {
                                Some(error) => error,
                                None => SqlError::unsupported(format!(
                                    "function `{name}` is not in QL_CONTRACT §4"
                                )),
                            });
                        }
                        let name = self.name()?;
                        self.optional_cast()?;
                        Ok(PExpr::Column(name))
                    }
                }
            }
            other => Err(SqlError::syntax(
                format!("expected a value, found `{}`", other.written()),
                at,
            )),
        }
    }

    // ── §4.1 string and §4.2 date/time functions ─────────────────────────

    /// True when the SELECT-list item that starts at the cursor holds a
    /// §4.1 / §4.2 function, a `||` or a cast anywhere inside it.
    ///
    /// The lookahead runs to the item's own terminator -- a comma at depth
    /// zero, `AS`, or `FROM` -- so it cannot reach into the next item or into
    /// the rest of the statement. A ranking expression
    /// (`1 - (emb <=> $v)`) holds none of these tokens and keeps its own
    /// path, which is the Score atomic.
    pub(super) fn row_expression_ahead(&self) -> bool {
        let mut depth = 0usize;
        let mut at = 0usize;
        loop {
            let token = self.peek_at(at);
            match token {
                Tok::Eof => return false,
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                Tok::Comma if depth == 0 => return false,
                Tok::Concat | Tok::Cast => return true,
                Tok::Word(word) if depth == 0 => {
                    let upper = word.to_ascii_uppercase();
                    if matches!(upper.as_str(), "AS" | "FROM") {
                        return false;
                    }
                    if self.row_function_at(&upper, at) {
                        return true;
                    }
                }
                Tok::Word(word) => {
                    let upper = word.to_ascii_uppercase();
                    if self.row_function_at(&upper, at) {
                        return true;
                    }
                }
                _ => {}
            }
            at += 1;
        }
    }

    /// True when the word `ahead` tokens along is a §4.1 / §4.2 function
    /// CALL rather than a column that happens to share its name.
    ///
    /// The distinction is the parenthesis. `position` and `left` and `right`
    /// are function names AND perfectly ordinary column names -- the catalog
    /// view `db_columns` has a `position` column -- so a bare word is a
    /// column and `word(` is a call. The exceptions are the three §4.2 forms
    /// the standard spells WITHOUT parentheses: `current_date`,
    /// `current_timestamp` and `interval '1 day'`.
    fn row_function_at(&self, upper: &str, ahead: usize) -> bool {
        if matches!(upper, "CURRENT_DATE" | "CURRENT_TIMESTAMP" | "INTERVAL") {
            return true;
        }
        ROW_FUNCTIONS.contains(&upper) && matches!(self.peek_at(ahead + 1), Tok::LParen)
    }

    /// True when the cursor stands on a §4.1 / §4.2 function CALL.
    pub(super) fn at_row_function(&self) -> bool {
        let Some(word) = self.word().map(|w| w.to_ascii_uppercase()) else {
            return false;
        };
        if matches!(word.as_str(), "CURRENT_DATE" | "CURRENT_TIMESTAMP") {
            return true;
        }
        ROW_FUNCTIONS.contains(&word.as_str()) && matches!(self.peek_at(1), Tok::LParen)
    }

    /// The unit `EXTRACT(<unit> FROM t)` names, written bare.
    fn extract_unit(&mut self) -> SqlResult2<TimeUnit> {
        let at = self.here();
        let word = match self.peek().clone() {
            Tok::Word(word) => word,
            Tok::Str(text) => text,
            other => {
                return Err(SqlError::syntax(
                    format!("expected an EXTRACT field, found `{}`", other.written()),
                    at,
                ))
            }
        };
        self.bump();
        TimeUnit::parse(&word).ok_or_else(|| {
            SqlError::unsupported(format!(
                "EXTRACT({word} FROM t): QL_CONTRACT §4.2 names YEAR, MONTH, DAY, DOW, HOUR, MINUTE, SECOND and EPOCH"
            ))
        })
    }

    /// `interval '<n> <unit>'`, folded to microseconds at parse.
    fn interval_micros(&mut self) -> SqlResult2<i64> {
        self.expect_word("INTERVAL")?;
        let at = self.here();
        match self.bump() {
            Tok::Str(text) => functions::parse_interval(&text),
            other => Err(SqlError::syntax(
                format!("interval takes a quoted magnitude, found `{}`", other.written()),
                at,
            )),
        }
    }

    /// `now()` / `current_date` / `current_timestamp`, optionally `+` or `-`
    /// an interval. `None` when the cursor is not on the clock.
    fn clock_value(&mut self) -> SqlResult2<Option<TimeValue>> {
        let word = self.word().map(|w| w.to_ascii_uppercase());
        let date_only = match word.as_deref() {
            Some("NOW") => {
                self.bump();
                self.expect(&Tok::LParen)?;
                self.expect(&Tok::RParen)?;
                false
            }
            Some("CURRENT_TIMESTAMP") => {
                self.bump();
                if self.eat(&Tok::LParen) {
                    self.expect(&Tok::RParen)?;
                }
                false
            }
            Some("CURRENT_DATE") => {
                self.bump();
                true
            }
            _ => return Ok(None),
        };
        let mut offset = 0i64;
        loop {
            let sign = if self.eat(&Tok::Minus) {
                -1
            } else if self.eat(&Tok::Plus) {
                1
            } else {
                break;
            };
            offset += sign * self.interval_micros()?;
        }
        Ok(Some(TimeValue::Clock { date_only, offset }))
    }

    /// A `WHERE` predicate whose head is a §4.1 / §4.2 function call, or
    /// `None` when the cursor is not on one.
    ///
    /// Every shape here folds into scalar index RANGES at compile time; the
    /// function is never evaluated per candidate. `compile.rs` is what
    /// decides whether the pre-image is ONE range (accepted) or a SET of them
    /// (refused while the membership union `OR` compiles to is unbuilt).
    fn function_predicate(&mut self) -> SqlResult2<Option<Predicate>> {
        let Some(word) = self.word().map(|w| w.to_ascii_uppercase()) else {
            return Ok(None);
        };
        match word.as_str() {
            "EXTRACT" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let unit = self.extract_unit()?;
                self.expect_word("FROM")?;
                let column = self.name()?;
                self.expect(&Tok::RParen)?;
                let shape = if self.eat_word("BETWEEN") {
                    let lower = self.literal()?;
                    self.expect_word("AND")?;
                    let upper = self.literal()?;
                    TimeShape::ExtractBetween { unit, lower, upper }
                } else {
                    let op = self.comparison(&format!("EXTRACT({} FROM {column})", unit.written()))?;
                    TimeShape::Extract {
                        unit,
                        op,
                        value: self.literal()?,
                    }
                };
                Ok(Some(Predicate::Time { column, shape }))
            }
            "DATE_TRUNC" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let at = self.here();
                let unit = match self.bump() {
                    Tok::Str(text) => TimeUnit::parse(&text).ok_or_else(|| {
                        SqlError::unsupported(format!(
                            "date_trunc('{text}', t): QL_CONTRACT §4.2 names year, month, day, hour, minute and second"
                        ))
                    })?,
                    other => {
                        return Err(SqlError::syntax(
                            format!("date_trunc takes a quoted unit, found `{}`", other.written()),
                            at,
                        ))
                    }
                };
                if !unit.truncates() {
                    return Err(SqlError::unsupported(format!(
                        "date_trunc('{}', t): `{}` is an EXTRACT field, not a truncation unit",
                        unit.written(),
                        unit.written()
                    )));
                }
                self.expect(&Tok::Comma)?;
                let column = self.name()?;
                self.expect(&Tok::RParen)?;
                let shape = if self.eat_word("BETWEEN") {
                    let lower = self.literal()?;
                    self.expect_word("AND")?;
                    let upper = self.literal()?;
                    TimeShape::TruncBetween { unit, lower, upper }
                } else {
                    let op =
                        self.comparison(&format!("date_trunc('{}', {column})", unit.written()))?;
                    TimeShape::Trunc {
                        unit,
                        op,
                        value: self.literal()?,
                    }
                };
                Ok(Some(Predicate::Time { column, shape }))
            }
            "LOWER" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let column = self.name()?;
                self.expect(&Tok::RParen)?;
                if self.eat_word("LIKE") {
                    let value = self.literal()?;
                    return Ok(Some(Predicate::TextFn {
                        column,
                        shape: TextShape::LowerPrefix {
                            value,
                            written: "LIKE",
                        },
                    }));
                }
                let op = self.comparison(&format!("lower({column})"))?;
                if op != CmpOp::Eq {
                    return Err(SqlError::unsupported(format!(
                        "lower({column}) {} v: QL_CONTRACT §4.1 rewrites lower(col) = v to an index EQUALITY and lower(col) LIKE 'v%' to a prefix range; an ordering comparison over a folded value is neither",
                        op.written()
                    )));
                }
                Ok(Some(Predicate::TextFn {
                    column,
                    shape: TextShape::LowerEq {
                        value: self.literal()?,
                    },
                }))
            }
            "STARTS_WITH" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let lowered = self.eat_word("LOWER");
                if lowered {
                    self.expect(&Tok::LParen)?;
                }
                let column = self.name()?;
                if lowered {
                    self.expect(&Tok::RParen)?;
                }
                self.expect(&Tok::Comma)?;
                let value = self.literal()?;
                self.expect(&Tok::RParen)?;
                Ok(Some(Predicate::TextFn {
                    column,
                    shape: if lowered {
                        TextShape::LowerPrefix {
                            value,
                            written: "starts_with",
                        }
                    } else {
                        TextShape::Prefix {
                            value,
                            written: "starts_with",
                        }
                    },
                }))
            }
            _ => Ok(None),
        }
    }

    /// The comparison operator after a function call, with the call named in
    /// the error when there is none.
    /// `<number> <cmp> <number>` -- a predicate that names no column --
    /// folded to its truth value. `None` when the cursor is not on one.
    fn constant_predicate(&mut self) -> SqlResult2<Option<Predicate>> {
        let Tok::Num(left, _) = self.peek().clone() else {
            return Ok(None);
        };
        if !matches!(
            self.peek_at(1),
            Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
        ) {
            return Ok(None);
        }
        self.bump();
        let op = self.comparison("a constant")?;
        let Tok::Num(right, _) = self.bump() else {
            return Err(SqlError::unsupported(
                "a predicate with no column compares two NUMBERS: that is the `WHERE 1<>1` a driver writes to read a result's columns without a row",
            ));
        };
        Ok(Some(Predicate::Constant(match op {
            CmpOp::Eq => left == right,
            CmpOp::Ne => left != right,
            CmpOp::Lt => left < right,
            CmpOp::Le => left <= right,
            CmpOp::Gt => left > right,
            CmpOp::Ge => left >= right,
        })))
    }

    fn comparison(&mut self, what: &str) -> SqlResult2<CmpOp> {
        let at = self.here();
        let op = match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            other => {
                return Err(SqlError::syntax(
                    format!("expected a comparison after `{what}`, found `{}`", other.written()),
                    at,
                ))
            }
        };
        self.bump();
        Ok(op)
    }

    /// A ROW expression: `a + b`, `a - b`, `a || b` over the §4.1 / §4.2
    /// function set. One row in, one value out.
    pub(super) fn row_expr(&mut self) -> SqlResult2<RowExpr> {
        self.deeper()?;
        let result = self.row_expr_inner();
        self.shallower();
        result
    }

    fn row_expr_inner(&mut self) -> SqlResult2<RowExpr> {
        let mut left = self.row_atom()?;
        loop {
            left = match self.peek() {
                Tok::Plus => {
                    self.bump();
                    RowExpr::Add(Box::new(left), Box::new(self.row_atom()?))
                }
                Tok::Minus => {
                    self.bump();
                    RowExpr::Sub(Box::new(left), Box::new(self.row_atom()?))
                }
                Tok::Concat => {
                    self.bump();
                    RowExpr::Concat(Box::new(left), Box::new(self.row_atom()?))
                }
                _ => return Ok(left),
            };
        }
    }

    /// `::date` and `::text` change the value; every other cast this grammar
    /// already read and dropped keeps doing so.
    fn row_casts(&mut self, mut expr: RowExpr) -> SqlResult2<RowExpr> {
        while matches!(self.peek(), Tok::Cast) {
            let at = self.here();
            self.bump();
            let Some(word) = self.word() else {
                return Err(SqlError::syntax("expected a type after `::`", at));
            };
            self.bump();
            expr = match word.to_ascii_uppercase().as_str() {
                "DATE" => RowExpr::CastDate(Box::new(expr)),
                "TEXT" | "VARCHAR" => RowExpr::CastText(Box::new(expr)),
                "TIMESTAMPTZ" | "TIMESTAMP" => expr,
                "INT" | "INTEGER" | "BIGINT" | "REAL" | "FLOAT8" => expr,
                "DOUBLE" => {
                    self.expect_word("PRECISION")?;
                    expr
                }
                other => {
                    return Err(SqlError::unsupported(format!(
                        "cast `::{other}` has no Tier-1 meaning in a row expression"
                    )))
                }
            };
        }
        Ok(expr)
    }

    fn row_atom(&mut self) -> SqlResult2<RowExpr> {
        if self.eat(&Tok::LParen) {
            let inner = self.row_expr()?;
            self.expect(&Tok::RParen)?;
            return self.row_casts(inner);
        }
        let at = self.here();
        if matches!(self.peek(), Tok::Num(_, _) | Tok::Str(_) | Tok::Param(_) | Tok::Minus) {
            let literal = self.literal_no_cast()?;
            return self.row_casts(RowExpr::Lit(literal));
        }
        let word = match self.word() {
            Some(word) => word.to_ascii_uppercase(),
            None => {
                return Err(SqlError::syntax(
                    format!("expected a value, found `{}`", self.peek().written()),
                    at,
                ))
            }
        };
        if word == "INTERVAL" {
            return Ok(RowExpr::Interval(self.interval_micros()?));
        }
        if let Some(TimeValue::Clock { date_only, offset }) = self.clock_value()? {
            let clock = if date_only {
                RowExpr::CurrentDate
            } else {
                RowExpr::Now
            };
            let expr = if offset == 0 {
                clock
            } else {
                RowExpr::Add(Box::new(clock), Box::new(RowExpr::Interval(offset)))
            };
            return self.row_casts(expr);
        }
        let expr = match word.as_str() {
            "EXTRACT" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let unit = self.extract_unit()?;
                self.expect_word("FROM")?;
                let arg = self.row_expr()?;
                self.expect(&Tok::RParen)?;
                RowExpr::Extract {
                    unit,
                    arg: Box::new(arg),
                }
            }
            "DATE_TRUNC" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let unit_at = self.here();
                let unit = match self.bump() {
                    Tok::Str(text) => TimeUnit::parse(&text)
                        .filter(|unit| unit.truncates())
                        .ok_or_else(|| {
                            SqlError::unsupported(format!(
                                "date_trunc('{text}', t): QL_CONTRACT §4.2 names year, month, day, hour, minute and second"
                            ))
                        })?,
                    other => {
                        return Err(SqlError::syntax(
                            format!("date_trunc takes a quoted unit, found `{}`", other.written()),
                            unit_at,
                        ))
                    }
                };
                self.expect(&Tok::Comma)?;
                let arg = self.row_expr()?;
                self.expect(&Tok::RParen)?;
                RowExpr::Trunc {
                    unit,
                    arg: Box::new(arg),
                }
            }
            "AGE" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let left = self.row_expr()?;
                let right = if self.eat(&Tok::Comma) {
                    Some(Box::new(self.row_expr()?))
                } else {
                    None
                };
                self.expect(&Tok::RParen)?;
                RowExpr::Age {
                    left: Box::new(left),
                    right,
                }
            }
            "TO_CHAR" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let arg = self.row_expr()?;
                self.expect(&Tok::Comma)?;
                let format_at = self.here();
                let format = match self.bump() {
                    Tok::Str(text) => text,
                    other => {
                        return Err(SqlError::syntax(
                            format!("to_char takes a quoted template, found `{}`", other.written()),
                            format_at,
                        ))
                    }
                };
                self.expect(&Tok::RParen)?;
                RowExpr::ToChar {
                    arg: Box::new(arg),
                    format,
                }
            }
            "TO_TIMESTAMP" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let arg = self.row_expr()?;
                self.expect(&Tok::RParen)?;
                RowExpr::ToTimestamp(Box::new(arg))
            }
            "TO_DATE" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let arg = self.row_expr()?;
                if self.eat(&Tok::Comma) {
                    let at = self.here();
                    match self.bump() {
                        // The template is read and checked against the one
                        // form this slice parses; a different template would
                        // silently mean a different literal grammar.
                        Tok::Str(text) if text == "YYYY-MM-DD" => {}
                        other => {
                            return Err(SqlError::unsupported(format!(
                                "to_date(t, {}): the literal grammar QL_CONTRACT §4.2 accepts is ISO-8601, so the only template is 'YYYY-MM-DD'",
                                other.written()
                            )))
                            .map_err(|e: SqlError| {
                                let _ = at;
                                e
                            })
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                RowExpr::ToDate(Box::new(arg))
            }
            "POSITION" | "STRPOS" => {
                let strpos = word == "STRPOS";
                self.bump();
                self.expect(&Tok::LParen)?;
                let first = self.row_expr()?;
                let second = if strpos {
                    self.expect(&Tok::Comma)?;
                    self.row_expr()?
                } else {
                    self.expect_word("IN")?;
                    self.row_expr()?
                };
                self.expect(&Tok::RParen)?;
                // `position(sub IN s)` and `strpos(s, sub)` take their two
                // arguments in opposite orders, which is Postgres's own shape.
                let (haystack, needle) = if strpos {
                    (first, second)
                } else {
                    (second, first)
                };
                RowExpr::Str {
                    func: StrFunc::Position,
                    args: vec![haystack, needle],
                }
            }
            "SUBSTRING" | "SUBSTR" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let mut args = vec![self.row_expr()?];
                if self.eat_word("FROM") {
                    args.push(self.row_expr()?);
                    if self.eat_word("FOR") {
                        args.push(self.row_expr()?);
                    }
                } else {
                    self.expect(&Tok::Comma)?;
                    args.push(self.row_expr()?);
                    if self.eat(&Tok::Comma) {
                        args.push(self.row_expr()?);
                    }
                }
                self.expect(&Tok::RParen)?;
                RowExpr::Str {
                    func: StrFunc::Substring,
                    args,
                }
            }
            "TRIM" | "BTRIM" => {
                self.bump();
                self.expect(&Tok::LParen)?;
                // `trim(BOTH ' ' FROM s)` picks a side and a fill character;
                // this slice trims ASCII/Unicode whitespace from both ends,
                // which is `btrim(s)`.
                for side in ["BOTH", "LEADING", "TRAILING"] {
                    if self.word().as_deref() == Some(side) {
                        return Err(SqlError::unsupported(format!(
                            "trim({side} ... FROM s): QL_CONTRACT §4.1 names `trim`, which is btrim -- whitespace off both ends"
                        )));
                    }
                }
                let arg = self.row_expr()?;
                self.expect(&Tok::RParen)?;
                RowExpr::Str {
                    func: StrFunc::Trim,
                    args: vec![arg],
                }
            }
            other => {
                let func = match other {
                    "LOWER" => StrFunc::Lower,
                    "UPPER" => StrFunc::Upper,
                    "LENGTH" | "CHAR_LENGTH" => StrFunc::Length,
                    "CONCAT" => StrFunc::Concat,
                    "LEFT" => StrFunc::Left,
                    "RIGHT" => StrFunc::Right,
                    "SPLIT_PART" => StrFunc::SplitPart,
                    "REPLACE" => StrFunc::Replace,
                    "STARTS_WITH" => StrFunc::StartsWith,
                    _ => {
                        // Not a function: a plain column, possibly cast.
                        if matches!(self.peek_at(1), Tok::LParen) {
                            let name = self.word().unwrap_or_default();
                            return Err(match self.listed(&name.to_ascii_uppercase()) {
                                Some(error) => error,
                                None => SqlError::unsupported(format!(
                                    "function `{name}` is not in QL_CONTRACT §4.1 or §4.2"
                                )),
                            });
                        }
                        self.guard_word()?;
                        let name = self.name()?;
                        return self.row_casts(RowExpr::Column(name));
                    }
                };
                self.bump();
                self.expect(&Tok::LParen)?;
                let mut args = Vec::new();
                if !matches!(self.peek(), Tok::RParen) {
                    loop {
                        args.push(self.row_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                RowExpr::Str { func, args }
            }
        };
        self.row_casts(expr)
    }

    /// A literal without the trailing cast, which a row expression reads
    /// itself so `'2020-01-01'::date` is one node rather than two.
    fn literal_no_cast(&mut self) -> SqlResult2<Literal> {
        if self.eat(&Tok::Minus) {
            return match self.literal_no_cast()? {
                Literal::Num(value, exact) => Ok(Literal::Num(-value, exact)),
                other => Err(SqlError::unsupported(format!(
                    "unary minus applies to a number, not to {other:?}"
                ))),
            };
        }
        let at = self.here();
        Ok(match self.peek().clone() {
            Tok::Num(value, exact) => {
                self.bump();
                Literal::Num(value, exact)
            }
            Tok::Str(text) => {
                self.bump();
                Literal::Str(text)
            }
            Tok::Param(n) => {
                self.bump();
                Literal::Param(n)
            }
            other => {
                return Err(SqlError::syntax(
                    format!("expected a literal, found `{}`", other.written()),
                    at,
                ))
            }
        })
    }

    pub(super) fn literal(&mut self) -> SqlResult2<Literal> {
        self.deeper()?;
        let result = self.literal_inner();
        self.shallower();
        result
    }

    fn literal_inner(&mut self) -> SqlResult2<Literal> {
        if self.eat(&Tok::Minus) {
            return match self.literal()? {
                Literal::Num(value, exact) => Ok(Literal::Num(-value, exact)),
                other => Err(SqlError::unsupported(format!(
                    "unary minus applies to a number, not to {other:?}"
                ))),
            };
        }
        if matches!(self.peek(), Tok::LParen) && self.word_at(1).as_deref() == Some("SELECT") {
            self.bump();
            self.bump();
            let column = self.name()?;
            self.expect_word("FROM")?;
            let table = self.name()?;
            self.expect_word("WHERE")?;
            let key_column = self.name()?;
            if !super::is_key_column(&key_column) {
                return Err(SqlError::unsupported(format!(
                    "a scalar subquery reads ONE row by key: write `WHERE {} = ...`",
                    super::KEY_COLUMN
                )));
            }
            self.expect(&Tok::Eq)?;
            let key = self.literal()?;
            self.expect(&Tok::RParen)?;
            return Ok(Literal::Subquery(Box::new(ScalarSubquery {
                column,
                table,
                key,
            })));
        }
        let at = self.here();
        let literal = match self.peek().clone() {
            Tok::Num(value, exact) => {
                self.bump();
                Literal::Num(value, exact)
            }
            Tok::Str(text) => {
                self.bump();
                Literal::Str(text)
            }
            Tok::Param(n) => {
                self.bump();
                Literal::Param(n)
            }
            Tok::Word(word) => match word.to_ascii_uppercase().as_str() {
                "NULL" => {
                    self.bump();
                    Literal::Null
                }
                "TRUE" => {
                    self.bump();
                    Literal::Bool(true)
                }
                "FALSE" => {
                    self.bump();
                    Literal::Bool(false)
                }
                other => {
                    return Err(match self.listed(other) {
                        Some(error) => error,
                        None => SqlError::syntax(
                            format!("expected a literal or a parameter, found `{word}`"),
                            at,
                        ),
                    })
                }
            },
            other => {
                return Err(SqlError::syntax(
                    format!("expected a literal or a parameter, found `{}`", other.written()),
                    at,
                ))
            }
        };
        self.optional_cast()?;
        Ok(literal)
    }
}
