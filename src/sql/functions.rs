//! The §4.1 string and §4.2 date/time functions, as PURE functions over
//! values -- no database, no index, no row reader.
//!
//! Two callers share this module and that sharing is the point:
//!
//! 1. `compile.rs` folds a date/time function in a `WHERE` into scalar index
//!    bounds (`micros` in, `micros` out). The rewrite is index-side: the
//!    predicate becomes one `ScalarFilter::Range` on the column's own btree,
//!    so the work stays proportional to the candidates the range walks.
//! 2. `compile.rs` also compiles a function in a SELECT list into a row
//!    expression evaluated over PROJECTED values, after the index-side stage.
//!    That cost is proportional to the rows RETURNED, and `EXPLAIN` says so.
//!
//! One implementation answers both, so `EXTRACT(YEAR FROM t) = 1950` in a
//! WHERE and `EXTRACT(YEAR FROM t)` in a SELECT list cannot disagree.
//!
//! Storage, per `docs/QL_CONTRACT.md` §4.2 and §5 deviation 8: a declared
//! `TIMESTAMPTZ` or `DATE` is `Kind::Int`, microseconds since 1970-01-01
//! 00:00:00 UTC, and a `DATE` is midnight UTC of its day. There is no
//! time-zone storage; a zone offset in a written literal is applied when the
//! literal is read and nothing else is kept.

use super::{SqlError, SqlResult2};

/// Microseconds in one second, minute, hour and day. A day is exactly
/// 86_400_000_000 microseconds here: the stored value is a UTC instant count,
/// so there is no leap second to widen one.
pub(crate) const MICROS_PER_SECOND: i64 = 1_000_000;
pub(crate) const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
pub(crate) const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
pub(crate) const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;

/// The declared SQL types that are stored as `Kind::Int` microseconds.
pub(crate) fn is_time_type(declared: &str) -> bool {
    matches!(declared, "TIMESTAMPTZ" | "TIMESTAMP" | "DATE")
}

/// The unit a `date_trunc` or an `EXTRACT` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeUnit {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    /// `EXTRACT(DOW ...)` only: there is no `date_trunc('dow', ...)`.
    Dow,
    /// `EXTRACT(EPOCH ...)` only.
    Epoch,
}

impl TimeUnit {
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
            Self::Hour => "hour",
            Self::Minute => "minute",
            Self::Second => "second",
            Self::Dow => "dow",
            Self::Epoch => "epoch",
        }
    }

    /// The unit as `EXTRACT` and `date_trunc` write it, or `None` for a unit
    /// this contract does not carry.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        Some(match word.to_ascii_lowercase().as_str() {
            "year" | "years" | "y" => Self::Year,
            "month" | "months" | "mon" => Self::Month,
            "day" | "days" | "d" => Self::Day,
            "hour" | "hours" | "h" => Self::Hour,
            "minute" | "minutes" | "min" => Self::Minute,
            "second" | "seconds" | "sec" => Self::Second,
            "dow" | "dayofweek" => Self::Dow,
            "epoch" => Self::Epoch,
            _ => return None,
        })
    }

    /// True when truncating to this unit is MONOTONE over the stored integer
    /// AND has a contiguous pre-image: `date_trunc(u, t) = v` is then ONE
    /// range `[v, next(v))`. Every unit here has that property; `Dow` and
    /// `Epoch` are not truncation units at all.
    pub(crate) fn truncates(self) -> bool {
        !matches!(self, Self::Dow | Self::Epoch)
    }
}

/// A civil UTC date and time, the shape both the formatter and the extractor
/// read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Civil {
    pub(crate) year: i64,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
    pub(crate) micro: u32,
}

/// Days since 1970-01-01 from a proleptic-Gregorian civil date.
///
/// Howard Hinnant's `days_from_civil` (chrono-Compatible Low-Level Date
/// Algorithms, 2013), which is exact for every year in `i64` and has no table
/// and no branch on leap years. It is written out rather than pulled in
/// because this crate takes no date dependency.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // 0..=399
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Floor division and the non-negative remainder, which is what a civil
/// calendar needs for instants before the epoch.
fn floor_div_rem(a: i64, b: i64) -> (i64, i64) {
    let q = a.div_euclid(b);
    (q, a - q * b)
}

