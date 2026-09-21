use super::*;

// ── row functions over projected values (QL_CONTRACT §4.1, §4.2) ──────────

/// A row expression with every name resolved and every constant folded.
///
/// `Field(at)` is a position in the plan's `Projection::Fields` list, so
/// evaluating one costs a read of a value the page already produced: the cost
/// is proportional to the rows RETURNED, which is what §4.1 and §4.2 promise
/// and what `EXPLAIN` prints under "row functions".
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledRow {
    /// The n-th projected field, with the declared spelling that decides how
    /// a stored integer prints.
    Field { at: usize, time: bool },
    Lit(SqlValue),
    /// The clock, already folded: every row of one answer sees one instant.
    Micros(i64),
    Extract { unit: TimeUnit, arg: Box<CompiledRow> },
    Trunc { unit: TimeUnit, arg: Box<CompiledRow> },
    Age { left: Box<CompiledRow>, right: Box<CompiledRow> },
    ToChar { arg: Box<CompiledRow>, format: String },
    ToTimestamp(Box<CompiledRow>),
    ToDate(Box<CompiledRow>),
    CastDate(Box<CompiledRow>),
    CastText(Box<CompiledRow>),
    /// A declared TIMESTAMPTZ/DATE printed back as an ISO-8601 string.
    Iso { arg: Box<CompiledRow>, date_only: bool },
    Str { func: StrFunc, args: Vec<CompiledRow> },
    Add(Box<CompiledRow>, Box<CompiledRow>),
    Sub(Box<CompiledRow>, Box<CompiledRow>),
    Concat(Box<CompiledRow>, Box<CompiledRow>),
}

/// NULL propagates the way SQL says it does: any NULL or MISSING input makes
/// the whole expression NULL, and no function is called on it.
fn nullish(value: &SqlValue) -> bool {
    matches!(value, SqlValue::Null | SqlValue::Missing)
}

fn want_int(value: &SqlValue, what: &str) -> SqlResult2<i64> {
    match value {
        SqlValue::Int(n) => Ok(*n),
        SqlValue::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
        other => Err(SqlError::Parameter(format!(
            "{what} takes a whole number and the row holds {other:?}"
        ))),
    }
}

fn want_text(value: &SqlValue, what: &str) -> SqlResult2<String> {
    match value {
        SqlValue::Text(text) => Ok(text.clone()),
        SqlValue::Int(n) => Ok(n.to_string()),
        SqlValue::Float(f) => Ok(f.to_string()),
        SqlValue::Bool(b) => Ok(if *b { "true" } else { "false" }.to_owned()),
        other => Err(SqlError::Parameter(format!(
            "{what} takes text and the row holds {other:?}"
        ))),
    }
}

