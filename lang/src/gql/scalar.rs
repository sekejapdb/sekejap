//! The value operations of the M3-C scalar pack: arithmetic, `||`, the
//! functions and the casts, over values already evaluated
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §2.5, owner answer Q14).
//!
//! Each is PostgreSQL's operation, with PostgreSQL's errors, each carrying
//! its SQLSTATE as data (`SqlError::Coded`):
//!
//! * an integer operation stays an integer: `/` truncates toward zero, `%`
//!   takes the dividend's sign, and a result outside `i64` is `22003`, never
//!   a wrap and never a float;
//! * a float result that overflows to infinity from finite operands is
//!   `22003`;
//! * a division or a remainder by zero is `22012` and `LN` of zero or of a
//!   negative number is `2201E` (Q14): an error, never NULL. `ScoreExpr` in
//!   `ORDER BY` keeps its own rule (a division by zero gives NaN), which is
//!   a different surface;
//! * `SQRT` of a negative number, and the `POWER` cases with no real answer,
//!   are `2201F`;
//! * a text that does not spell the type it is cast to is `22P02`.
//!
//! NULL is not seen here: the evaluator returns NULL for a NULL operand
//! before it calls in, except for `concat`, which skips NULL arguments.
//!
//! The string functions and the boolean input are `crate::functions`'s,
//! which the SQL row functions (`compile/row.rs`) call too; an integer, a
//! float or a boolean argument is read as its text, as the SQL row
//! functions read it.

use super::ast::{ArithOp, CastType, Func};
use super::eval::{kind, mismatch, raise, Evaluated, Fault};
use crate::functions::{self, TimeUnit};
use crate::sqlstate::{
    CANNOT_COERCE, DATETIME_FIELD_OVERFLOW, DIVISION_BY_ZERO, INVALID_ARGUMENT_FOR_LOGARITHM,
    INVALID_ARGUMENT_FOR_POWER_FUNCTION, INVALID_DATETIME_FORMAT, INVALID_TEXT_REPRESENTATION,
    NUMERIC_VALUE_OUT_OF_RANGE, SUBSTRING_ERROR,
};
use sekejap_core::collections::gql::BindingValue as V;