pub(crate) fn civil_from_micros(micros: i64) -> Civil {
    let (days, rest) = floor_div_rem(micros, MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (rest / MICROS_PER_HOUR) as u32,
        minute: (rest % MICROS_PER_HOUR / MICROS_PER_MINUTE) as u32,
        second: (rest % MICROS_PER_MINUTE / MICROS_PER_SECOND) as u32,
        micro: (rest % MICROS_PER_SECOND) as u32,
    }
}

pub(crate) fn micros_from_civil(c: Civil) -> i64 {
    days_from_civil(c.year, c.month, c.day) * MICROS_PER_DAY
        + i64::from(c.hour) * MICROS_PER_HOUR
        + i64::from(c.minute) * MICROS_PER_MINUTE
        + i64::from(c.second) * MICROS_PER_SECOND
        + i64::from(c.micro)
}

/// `date_trunc(unit, t)`: the first instant of the unit `t` falls in.
pub(crate) fn date_trunc(unit: TimeUnit, micros: i64) -> SqlResult2<i64> {
    let c = civil_from_micros(micros);
    Ok(match unit {
        TimeUnit::Year => micros_from_civil(Civil {
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
            ..c
        }),
        TimeUnit::Month => micros_from_civil(Civil {
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
            ..c
        }),
        TimeUnit::Day => micros_from_civil(Civil {
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
            ..c
        }),
        TimeUnit::Hour => micros - micros.rem_euclid(MICROS_PER_HOUR),
        TimeUnit::Minute => micros - micros.rem_euclid(MICROS_PER_MINUTE),
        TimeUnit::Second => micros - micros.rem_euclid(MICROS_PER_SECOND),
        TimeUnit::Dow | TimeUnit::Epoch => {
            return Err(SqlError::unsupported(format!(
                "date_trunc('{}', t): `{}` is an EXTRACT field, not a truncation unit",
                unit.written(),
                unit.written()
            )))
        }
    })
}

/// The first instant AFTER the unit that starts at `start`. `start` must
/// already be truncated to `unit`.
pub(crate) fn next_unit(unit: TimeUnit, start: i64) -> SqlResult2<i64> {
    let c = civil_from_micros(start);
    Ok(match unit {
        TimeUnit::Year => micros_from_civil(Civil {
            year: c.year + 1,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
        }),
        TimeUnit::Month => {
            let (year, month) = if c.month == 12 {
                (c.year + 1, 1)
            } else {
                (c.year, c.month + 1)
            };
            micros_from_civil(Civil {
                year,
                month,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                micro: 0,
            })
        }
        TimeUnit::Day => start + MICROS_PER_DAY,
        TimeUnit::Hour => start + MICROS_PER_HOUR,
        TimeUnit::Minute => start + MICROS_PER_MINUTE,
        TimeUnit::Second => start + MICROS_PER_SECOND,
        TimeUnit::Dow | TimeUnit::Epoch => {
            return Err(SqlError::unsupported(format!(
                "`{}` is an EXTRACT field, not a truncation unit",
                unit.written()
            )))
        }
    })
}

/// `EXTRACT(<unit> FROM t)`, as the whole number Postgres reports.
///
/// `EPOCH` is SECONDS, floored, which is what Postgres reports for a
/// timestamptz; every other field is the civil component.
pub(crate) fn extract(unit: TimeUnit, micros: i64) -> i64 {
    let c = civil_from_micros(micros);
    match unit {
        TimeUnit::Year => c.year,
        TimeUnit::Month => i64::from(c.month),
        TimeUnit::Day => i64::from(c.day),
        TimeUnit::Hour => i64::from(c.hour),
        TimeUnit::Minute => i64::from(c.minute),
        TimeUnit::Second => i64::from(c.second),
        // 1970-01-01 was a Thursday, so day 0 is 4. Postgres numbers Sunday 0.
        TimeUnit::Dow => (micros.div_euclid(MICROS_PER_DAY) + 4).rem_euclid(7),
        TimeUnit::Epoch => micros.div_euclid(MICROS_PER_SECOND),
    }
}