impl CompiledRow {
    /// One value, from one row's projected values. Reads no other row.
    pub(crate) fn eval(&self, values: &[SqlValue]) -> SqlResult2<SqlValue> {
        Ok(match self {
            Self::Field { at, time } => {
                let value = values.get(*at).cloned().unwrap_or(SqlValue::Missing);
                let _ = time;
                value
            }
            Self::Lit(value) => value.clone(),
            Self::Micros(n) => SqlValue::Int(*n),
            Self::Extract { unit, arg } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Int(functions::extract(
                    *unit,
                    want_int(&value, "EXTRACT(... FROM t)")?,
                ))
            }
            Self::Trunc { unit, arg } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Int(functions::date_trunc(
                    *unit,
                    want_int(&value, "date_trunc(u, t)")?,
                )?)
            }
            Self::Age { left, right } => {
                let (a, b) = (left.eval(values)?, right.eval(values)?);
                if nullish(&a) || nullish(&b) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Int(
                    want_int(&a, "age(a, b)")?.saturating_sub(want_int(&b, "age(a, b)")?),
                )
            }
            Self::ToChar { arg, format } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Text(functions::to_char(want_int(&value, "to_char(t, f)")?, format)?)
            }
            Self::ToTimestamp(arg) => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                match value {
                    // `to_timestamp(seconds)` per Postgres; a text argument is
                    // the literal reader, which is `to_date`'s job but the
                    // same grammar.
                    SqlValue::Text(text) => SqlValue::Int(functions::parse_timestamp(&text)?),
                    other => SqlValue::Int(
                        want_int(&other, "to_timestamp(seconds)")?
                            .saturating_mul(functions::MICROS_PER_SECOND),
                    ),
                }
            }
            Self::ToDate(arg) | Self::CastDate(arg) => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                let micros = match value {
                    SqlValue::Text(text) => functions::parse_timestamp(&text)?,
                    other => want_int(&other, "to_date(t)")?,
                };
                SqlValue::Int(functions::date_trunc(TimeUnit::Day, micros)?)
            }
            Self::CastText(arg) => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Text(want_text(&value, "::text")?)
            }
            Self::Iso { arg, date_only } => {
                let value = arg.eval(values)?;
                if nullish(&value) {
                    return Ok(SqlValue::Null);
                }
                let micros = want_int(&value, "a declared timestamp")?;
                SqlValue::Text(if *date_only {
                    functions::format_date(micros)
                } else {
                    functions::format_timestamp(micros)
                })
            }
            Self::Str { func, args } => {
                let mut evaluated = Vec::with_capacity(args.len());
                for arg in args {
                    let value = arg.eval(values)?;
                    // `concat` is the one Postgres function that IGNORES
                    // NULLs rather than propagating them, and it is the one
                    // exception here too.
                    if nullish(&value) && *func != StrFunc::Concat {
                        return Ok(SqlValue::Null);
                    }
                    evaluated.push(value);
                }
                return string_function(*func, &evaluated);
            }
            Self::Add(a, b) | Self::Sub(a, b) => {
                let (x, y) = (a.eval(values)?, b.eval(values)?);
                if nullish(&x) || nullish(&y) {
                    return Ok(SqlValue::Null);
                }
                let (x, y) = (want_int(&x, "date arithmetic")?, want_int(&y, "date arithmetic")?);
                SqlValue::Int(if matches!(self, Self::Add(_, _)) {
                    x.saturating_add(y)
                } else {
                    x.saturating_sub(y)
                })
            }
            Self::Concat(a, b) => {
                let (x, y) = (a.eval(values)?, b.eval(values)?);
                // `||` propagates NULL, unlike `concat`.
                if nullish(&x) || nullish(&y) {
                    return Ok(SqlValue::Null);
                }
                SqlValue::Text(format!("{}{}", want_text(&x, "||")?, want_text(&y, "||")?))
            }
        })
    }
}