pub(super) fn bigint_out_of_range() -> Fault {
    raise(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
}

fn overflow() -> Fault {
    raise(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

fn division_by_zero() -> Fault {
    raise(DIVISION_BY_ZERO, "division by zero")
}

fn number(value: &V) -> Option<f64> {
    match value {
        V::Int(i) => Some(*i as f64),
        V::Float(f) => Some(*f),
        _ => None,
    }
}

/// A float result, refused when it overflowed from finite operands.
fn finite(result: f64, operands: &[f64]) -> Evaluated<V> {
    if result.is_infinite() && operands.iter().all(|x| x.is_finite()) {
        return Err(overflow());
    }
    Ok(V::Float(result))
}

/// Unary minus.
pub(super) fn negate(value: &V) -> Evaluated<V> {
    match value {
        V::Int(i) => i.checked_neg().map(V::Int).ok_or_else(bigint_out_of_range),
        V::Float(f) => Ok(V::Float(-f)),
        other => Err(mismatch(format!(
            "-{} is not defined: unary minus takes a number",
            kind(other)
        ))),
    }
}

/// `left op right`, both not NULL.
pub(super) fn arith(op: ArithOp, left: &V, right: &V) -> Evaluated<V> {
    match (left, right) {
        (V::Int(a), V::Int(b)) => int_arith(op, *a, *b),
        _ => match (number(left), number(right)) {
            (Some(a), Some(b)) => float_arith(op, a, b),
            _ => Err(mismatch(format!(
                "{} {} {} is not defined: arithmetic takes numbers",
                kind(left),
                op.written(),
                kind(right)
            ))),
        },
    }
}

fn int_arith(op: ArithOp, a: i64, b: i64) -> Evaluated<V> {
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div | ArithOp::Mod if b == 0 => return Err(division_by_zero()),
        ArithOp::Div => a.checked_div(b),
        // The one remainder that would overflow, `i64::MIN % -1`, is 0 in
        // PostgreSQL.
        ArithOp::Mod => Some(a.checked_rem(b).unwrap_or(0)),
        // `^` is `power`, which is a float function for integers too.
        ArithOp::Pow => return power(a as f64, b as f64),
    };
    result.map(V::Int).ok_or_else(bigint_out_of_range)
}

fn float_arith(op: ArithOp, a: f64, b: f64) -> Evaluated<V> {
    let result = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div | ArithOp::Mod if b == 0.0 => return Err(division_by_zero()),
        ArithOp::Div => a / b,
        ArithOp::Mod => a % b,
        ArithOp::Pow => return power(a, b),
    };
    finite(result, &[a, b])
}

/// `power(a, b)` and `a ^ b`.
fn power(a: f64, b: f64) -> Evaluated<V> {
    if a == 0.0 && b < 0.0 {
        return Err(raise(
            INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "zero raised to a negative power is undefined",
        ));
    }
    if a < 0.0 && b.fract() != 0.0 {
        return Err(raise(
            INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    finite(a.powf(b), &[a, b])
}

/// `left || right`, both not NULL.
pub(super) fn concat(left: &V, right: &V) -> Evaluated<V> {
    Ok(V::Text(
        format!("{}{}", text_of(left, "||")?, text_of(right, "||")?).into(),
    ))
}

/// A value read as text by a string function: text, or a number or a
/// boolean spelled as the SQL row functions spell it.
fn text_of(value: &V, what: &str) -> Evaluated<String> {
    Ok(match value {
        V::Text(text) => text.to_string(),
        V::Int(i) => i.to_string(),
        V::Float(f) => f.to_string(),
        V::Bool(b) => b.to_string(),
        other => {
            return Err(mismatch(format!(
                "{what} takes text and was given {}",
                kind(other)
            )))
        }
    })
}

/// A whole-number argument, as the SQL row functions read one.
fn int_of(value: &V, what: &str) -> Evaluated<i64> {
    match value {
        V::Int(i) => Ok(*i),
        V::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
        other => Err(mismatch(format!(
            "{what} takes a whole number and was given {}",
            kind(other)
        ))),
    }
}

fn float_of(value: &V, what: &str) -> Evaluated<f64> {
    number(value).ok_or_else(|| {
        mismatch(format!(
            "{what} takes a number and was given {}",
            kind(value)
        ))
    })
}

/// A function of the pack over its evaluated arguments: none of them NULL,
/// except for `concat`, which skips them.
pub(super) fn call(func: Func, args: &[V]) -> Evaluated<V> {
    let what = func.written();
    let text = |at: usize| text_of(&args[at], what);
    Ok(match func {
        Func::Abs => match &args[0] {
            V::Int(i) => i
                .checked_abs()
                .map(V::Int)
                .ok_or_else(bigint_out_of_range)?,
            other => V::Float(float_of(other, what)?.abs()),
        },
        Func::Sqrt => {
            let x = float_of(&args[0], what)?;
            if x < 0.0 {
                return Err(raise(
                    INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                    "cannot take square root of a negative number",
                ));
            }
            V::Float(x.sqrt())
        }
        Func::Power => power(float_of(&args[0], what)?, float_of(&args[1], what)?)?,
        Func::Exp => {
            let x = float_of(&args[0], what)?;
            finite(x.exp(), &[x])?
        }
        Func::Ln => {
            let x = float_of(&args[0], what)?;
            if x == 0.0 {
                return Err(raise(INVALID_ARGUMENT_FOR_LOGARITHM, "cannot take logarithm of zero"));
            }
            if x < 0.0 {
                return Err(raise(INVALID_ARGUMENT_FOR_LOGARITHM, "cannot take logarithm of a negative number"));
            }
            V::Float(x.ln())
        }
        Func::Lower => V::Text(functions::lower(&text(0)?).into()),
        Func::Upper => V::Text(functions::upper(&text(0)?).into()),
        Func::Trim => V::Text(functions::trim(&text(0)?).into()),
        Func::Length => V::Int(functions::length(&text(0)?)),
        Func::Substring => {
            let count = args.get(2).map(|count| int_of(count, what)).transpose()?;
            let out = functions::substring(&text(0)?, int_of(&args[1], what)?, count)
                .map_err(|error| raise(SUBSTRING_ERROR, error))?;
            V::Text(out.into())
        }
        Func::Concat => {
            let parts = args
                .iter()
                .map(|arg| match arg {
                    V::Null => Ok(None),
                    arg => text_of(arg, what).map(Some),
                })
                .collect::<Evaluated<Vec<_>>>()?;
            V::Text(functions::concat(parts).into())
        }
    })
}

/// `CAST(value AS to)`, `value` not NULL.
pub(super) fn cast(value: &V, to: CastType) -> Evaluated<V> {
    let syntax = |text: &str, ty: &str| {
        raise(
            INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type {ty}: \"{text}\""),
        )
    };
    Ok(match (to, value) {
        (CastType::Text, V::Text(_))
        | (CastType::Int, V::Int(_))
        | (CastType::Float, V::Float(_))
        | (CastType::Bool, V::Bool(_))
        | (CastType::Json, V::Json(_))
        | (CastType::Timestamp, V::Int(_)) => value.clone(),
        (CastType::Text, V::Int(_) | V::Float(_) | V::Bool(_)) => {
            V::Text(text_of(value, "::text")?.into())
        }
        (CastType::Int, V::Float(f)) => {
            // PostgreSQL rounds half away from zero, and refuses what does
            // not fit.
            let rounded = f.round();
            if !(-9.223_372_036_854_775_808e18..9.223_372_036_854_775_808e18).contains(&rounded) {
                return Err(bigint_out_of_range());
            }
            V::Int(rounded as i64)
        }
        (CastType::Int, V::Text(text)) => V::Int(
            text.trim()
                .parse()
                .map_err(|_| syntax(text.as_ref(), "bigint"))?,
        ),
        (CastType::Int, V::Bool(b)) => V::Int(i64::from(*b)),
        (CastType::Float, V::Int(i)) => V::Float(*i as f64),
        (CastType::Float, V::Text(text)) => V::Float(
            text.trim()
                .parse()
                .map_err(|_| syntax(text.as_ref(), "double precision"))?,
        ),
        (CastType::Bool, V::Int(i)) => V::Bool(*i != 0),
        (CastType::Bool, V::Text(text)) => {
            V::Bool(functions::parse_bool(text).ok_or_else(|| syntax(text.as_ref(), "boolean"))?)
        }
        (CastType::Json, V::Text(text)) => V::Json(
            serde_json::from_str::<serde_json::Value>(text)
                .map_err(|_| syntax(text.as_ref(), "json"))?
                .into(),
        ),
        (CastType::Date, V::Text(text)) => V::Int(day(timestamp(text)?)?),
        (CastType::Date, V::Int(micros)) => V::Int(day(*micros)?),
        (CastType::Timestamp, V::Text(text)) => V::Int(timestamp(text)?),
        (to, other) => {
            return Err(raise(CANNOT_COERCE, format!(
                "cannot cast {} to {}",
                kind(other),
                to.written().to_ascii_lowercase()
            )))
        }
    })
}

/// A date/time literal read as the SQL surface reads one.
fn timestamp(text: &str) -> Evaluated<i64> {
    functions::parse_timestamp(text).map_err(|error| raise(INVALID_DATETIME_FORMAT, error))
}

/// Midnight UTC of the day `micros` falls in: a `DATE`.
fn day(micros: i64) -> Evaluated<i64> {
    functions::date_trunc(TimeUnit::Day, micros).map_err(|error| raise(DATETIME_FIELD_OVERFLOW, error))
}