/// A written date/time literal, read into stored microseconds.
///
/// Accepted, per `docs/QL_CONTRACT.md` §4.2's "ISO-8601 literals and Postgres
/// date/time literal forms":
///
/// ```text
/// YYYY-MM-DD
/// YYYY-MM-DD HH:MM[:SS[.ffffff]]
/// YYYY-MM-DDTHH:MM[:SS[.ffffff]]        -- ISO-8601 'T'
/// ... followed by Z, +HH, +HH:MM or -HH:MM
/// ```
///
/// A zone offset is SUBTRACTED to reach UTC and is then gone: §5 deviation 8
/// says there is no time-zone storage. No offset means UTC, which is the
/// deviation stated plainly -- Postgres would read a bare literal in the
/// session zone, and this engine has no session zone.
pub(crate) fn parse_timestamp(text: &str) -> SqlResult2<i64> {
    let s = text.trim();
    let bad = || {
        SqlError::Parameter(format!(
            "`{text}` is not an ISO-8601 or Postgres date/time literal: write YYYY-MM-DD, or YYYY-MM-DD HH:MM:SS[.ffffff] with an optional Z or +HH:MM offset"
        ))
    };
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return Err(bad());
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(bad());
    }
    let year = num(0, 4).ok_or_else(bad)?;
    let month = num(5, 7).ok_or_else(bad)?;
    let day = num(8, 10).ok_or_else(bad)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let mut rest = &s[10..];
    let mut hour = 0i64;
    let mut minute = 0i64;
    let mut second = 0i64;
    let mut micro = 0i64;
    let mut offset = 0i64;
    if !rest.is_empty() {
        let first = rest.as_bytes()[0];
        if first == b'T' || first == b't' || first == b' ' {
            rest = &rest[1..];
            let time_bytes = rest.as_bytes();
            if time_bytes.len() < 5 || time_bytes[2] != b':' {
                return Err(bad());
            }
            hour = rest[0..2].parse::<i64>().map_err(|_| bad())?;
            minute = rest[3..5].parse::<i64>().map_err(|_| bad())?;
            rest = &rest[5..];
            if rest.as_bytes().first() == Some(&b':') {
                if rest.len() < 3 {
                    return Err(bad());
                }
                second = rest[1..3].parse::<i64>().map_err(|_| bad())?;
                rest = &rest[3..];
                if rest.as_bytes().first() == Some(&b'.') {
                    let digits = rest[1..]
                        .bytes()
                        .take_while(u8::is_ascii_digit)
                        .count()
                        .min(9);
                    if digits == 0 {
                        return Err(bad());
                    }
                    let mut fraction = rest[1..1 + digits].parse::<i64>().map_err(|_| bad())?;
                    // Scale to microseconds: pad short fractions, drop the
                    // nanosecond digits the store has no room for.
                    for _ in digits..6 {
                        fraction *= 10;
                    }
                    for _ in 6..digits {
                        fraction /= 10;
                    }
                    micro = fraction;
                    rest = &rest[1 + digits..];
                }
            }
        }
        // The zone suffix.
        if !rest.is_empty() {
            let b = rest.as_bytes();
            if b[0] == b'Z' || b[0] == b'z' {
                rest = &rest[1..];
            } else if b[0] == b'+' || b[0] == b'-' {
                let sign = if b[0] == b'-' { -1 } else { 1 };
                let body = &rest[1..];
                let (oh, om, used) = if body.len() >= 5 && body.as_bytes()[2] == b':' {
                    (&body[0..2], &body[3..5], 6)
                } else if body.len() >= 4 {
                    (&body[0..2], &body[2..4], 5)
                } else if body.len() >= 2 {
                    (&body[0..2], "00", 3)
                } else {
                    return Err(bad());
                };
                offset = sign
                    * (oh.parse::<i64>().map_err(|_| bad())? * MICROS_PER_HOUR
                        + om.parse::<i64>().map_err(|_| bad())? * MICROS_PER_MINUTE);
                rest = &rest[used..];
            }
        }
    }
    if !rest.trim().is_empty() {
        return Err(bad());
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return Err(bad());
    }
    // The day is checked by round-tripping it: 2001-02-30 encodes to a day
    // that decodes as March, and that is what makes it invalid.
    let days = days_from_civil(year, month as u32, day as u32);
    if civil_from_days(days) != (year, month as u32, day as u32) {
        return Err(bad());
    }
    Ok(days * MICROS_PER_DAY
        + hour * MICROS_PER_HOUR
        + minute * MICROS_PER_MINUTE
        + second * MICROS_PER_SECOND
        + micro
        - offset)
}