/// The §4.1 string functions, over already-evaluated arguments.
fn string_function(func: StrFunc, args: &[SqlValue]) -> SqlResult2<SqlValue> {
    let arity = |want: std::ops::RangeInclusive<usize>| -> SqlResult2<()> {
        if want.contains(&args.len()) {
            Ok(())
        } else {
            Err(SqlError::unsupported(format!(
                "{}() takes {}..={} arguments and was given {}",
                func.written(),
                want.start(),
                want.end(),
                args.len()
            )))
        }
    };
    let text = |at: usize| want_text(&args[at], func.written());
    let int = |at: usize| want_int(&args[at], func.written());
    Ok(match func {
        StrFunc::Lower => {
            arity(1..=1)?;
            SqlValue::Text(text(0)?.to_lowercase())
        }
        StrFunc::Upper => {
            arity(1..=1)?;
            SqlValue::Text(text(0)?.to_uppercase())
        }
        StrFunc::Length => {
            arity(1..=1)?;
            SqlValue::Int(functions::length(&text(0)?))
        }
        StrFunc::Trim => {
            arity(1..=1)?;
            SqlValue::Text(text(0)?.trim().to_owned())
        }
        StrFunc::Concat => {
            let mut out = String::new();
            for (at, value) in args.iter().enumerate() {
                if nullish(value) {
                    continue;
                }
                out.push_str(&want_text(value, func.written()).map_err(|_| {
                    SqlError::Parameter(format!("concat() argument {} is not text", at + 1))
                })?);
            }
            SqlValue::Text(out)
        }
        StrFunc::Substring => {
            arity(2..=3)?;
            let count = if args.len() == 3 { Some(int(2)?) } else { None };
            SqlValue::Text(functions::substring(&text(0)?, int(1)?, count)?)
        }
        StrFunc::Left => {
            arity(2..=2)?;
            SqlValue::Text(functions::left(&text(0)?, int(1)?))
        }
        StrFunc::Right => {
            arity(2..=2)?;
            SqlValue::Text(functions::right(&text(0)?, int(1)?))
        }
        StrFunc::SplitPart => {
            arity(3..=3)?;
            SqlValue::Text(functions::split_part(&text(0)?, &text(1)?, int(2)?)?)
        }
        StrFunc::Replace => {
            arity(3..=3)?;
            SqlValue::Text(text(0)?.replace(&text(1)?, &text(2)?))
        }
        StrFunc::Position => {
            arity(2..=2)?;
            SqlValue::Int(functions::position(&text(0)?, &text(1)?))
        }
        StrFunc::StartsWith => {
            arity(2..=2)?;
            SqlValue::Bool(text(0)?.starts_with(&text(1)?))
        }
    })
}