/// The ISO-8601 spelling a `TIMESTAMPTZ` prints back as: always UTC, always
/// `Z`, and the fractional part only when it is non-zero, which is what makes
/// the round trip through [`parse_timestamp`] exact.
pub(crate) fn format_timestamp(micros: i64) -> String {
    let c = civil_from_micros(micros);
    let head = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    );
    if c.micro == 0 {
        format!("{head}Z")
    } else {
        format!("{head}.{:06}Z", c.micro)
    }
}

/// The ISO-8601 spelling a declared `DATE` prints back as.
pub(crate) fn format_date(micros: i64) -> String {
    let c = civil_from_micros(micros);
    format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)
}

/// The `to_char` format strings §4.2 names, and nothing else: an arbitrary
/// Postgres template is a small language of its own and has no atomic here.
pub(crate) fn to_char(micros: i64, format: &str) -> SqlResult2<String> {
    let c = civil_from_micros(micros);
    Ok(match format {
        "YYYY-MM-DD" => format!("{:04}-{:02}-{:02}", c.year, c.month, c.day),
        "YYYY-MM" => format!("{:04}-{:02}", c.year, c.month),
        "YYYY" => format!("{:04}", c.year),
        "HH24:MI" => format!("{:02}:{:02}", c.hour, c.minute),
        "HH24:MI:SS" => format!("{:02}:{:02}:{:02}", c.hour, c.minute, c.second),
        "YYYY-MM-DD HH24:MI:SS" => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            c.year, c.month, c.day, c.hour, c.minute, c.second
        ),
        other => {
            return Err(SqlError::unsupported(format!(
                "to_char(t, '{other}'): QL_CONTRACT §4.2 names the templates `YYYY-MM-DD`, `YYYY-MM`, `YYYY`, `HH24:MI`, `HH24:MI:SS` and `YYYY-MM-DD HH24:MI:SS`; a general Postgres template is a formatting language with no atomic here"
            )))
        }
    })
}

/// `interval '<n> <unit>'`, in microseconds.
///
/// Only FIXED-WIDTH units are accepted. A month and a year are not fixed
/// widths -- `interval '1 month'` is 28, 29, 30 or 31 days depending on where
/// it is added -- so an interval that names one has no constant to fold at
/// prepare, and it is refused with that reason rather than rounded to 30 days.
/// `date_trunc('month', t)` is the calendar-aware spelling and IS accepted.
pub(crate) fn parse_interval(text: &str) -> SqlResult2<i64> {
    let s = text.trim();
    if s.is_empty() {
        return Err(SqlError::unsupported("interval '': no magnitude written"));
    }
    let mut total: i64 = 0;
    let mut seen = false;
    let mut rest = s;
    while !rest.trim().is_empty() {
        let body = rest.trim_start();
        let sign_len = usize::from(body.starts_with('-') || body.starts_with('+'));
        let digits = body[sign_len..]
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            return Err(SqlError::unsupported(format!(
                "interval '{s}': expected a whole number before the unit"
            )));
        }
        let magnitude: i64 = body[..sign_len + digits].parse().map_err(|_| {
            SqlError::unsupported(format!("interval '{s}': the magnitude does not fit i64"))
        })?;
        let after = body[sign_len + digits..].trim_start();
        let unit_len = after.bytes().take_while(u8::is_ascii_alphabetic).count();
        let unit = &after[..unit_len];
        let scale = match unit.to_ascii_lowercase().as_str() {
            "microsecond" | "microseconds" | "us" => 1,
            "millisecond" | "milliseconds" | "ms" => 1_000,
            "second" | "seconds" | "sec" | "secs" | "s" => MICROS_PER_SECOND,
            "minute" | "minutes" | "min" | "mins" => MICROS_PER_MINUTE,
            "hour" | "hours" | "h" => MICROS_PER_HOUR,
            "day" | "days" | "d" => MICROS_PER_DAY,
            "week" | "weeks" | "w" => 7 * MICROS_PER_DAY,
            "month" | "months" | "mon" | "year" | "years" | "y" => {
                return Err(SqlError::unsupported(format!(
                    "interval '{s}': a month and a year are NOT fixed widths (28-31 days, 365-366 days), so there is no constant to fold at prepare. QL_CONTRACT §4.2 folds an interval to microseconds; date_trunc('month'|'year', t) is the calendar-aware spelling and is accepted"
                )))
            }
            "" => {
                return Err(SqlError::unsupported(format!(
                    "interval '{s}': a magnitude needs a unit (microseconds, milliseconds, seconds, minutes, hours, days, weeks)"
                )))
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "interval '{s}': `{other}` is not a fixed-width unit"
                )))
            }
        };
        total = total
            .checked_add(magnitude.checked_mul(scale).ok_or_else(|| {
                SqlError::unsupported(format!("interval '{s}': the magnitude overflows i64"))
            })?)
            .ok_or_else(|| {
                SqlError::unsupported(format!("interval '{s}': the total overflows i64"))
            })?;
        seen = true;
        rest = &after[unit_len..];
    }
    if !seen {
        return Err(SqlError::unsupported(format!("interval '{s}': empty")));
    }
    Ok(total)
}

// ── §4.1 string functions ─────────────────────────────────────────────────

/// `length(s)`: CHARACTERS, as Postgres counts them for `text`, not bytes.
pub(crate) fn length(s: &str) -> i64 {
    s.chars().count() as i64
}

/// `substring(s FROM start FOR count)`: one-based, and a start below 1 eats
/// the count the way Postgres's does.
pub(crate) fn substring(s: &str, start: i64, count: Option<i64>) -> SqlResult2<String> {
    let chars: Vec<char> = s.chars().collect();
    let end = match count {
        None => chars.len() as i64 + 1,
        Some(n) if n < 0 => {
            return Err(SqlError::unsupported(
                "substring(...) FOR a negative length: Postgres raises here and so does this",
            ))
        }
        Some(n) => start.saturating_add(n),
    };
    let from = start.max(1);
    if end <= from {
        return Ok(String::new());
    }
    let from = (from - 1) as usize;
    let to = ((end - 1) as usize).min(chars.len());
    if from >= chars.len() {
        return Ok(String::new());
    }
    Ok(chars[from..to].iter().collect())
}

pub(crate) fn left(s: &str, n: i64) -> String {
    let chars: Vec<char> = s.chars().collect();
    let take = if n < 0 {
        (chars.len() as i64 + n).max(0)
    } else {
        n.min(chars.len() as i64)
    } as usize;
    chars[..take].iter().collect()
}

pub(crate) fn right(s: &str, n: i64) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let from = if n < 0 { (-n).min(len) } else { len - n.min(len) } as usize;
    chars[from..].iter().collect()
}