impl Compiler<'_> {
    /// A parsed `RowExpr` with every column resolved to a projection slot and
    /// every constant folded.
    ///
    /// Resolving a column APPENDS it to the projection list, so a function
    /// over a column the select list does not otherwise name still costs one
    /// projected field and no extra read: the page already decodes the row it
    /// returns.
    /// A row expression whose value should be printed as a declared
    /// timestamp's ISO text rather than as the decimal of its microseconds.
    ///
    /// `Some(date_only)` exactly when `expr` is a bare column whose declared
    /// type is TIMESTAMPTZ or DATE, so `born_day` prints `1940-01-03` and
    /// `born_ts` prints `1940-01-03T05:06:00Z` on EVERY string path -- the
    /// two `||` sides, a string function's argument, and `::text`. Stated
    /// once so those three cannot disagree.
    fn iso_text(&mut self, c: CollectionId, expr: &RowExpr) -> SqlResult2<Option<bool>> {
        let RowExpr::Column(name) = expr else {
            return Ok(None);
        };
        Ok(self
            .time_column(c, name)?
            .map(|declared| declared == "DATE"))
    }

    pub(super) fn row_function(
        &mut self,
        c: CollectionId,
        expr: &RowExpr,
        fields: &mut Vec<String>,
    ) -> SqlResult2<CompiledRow> {
        Ok(match expr {
            RowExpr::Column(name) => {
                self.kind_of(c, name)?;
                let at = match fields.iter().position(|existing| existing == name) {
                    Some(at) => at,
                    None => {
                        fields.push(name.clone());
                        fields.len() - 1
                    }
                };
                CompiledRow::Field {
                    at,
                    time: self.time_column(c, name)?.is_some(),
                }
            }
            RowExpr::Lit(literal) => CompiledRow::Lit(match self.value_of(literal)? {
                Value::Null => SqlValue::Null,
                Value::Bool(b) => SqlValue::Bool(b),
                Value::Number(n) => match n.as_i64() {
                    Some(i) => SqlValue::Int(i),
                    None => SqlValue::Float(n.as_f64().unwrap_or(f64::NAN)),
                },
                Value::String(text) => SqlValue::Text(text),
                other => SqlValue::Json(other),
            }),
            RowExpr::Now => CompiledRow::Micros(self.clock_micros()),
            RowExpr::CurrentDate => {
                CompiledRow::Micros(functions::date_trunc(TimeUnit::Day, self.clock_micros())?)
            }
            RowExpr::Interval(micros) => CompiledRow::Micros(*micros),
            RowExpr::Extract { unit, arg } => CompiledRow::Extract {
                unit: *unit,
                arg: Box::new(self.row_function(c, arg, fields)?),
            },
            RowExpr::Trunc { unit, arg } => CompiledRow::Trunc {
                unit: *unit,
                arg: Box::new(self.row_function(c, arg, fields)?),
            },
            RowExpr::Age { left, right } => CompiledRow::Age {
                left: Box::new(match right {
                    // `age(t)` is `now() - t`, so the clock is the LEFT side.
                    None => CompiledRow::Micros(self.clock_micros()),
                    Some(_) => self.row_function(c, left, fields)?,
                }),
                right: Box::new(match right {
                    None => self.row_function(c, left, fields)?,
                    Some(right) => self.row_function(c, right, fields)?,
                }),
            },
            RowExpr::ToChar { arg, format } => {
                // The template is checked HERE, at prepare, so a template
                // this slice does not carry is a refusal rather than an error
                // on the first row.
                functions::to_char(0, format)?;
                CompiledRow::ToChar {
                    arg: Box::new(self.row_function(c, arg, fields)?),
                    format: format.clone(),
                }
            }
            RowExpr::ToTimestamp(arg) => {
                CompiledRow::ToTimestamp(Box::new(self.row_function(c, arg, fields)?))
            }
            RowExpr::ToDate(arg) => {
                CompiledRow::ToDate(Box::new(self.row_function(c, arg, fields)?))
            }
            RowExpr::CastDate(arg) => {
                CompiledRow::CastDate(Box::new(self.row_function(c, arg, fields)?))
            }
            RowExpr::CastText(arg) => {
                let inner = self.row_function(c, arg, fields)?;
                // A declared timestamp cast to text is its ISO spelling, not
                // the decimal of its microseconds.
                match self.iso_text(c, arg)? {
                    Some(date_only) => CompiledRow::Iso {
                        arg: Box::new(inner),
                        date_only,
                    },
                    None => CompiledRow::CastText(Box::new(inner)),
                }
            }
            RowExpr::Str { func, args } => {
                let mut compiled = Vec::with_capacity(args.len());
                for arg in args {
                    compiled.push(self.row_function(c, arg, fields)?);
                }
                // A declared timestamp handed to a STRING function is its ISO
                // spelling: `upper(born_ts)` reads the text a SELECT prints,
                // not the integer underneath it.
                for at in 0..compiled.len() {
                    if let Some(date_only) = self.iso_text(c, &args[at])? {
                        compiled[at] = CompiledRow::Iso {
                            arg: Box::new(compiled[at].clone()),
                            date_only,
                        };
                    }
                }
                CompiledRow::Str {
                    func: *func,
                    args: compiled,
                }
            }
            RowExpr::Add(a, b) => CompiledRow::Add(
                Box::new(self.row_function(c, a, fields)?),
                Box::new(self.row_function(c, b, fields)?),
            ),
            RowExpr::Sub(a, b) => CompiledRow::Sub(
                Box::new(self.row_function(c, a, fields)?),
                Box::new(self.row_function(c, b, fields)?),
            ),
            RowExpr::Concat(a, b) => {
                // `||` is a string operator, so a declared timestamp on
                // either side of it is its ISO spelling -- the same rule
                // `concat(born_ts, '')` and `born_ts::text` already follow.
                // Without this `born_ts || ''` printed the decimal of its
                // microseconds while `concat(born_ts, '')` printed the text.
                let mut left = self.row_function(c, a, fields)?;
                let mut right = self.row_function(c, b, fields)?;
                if let Some(date_only) = self.iso_text(c, a)? {
                    left = CompiledRow::Iso {
                        arg: Box::new(left),
                        date_only,
                    };
                }
                if let Some(date_only) = self.iso_text(c, b)? {
                    right = CompiledRow::Iso {
                        arg: Box::new(right),
                        date_only,
                    };
                }
                CompiledRow::Concat(Box::new(left), Box::new(right))
            }
        })
    }

}