/// `split_part(s, delimiter, n)`: one-based, empty when `n` runs past the
/// last field. A negative `n` counts from the end, as Postgres 14 added.
pub(crate) fn split_part(s: &str, delimiter: &str, n: i64) -> SqlResult2<String> {
    if delimiter.is_empty() {
        return Err(SqlError::unsupported(
            "split_part(s, '', n): an empty delimiter splits nothing",
        ));
    }
    if n == 0 {
        return Err(SqlError::unsupported(
            "split_part(s, d, 0): the field number is one-based",
        ));
    }
    let parts: Vec<&str> = s.split(delimiter).collect();
    let at = if n > 0 {
        n - 1
    } else {
        parts.len() as i64 + n
    };
    if at < 0 || at >= parts.len() as i64 {
        return Ok(String::new());
    }
    Ok(parts[at as usize].to_owned())
}

/// `position(sub IN s)`: one-based CHARACTER offset, 0 when absent.
pub(crate) fn position(s: &str, sub: &str) -> i64 {
    match s.find(sub) {
        None => 0,
        Some(byte) => s[..byte].chars().count() as i64 + 1,
    }
}

/// What asking for a prefix's upper bound produced.
///
/// See [`prefix_successor`]. The third arm exists because the bound has to
/// travel as a `Scalar::Text`, which is a `String`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrefixSuccessor {
    /// The smallest string strictly greater than every string starting with
    /// the prefix.
    Bound(String),
    /// The prefix is all `0xFF`: every string that starts with it runs to the
    /// end of the key order, so the range has no upper bound.
    Unbounded,
    /// The successor exists as BYTES, but is not valid UTF-8, so it cannot be
    /// carried in a `Scalar::Text` bound. REFUSED by the caller rather than
    /// approximated: see [`prefix_successor`].
    NotUtf8,
}

/// The smallest string strictly greater than every string that starts with
/// `prefix`.
///
/// This is what makes `LIKE 'abc%'` and `starts_with(col, 'abc')` ONE scalar
/// range rather than a scan: `store::scalar_key` puts text in binary UTF-8
/// order, so `[prefix, successor)` holds exactly the rows whose value starts
/// with `prefix` -- a value in that half-open interval cannot differ from
/// `prefix` inside its first `prefix.len()` bytes.
///
/// Incrementing the last byte often leaves a byte string that is not valid
/// UTF-8: `"\u{ff}"` is `C3 BF`, whose successor is `C3 C0`. A text bound is
/// a `String`, and `String::from_utf8_lossy` would turn `C3 C0` into
/// `EF BF BD` -- a strictly LARGER byte string, so the range would admit
/// every value between the two and the residual filter, which uses the same
/// bound, would not catch one. There is no atomic for a byte-valued text
/// bound in this slice, so the case is refused with a named reason rather
/// than answered from a bound that is not the one asked for (Law 8).
pub(crate) fn prefix_successor(prefix: &str) -> PrefixSuccessor {
    let mut bytes = prefix.as_bytes().to_vec();
    while let Some(last) = bytes.pop() {
        if last != 0xFF {
            bytes.push(last + 1);
            return match String::from_utf8(bytes) {
                Ok(text) => PrefixSuccessor::Bound(text),
                Err(_) => PrefixSuccessor::NotUtf8,
            };
        }
    }
    PrefixSuccessor::Unbounded
}

/// The literal prefix of a `LIKE` pattern that is a pure prefix match --
/// `'abc%'` -- or `None` for every other pattern.
///
/// `_` and an interior `%` are not prefixes, and a `%` inside the literal
/// part cannot be escaped in this slice: both cases return `None` and the
/// caller refuses with the trigram reason `docs/QL_CONTRACT.md` §3 names.
pub(crate) fn like_prefix(pattern: &str) -> Option<&str> {
    let body = pattern.strip_suffix('%')?;
    if body.is_empty() || body.contains('%') || body.contains('_') || body.contains('\\') {
        return None;
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_across_the_epoch() {
        for micros in [
            -62_135_596_800_000_000i64,
            -1,
            0,
            1,
            1_000_000,
            1_600_000_000_000_000,
        ] {
            assert_eq!(micros_from_civil(civil_from_micros(micros)), micros);
        }
    }

    #[test]
    fn a_year_is_one_range() {
        let start = parse_timestamp("1950-01-01").unwrap();
        assert_eq!(date_trunc(TimeUnit::Year, start).unwrap(), start);
        assert_eq!(
            next_unit(TimeUnit::Year, start).unwrap(),
            parse_timestamp("1951-01-01").unwrap()
        );
    }

    #[test]
    fn literal_forms_read_to_the_same_instant() {
        let base = parse_timestamp("2001-02-03T04:05:06Z").unwrap();
        assert_eq!(parse_timestamp("2001-02-03 04:05:06").unwrap(), base);
        assert_eq!(parse_timestamp("2001-02-03T05:05:06+01:00").unwrap(), base);
        assert_eq!(parse_timestamp("2001-02-03T03:05:06-0100").unwrap(), base);
        assert_eq!(
            parse_timestamp("2001-02-03").unwrap(),
            base - 4 * MICROS_PER_HOUR - 5 * MICROS_PER_MINUTE - 6 * MICROS_PER_SECOND
        );
        assert!(parse_timestamp("2001-02-30").is_err());
    }

    #[test]
    fn formatting_round_trips() {
        for text in ["1950-06-15T00:00:00Z", "2020-12-31T23:59:59.123456Z"] {
            assert_eq!(format_timestamp(parse_timestamp(text).unwrap()), text);
        }
    }

    #[test]
    fn dow_matches_the_known_weekday() {
        // 2026-09-21 is a Monday.
        let monday = parse_timestamp("2026-09-21").unwrap();
        assert_eq!(extract(TimeUnit::Dow, monday), 1);
    }

    #[test]
    fn a_calendar_interval_is_refused() {
        assert!(parse_interval("7 days").is_ok());
        assert_eq!(parse_interval("7 days").unwrap(), 7 * MICROS_PER_DAY);
        assert!(parse_interval("1 month").is_err());
    }

    #[test]
    fn prefix_ranges_bound_the_prefix() {
        assert_eq!(
            prefix_successor("Ti"),
            PrefixSuccessor::Bound("Tj".to_owned())
        );
        // A two-byte character whose last byte is not 0xBF still has a valid
        // UTF-8 successor: `é` is C3 A9 and `ê` is C3 AA.
        assert_eq!(
            prefix_successor("é"),
            PrefixSuccessor::Bound("ê".to_owned())
        );
        // `ÿ` is C3 BF, whose byte successor C3 C0 is not valid UTF-8.
        // Refused rather than lossily widened to EF BF BD, which is a LARGER
        // byte string than the bound asked for.
        assert_eq!(prefix_successor("ÿ"), PrefixSuccessor::NotUtf8);
        assert_eq!(prefix_successor("aÿ"), PrefixSuccessor::NotUtf8);
        // Nothing left to increment.
        assert_eq!(prefix_successor(""), PrefixSuccessor::Unbounded);
        assert_eq!(like_prefix("Ti%"), Some("Ti"));
        assert_eq!(like_prefix("%Ti%"), None);
        assert_eq!(like_prefix("T_i%"), None);
        assert_eq!(like_prefix("Ti"), None);
    }

    #[test]
    fn string_functions_match_their_postgres_shapes() {
        assert_eq!(length("naïve"), 5);
        assert_eq!(substring("abcdef", 2, Some(3)).unwrap(), "bcd");
        assert_eq!(substring("abcdef", 2, None).unwrap(), "bcdef");
        assert_eq!(left("abcdef", 2), "ab");
        assert_eq!(left("abcdef", -2), "abcd");
        assert_eq!(right("abcdef", 2), "ef");
        assert_eq!(right("abcdef", -2), "cdef");
        assert_eq!(split_part("a,b,c", ",", 2).unwrap(), "b");
        assert_eq!(split_part("a,b,c", ",", -1).unwrap(), "c");
        assert_eq!(split_part("a,b,c", ",", 9).unwrap(), "");
        assert_eq!(position("abcdef", "cd"), 3);
        assert_eq!(position("abcdef", "zz"), 0);
    }
}
