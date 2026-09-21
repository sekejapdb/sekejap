//! `docs/lang/QL_CONTRACT.md` §4.1 (string) and §4.2 (date/time) under test, with
//! the execution split the contract states.
//!
//! Two questions, asked separately because the contract answers them
//! separately:
//!
//! 1. **A range rewrite is index-side.** `EXTRACT(YEAR FROM t) = 1950`,
//!    `date_trunc('month', t) BETWEEN ...`, `t::date = lit`, `t >= lit`,
//!    `t BETWEEN`, `t > now() - interval '7 days'`, `lower(col) = 'x'`,
//!    `col LIKE 'x%'` and `starts_with(col, 'x')` each become ONE scalar
//!    range on an index. Every one of them is checked against a BRUTE-FORCE
//!    filter over the 2,000-row fixture held in this process: the rewrite is
//!    right only if it admits exactly the rows the definition admits.
//! 2. **A row function is per returned row.** `EXTRACT`, `date_trunc`,
//!    `to_char`, `age`, `lower`, `upper`, `length`, `concat`, `||`,
//!    `substring`, `left`, `right`, `trim`, `split_part`, `replace`,
//!    `position` and `starts_with` are each checked against Rust's own
//!    computation on the same projected values.
//!
//! And the eighth law: a rewrite whose pre-image is a SET of ranges is a
//! membership-set union, which is what `OR` compiles to. That union is not
//! built, so the multi-range forms are REFUSED with the named reason -- never
//! answered by a scan. `EXPLAIN` is asserted to tell the two apart.
//!
//! The fixture is this file's own, in the style of `lang/tests/sqlslice/fixture.rs`
//! and `tests/query_combinations.rs`: a deterministic generator, one
//! collection, and the indexes the rewrites name.

use sekejap_lang::SqlDatabase;
use sekejap_core::collections::{
    verification::{verify_indexed_source, VerificationLimits},
    Database,
};
use sekejap_lang::{SqlError, SqlResult, SqlRow, SqlValue, Tier, MULTI_RANGE_REASON};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use tempfile::TempDir;

const ROWS: usize = 2_000;
const SEED: u64 = 0x464F_5543_4E53_5F31;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Deliberately mixed case: `lower(kind) = 'home'` must find all three
/// spellings of home and `kind = 'home'` must find only one.
const KINDS: [&str; 8] = [
    "Home", "home", "HOME", "Farm", "farm", "Mill", "Port", "port",
];
/// Two names start with `Ti`, so `LIKE 'Ti%'` has a non-trivial answer and a
/// non-trivial complement.
const NAMES: [&str; 8] = [
    "Tiga", "Tiara", "Empat", "Tujuh", "Lima", "Enam", "Delapan", "Sembilan",
];
const WORDS: [&str; 6] = ["kebun", "sekolah", "jembatan", "bengkel", "desa", "kopi"];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn usize(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

/// Howard Hinnant's `days_from_civil`, written out here so the ORACLE does
/// its own arithmetic instead of borrowing the engine's. A test that called
/// the code under test to compute its expected answer would agree with a bug.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Clone, Debug)]
struct Row {
    key: String,
    name: String,
    kind: String,
    descr: String,
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    /// The instant the row stores, in microseconds since the epoch.
    micros: i64,
    /// The literal the INSERT wrote, ISO-8601.
    literal: String,
    /// The same date as a plain `yyyymmdd` integer, with its own ordinary
    /// btree. It exists so a NON-declared, non-expression range can DRIVE a
    /// statement whose other predicate is an expression-index fold.
    born: i64,
    /// Present on even rows, absent on odd ones, so `concat` (which ignores
    /// NULL) and `||` (which propagates it) can be told apart.
    note: Option<String>,
}

fn generate(i: usize, rng: &mut Rng) -> Row {
    let year = 1940 + (i as i64 % 50);
    let month = 1 + (rng.usize(12) as u32);
    let day = 1 + (rng.usize(28) as u32);
    let hour = rng.usize(24) as u32;
    let minute = rng.usize(60) as u32;
    let micros = days_from_civil(year, month, day) * MICROS_PER_DAY
        + i64::from(hour) * 3_600_000_000
        + i64::from(minute) * 60_000_000;
    Row {
        key: format!("k{i:05}"),
        name: format!("{}{}", NAMES[i % NAMES.len()], i % 10),
        kind: KINDS[rng.usize(KINDS.len())].to_owned(),
        descr: format!(
            "{} {} {}",
            WORDS[rng.usize(WORDS.len())],
            WORDS[rng.usize(WORDS.len())],
            WORDS[rng.usize(WORDS.len())]
        ),
        year,
        month,
        day,
        hour,
        minute,
        micros,
        literal: format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z"),
        born: year * 10_000 + i64::from(month) * 100 + i64::from(day),
        note: (i % 2 == 0).then(|| format!("nota{i}")),
    }
}

struct Fixture {
    db: Database,
    rows: Vec<Row>,
    /// The instant the "recent" rows were written against, so a
    /// `now() - interval` predicate has a known answer.
    recent: Vec<String>,
}

fn config() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn now_micros() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
}

/// The civil UTC breakdown of an instant, for the oracle's own arithmetic.
fn civil(micros: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rest = micros - days * MICROS_PER_DAY;
    // The inverse of `days_from_civil`, again written out for the oracle.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        m as u32,
        d as u32,
        (rest / 3_600_000_000) as u32,
        (rest % 3_600_000_000 / 60_000_000) as u32,
        (rest % 60_000_000 / 1_000_000) as u32,
    )
}

fn iso(micros: i64) -> String {
    let (year, month, day, hour, minute, second) = civil(micros);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn build(dir: &TempDir) -> Fixture {
    let mut db = Database::create(&dir.path().join("db"), config()).unwrap();
    db.sql(
        "CREATE TABLE evt (k TEXT PRIMARY KEY, name TEXT, kind TEXT, descr TEXT, \
         born INT, note TEXT, born_ts TIMESTAMPTZ, born_day DATE)",
        &[],
    )
    .unwrap();
    let mut rng = Rng(SEED);
    let mut rows = Vec::with_capacity(ROWS);
    for i in 0..ROWS {
        let row = generate(i, &mut rng);
        // A row with no `note` omits the column entirely, so the field is
        // MISSING rather than a written empty string: that is the value
        // `concat` and `||` disagree about.
        let statement = match &row.note {
            Some(note) => format!(
                "INSERT INTO evt (k, name, kind, descr, born, note, born_ts, born_day) \
                 VALUES ('{}', '{}', '{}', '{}', {}, '{}', '{}', '{:04}-{:02}-{:02}')",
                row.key,
                row.name,
                row.kind,
                row.descr,
                row.born,
                note,
                row.literal,
                row.year,
                row.month,
                row.day
            ),
            None => format!(
                "INSERT INTO evt (k, name, kind, descr, born, born_ts, born_day) \
                 VALUES ('{}', '{}', '{}', '{}', {}, '{}', '{:04}-{:02}-{:02}')",
                row.key,
                row.name,
                row.kind,
                row.descr,
                row.born,
                row.literal,
                row.year,
                row.month,
                row.day
            ),
        };
        db.sql(&statement, &[]).unwrap();
        rows.push(row);
        if (i + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    // Five rows inside the last week, so `t > now() - interval '7 days'` has
    // an answer that is neither every row nor none. The literal is written
    // from one clock read and the predicate reads another a moment later;
    // both sides of the comparison move together, so the answer is stable.
    let base = now_micros();
    let mut recent = Vec::new();
    for (n, days_back) in [1i64, 2, 3, 4, 6].into_iter().enumerate() {
        let key = format!("r{n:05}");
        let micros = base - days_back * MICROS_PER_DAY;
        let (year, month, day, hour, minute, _) = civil(micros);
        let literal = iso(micros);
        let born = year * 10_000 + i64::from(month) * 100 + i64::from(day);
        db.sql(
            &format!(
                "INSERT INTO evt (k, name, kind, descr, born, note, born_ts, born_day) \
                 VALUES ('{key}', 'Recent{n}', 'Home', 'baru baru baru', {born}, 'nota{n}', \
                 '{literal}', '{year:04}-{month:02}-{day:02}')"
            ),
            &[],
        )
        .unwrap();
        // The recent rows are fixture rows like any other, so the brute-force
        // oracle sees them too; a predicate they satisfy must not look like a
        // rewrite that over-admits.
        rows.push(Row {
            key: key.clone(),
            name: format!("Recent{n}"),
            kind: "Home".to_owned(),
            descr: "baru baru baru".to_owned(),
            year,
            month,
            day,
            hour,
            minute,
            // The literal carries whole seconds, so the stored instant is the
            // clock truncated to a second.
            micros: micros - micros.rem_euclid(1_000_000),
            literal,
            born,
            note: Some(format!("nota{n}")),
        });
        recent.push(key);
    }
    db.commit().unwrap();
    for statement in [
        "CREATE INDEX evt_born_ts ON evt USING btree (born_ts)",
        "CREATE INDEX evt_born_day ON evt USING btree (born_day)",
        "CREATE INDEX evt_name ON evt USING btree (name)",
        "CREATE INDEX evt_kind ON evt USING btree (kind)",
        "CREATE INDEX evt_kind_lower ON evt (lower(kind))",
        "CREATE INDEX evt_born ON evt USING btree (born)",
    ] {
        db.sql(statement, &[]).unwrap();
    }
    db.commit().unwrap();
    Fixture { db, rows, recent }
}

fn rows_of(result: SqlResult) -> Vec<SqlRow> {
    match result {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn text(value: &SqlValue) -> String {
    match value {
        SqlValue::Text(text) => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn int(value: &SqlValue) -> i64 {
    match value {
        SqlValue::Int(n) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// The keys one statement returns, sorted, so an answer can be compared with
/// a brute-force filter without depending on the walk's order.
fn keys(f: &mut Fixture, statement: &str) -> Vec<String> {
    let mut out: Vec<String> = rows_of(f.db.sql(statement, &[]).unwrap())
        .iter()
        .map(|row| text(&row.values[0]))
        .collect();
    out.sort();
    out
}

/// The brute-force answer: every fixture row the predicate admits, by
/// definition, with no index and no rewrite in sight.
fn oracle(f: &Fixture, keep: impl Fn(&Row) -> bool) -> Vec<String> {
    let mut out: Vec<String> = f
        .rows
        .iter()
        .filter(|row| keep(row))
        .map(|row| row.key.clone())
        .collect();
    out.sort();
    out
}

// ── §4.2 rewrites: each one equals the brute-force filter ─────────────────

#[test]
fn extract_year_equality_is_one_range_and_equals_the_brute_force_filter() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    for year in [1940i64, 1950, 1975, 1989, 1999] {
        let got = keys(
            &mut f,
            &format!("SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) = {year}"),
        );
        assert_eq!(got, oracle(&f, |row| row.year == year), "year {year}");
    }
    // A year outside the corpus admits nothing, and admits it from the index
    // rather than by reading rows.
    assert!(keys(
        &mut f,
        "SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1800"
    )
    .is_empty());
}

#[test]
fn extract_year_ordering_and_between_are_one_range_each() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) >= 1985"
        ),
        oracle(&f, |row| row.year >= 1985)
    );
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) < 1945"
        ),
        oracle(&f, |row| row.year < 1945)
    );
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) BETWEEN 1950 AND 1959"
        ),
        oracle(&f, |row| (1950..=1959).contains(&row.year))
    );
}

#[test]
fn date_trunc_equality_and_between_equal_the_brute_force_filter() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('year', born_ts) = '1955-01-01'"
        ),
        oracle(&f, |row| row.year == 1955)
    );
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('month', born_ts) BETWEEN '1950-01-01' AND '1950-06-01'"
        ),
        oracle(&f, |row| row.year == 1950 && (1..=6).contains(&row.month))
    );
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('day', born_ts) >= '1990-01-01'"
        ),
        oracle(&f, |row| row.year >= 1990)
    );
    // A literal that is not itself on the unit's boundary can equal no
    // truncation, so the answer is no rows -- which is what Postgres answers
    // and is still ONE range, not a refusal.
    assert!(keys(
        &mut f,
        "SELECT k FROM evt WHERE date_trunc('year', born_ts) = '1955-06-01'"
    )
    .is_empty());
}

#[test]
fn a_bare_comparison_against_a_date_literal_is_a_range() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let cut = days_from_civil(1970, 1, 1) * MICROS_PER_DAY;
    assert_eq!(
        keys(&mut f, "SELECT k FROM evt WHERE born_ts >= '1970-01-01'"),
        oracle(&f, |row| row.micros >= cut)
    );
    let low = days_from_civil(1960, 1, 1) * MICROS_PER_DAY;
    let high = days_from_civil(1965, 1, 1) * MICROS_PER_DAY;
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE born_ts BETWEEN '1960-01-01' AND '1965-01-01'"
        ),
        oracle(&f, |row| row.micros >= low && row.micros <= high)
    );
}

#[test]
fn a_cast_to_date_is_one_days_range() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // A day that at least one row falls on.
    let target = f.rows[7].clone();
    let literal = format!(
        "{:04}-{:02}-{:02}",
        target.year, target.month, target.day
    );
    assert_eq!(
        keys(
            &mut f,
            &format!("SELECT k FROM evt WHERE born_ts::date = '{literal}'")
        ),
        oracle(&f, |row| row.year == target.year
            && row.month == target.month
            && row.day == target.day)
    );
    // The declared DATE column stores midnight, so its cast is the identity.
    assert_eq!(
        keys(
            &mut f,
            &format!("SELECT k FROM evt WHERE born_day = '{literal}'")
        ),
        oracle(&f, |row| row.year == target.year
            && row.month == target.month
            && row.day == target.day)
    );
}

#[test]
fn a_clock_relative_predicate_is_folded_at_prepare_and_is_one_range() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let mut got = keys(
        &mut f,
        "SELECT k FROM evt WHERE born_ts > now() - interval '7 days'",
    );
    got.sort();
    let mut want = f.recent.clone();
    want.sort();
    assert_eq!(got, want, "only the five recent rows are inside the week");
    // `current_date` is the same clock truncated to midnight UTC.
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE born_ts > current_date - interval '30 days'"
        )
        .len(),
        f.recent.len()
    );
}

// ── §4.1 rewrites over text keys ──────────────────────────────────────────

#[test]
fn lower_equality_uses_the_expression_index_and_equals_the_filter() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    for want in ["home", "farm", "port", "mill"] {
        assert_eq!(
            keys(
                &mut f,
                &format!("SELECT k FROM evt WHERE lower(kind) = '{want}'")
            ),
            oracle(&f, |row| row.kind.to_lowercase() == want),
            "lower(kind) = '{want}'"
        );
    }
    // The plain index over the same column still answers the unfolded
    // equality, and the two answers differ -- which is why the expression is
    // part of the index's identity.
    let exact = keys(&mut f, "SELECT k FROM evt WHERE kind = 'home'");
    let folded = keys(&mut f, "SELECT k FROM evt WHERE lower(kind) = 'home'");
    assert_eq!(exact, oracle(&f, |row| row.kind == "home"));
    assert!(exact.len() < folded.len(), "mixed case is in the corpus");
}

#[test]
fn a_prefix_pattern_is_a_text_key_range_and_equals_the_filter() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    assert_eq!(
        keys(&mut f, "SELECT k FROM evt WHERE name LIKE 'Ti%'"),
        oracle(&f, |row| row.name.starts_with("Ti"))
    );
    assert_eq!(
        keys(&mut f, "SELECT k FROM evt WHERE starts_with(name, 'Ti')"),
        oracle(&f, |row| row.name.starts_with("Ti"))
    );
    assert_eq!(
        keys(&mut f, "SELECT k FROM evt WHERE name LIKE 'Tiara%'"),
        oracle(&f, |row| row.name.starts_with("Tiara"))
    );
    assert_eq!(
        keys(&mut f, "SELECT k FROM evt WHERE lower(kind) LIKE 'ho%'"),
        oracle(&f, |row| row.kind.to_lowercase().starts_with("ho"))
    );
}

// ── the refusals: a union with no atomic, and an index that is not there ──

#[test]
fn a_rewrite_that_would_need_a_union_is_refused_with_the_named_reason() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    for statement in [
        "SELECT k FROM evt WHERE EXTRACT(MONTH FROM born_ts) = 6",
        "SELECT k FROM evt WHERE EXTRACT(DAY FROM born_ts) = 1",
        "SELECT k FROM evt WHERE EXTRACT(DOW FROM born_ts) = 0",
        "SELECT k FROM evt WHERE EXTRACT(HOUR FROM born_ts) = 12",
        "SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) <> 1950",
        "SELECT k FROM evt WHERE date_trunc('month', born_ts) <> '1950-01-01'",
    ] {
        let error = f
            .db
            .sql(statement, &[])
            .expect_err(&format!("`{statement}` was not refused"));
        match &error {
            SqlError::Refused { tier, reason, .. } => {
                assert_eq!(*tier, Tier::Two, "{statement}");
                assert_eq!(
                    *reason, MULTI_RANGE_REASON,
                    "`{statement}` is refused for the wrong reason"
                );
            }
            other => panic!("`{statement}` produced {other:?}, not a refusal"),
        }
        // And it is refused, not answered: no rows came back.
        assert!(f.db.sql(statement, &[]).is_err());
    }
}

#[test]
fn a_fold_without_its_expression_index_is_refused_rather_than_scanned() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // There is no `lower(name)` index, only `lower(kind)`.
    let error = f
        .db
        .sql("SELECT k FROM evt WHERE lower(name) = 'tiga0'", &[])
        .expect_err("a fold with no expression index must be refused");
    let text = error.to_string();
    assert!(
        text.contains("expression index") && text.contains("lower(col)"),
        "the refusal must name the index that is missing: {text}"
    );
    // A non-prefix LIKE names the trigram family instead of taking a scan.
    let error = f
        .db
        .sql("SELECT k FROM evt WHERE name LIKE '%iga%'", &[])
        .expect_err("an infix LIKE must be refused");
    assert_eq!(error.tier(), Some(Tier::Two));
    assert!(error.to_string().contains("trigram"), "{error}");
}

// ── §4.1 / §4.2 row functions: each equals Rust's own computation ─────────

#[test]
fn projected_row_functions_equal_rusts_own_computation() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let rows = rows_of(
        f.db.sql(
            "SELECT k, upper(name), lower(name), length(descr), trim(descr), \
             left(name, 2), right(name, 1), split_part(descr, ' ', 2), \
             replace(name, 'i', 'I'), position(descr IN descr), \
             concat(name, '/', kind), name || '-' || kind, starts_with(name, 'Ti'), \
             substring(descr, 2, 4) \
             FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1950",
            &[],
        )
        .unwrap(),
    );
    assert!(!rows.is_empty(), "the year 1950 is in the corpus");
    for row in &rows {
        let key = text(&row.values[0]);
        let want = f.rows.iter().find(|r| r.key == key).expect("a fixture row");
        assert_eq!(text(&row.values[1]), want.name.to_uppercase());
        assert_eq!(text(&row.values[2]), want.name.to_lowercase());
        assert_eq!(int(&row.values[3]), want.descr.chars().count() as i64);
        assert_eq!(text(&row.values[4]), want.descr.trim());
        assert_eq!(
            text(&row.values[5]),
            want.name.chars().take(2).collect::<String>()
        );
        assert_eq!(
            text(&row.values[6]),
            want.name.chars().rev().take(1).collect::<String>()
        );
        assert_eq!(
            text(&row.values[7]),
            want.descr.split(' ').nth(1).unwrap_or_default()
        );
        assert_eq!(text(&row.values[8]), want.name.replace('i', "I"));
        assert_eq!(int(&row.values[9]), 1);
        assert_eq!(
            text(&row.values[10]),
            format!("{}/{}", want.name, want.kind)
        );
        assert_eq!(
            text(&row.values[11]),
            format!("{}-{}", want.name, want.kind)
        );
        assert_eq!(
            row.values[12],
            SqlValue::Bool(want.name.starts_with("Ti")),
            "starts_with"
        );
        assert_eq!(
            text(&row.values[13]),
            want.descr.chars().skip(1).take(4).collect::<String>()
        );
    }
}

#[test]
fn projected_date_functions_equal_rusts_own_computation() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let rows = rows_of(
        f.db.sql(
            "SELECT k, born_ts, born_day, EXTRACT(YEAR FROM born_ts), \
             EXTRACT(MONTH FROM born_ts), EXTRACT(DAY FROM born_ts), \
             EXTRACT(HOUR FROM born_ts), date_trunc('month', born_ts), \
             to_char(born_ts, 'YYYY-MM-DD'), to_char(born_ts, 'YYYY-MM'), \
             to_char(born_ts, 'HH24:MI') \
             FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1961",
            &[],
        )
        .unwrap(),
    );
    assert!(!rows.is_empty(), "the year 1961 is in the corpus");
    for row in &rows {
        let key = text(&row.values[0]);
        let want = f.rows.iter().find(|r| r.key == key).expect("a fixture row");
        // A declared TIMESTAMPTZ prints back as the ISO string it was
        // written as; a declared DATE prints as the date alone.
        assert_eq!(text(&row.values[1]), want.literal);
        assert_eq!(
            text(&row.values[2]),
            format!("{:04}-{:02}-{:02}", want.year, want.month, want.day)
        );
        assert_eq!(int(&row.values[3]), want.year);
        assert_eq!(int(&row.values[4]), i64::from(want.month));
        assert_eq!(int(&row.values[5]), i64::from(want.day));
        assert_eq!(int(&row.values[6]), i64::from(want.hour));
        assert_eq!(
            int(&row.values[7]),
            days_from_civil(want.year, want.month, 1) * MICROS_PER_DAY
        );
        assert_eq!(
            text(&row.values[8]),
            format!("{:04}-{:02}-{:02}", want.year, want.month, want.day)
        );
        assert_eq!(text(&row.values[9]), format!("{:04}-{:02}", want.year, want.month));
        assert_eq!(
            text(&row.values[10]),
            format!("{:02}:{:02}", want.hour, want.minute)
        );
    }
}

#[test]
fn age_and_interval_arithmetic_are_microseconds_over_one_folded_clock() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let before = now_micros();
    let rows = rows_of(
        f.db.sql(
            "SELECT k, age(born_ts), born_ts + interval '1 day', now() \
             FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1943",
            &[],
        )
        .unwrap(),
    );
    let after = now_micros();
    assert!(!rows.is_empty());
    // Every row of one answer sees ONE clock: `now()` is folded at prepare.
    let clock = int(&rows[0].values[3]);
    assert!((before..=after).contains(&clock), "the clock is this call's");
    for row in &rows {
        let key = text(&row.values[0]);
        let want = f.rows.iter().find(|r| r.key == key).unwrap();
        assert_eq!(int(&row.values[1]), clock - want.micros, "age(t) = now() - t");
        assert_eq!(int(&row.values[2]), want.micros + MICROS_PER_DAY);
        assert_eq!(int(&row.values[3]), clock);
    }
}

#[test]
fn a_calendar_interval_is_refused_because_it_folds_to_no_constant() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let error = f
        .db
        .sql(
            "SELECT k FROM evt WHERE born_ts > now() - interval '1 month'",
            &[],
        )
        .expect_err("a month is not a fixed width");
    let text = error.to_string();
    assert!(text.contains("fixed width"), "{text}");
    assert!(text.contains("date_trunc"), "the fix is named: {text}");
}

#[test]
fn an_unnamed_to_char_template_is_refused_at_prepare() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let error = f
        .db
        .sql("SELECT to_char(born_ts, 'Mon DD') FROM evt", &[])
        .expect_err("an unnamed template must be refused");
    assert!(error.to_string().contains("YYYY-MM-DD"), "{error}");
}

// ── EXPLAIN tells a rewrite from a row function ───────────────────────────

#[test]
fn explain_separates_the_range_rewrite_from_the_row_function() {
    let dir = TempDir::new().unwrap();
    let f = build(&dir);
    let plan = f
        .db
        .sql_explain(
            "SELECT upper(name), to_char(born_ts, 'YYYY-MM') FROM evt \
             WHERE EXTRACT(YEAR FROM born_ts) = 1950 AND lower(kind) = 'home'",
            &[],
        )
        .unwrap();
    assert!(plan.contains("range rewrites:"), "{plan}");
    assert!(
        plan.contains("EXTRACT(year FROM born_ts) = 1950 -> scalar range"),
        "the date rewrite is named as a range: {plan}"
    );
    assert!(
        plan.contains("lower(kind) = 'home'") && plan.contains("expression index"),
        "the fold names the index it rode: {plan}"
    );
    assert!(plan.contains("row functions:"), "{plan}");
    assert!(
        plan.contains("upper(name) -> evaluated over this row's projected values"),
        "the string function is named as a row function: {plan}"
    );
    assert!(
        plan.contains("to_char(born_ts, 'YYYY-MM') -> evaluated over this row"),
        "{plan}"
    );
    // The walk is a scalar index, not a scan.
    assert!(plan.contains("driver: Scalar"), "{plan}");
    assert!(!plan.contains("is a SCAN by definition"), "{plan}");

    // One year's range on its own drives, and it walks the year -- not the
    // collection. That is the whole point of the rewrite being index-side.
    let one = f
        .db
        .sql_explain(
            "SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1950",
            &[],
        )
        .unwrap();
    let candidates: usize = one
        .split("candidates=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("the work line carries a candidate count");
    assert!(
        candidates < ROWS / 10,
        "a year's range must not walk the collection: {candidates} candidates of {ROWS}"
    );

    // A statement with no function in either position says so in both lines.
    let plain = f
        .db
        .sql_explain("SELECT k FROM evt WHERE kind = 'Home'", &[])
        .unwrap();
    assert!(plain.contains("range rewrites: none"), "{plain}");
    assert!(plain.contains("row functions: none"), "{plain}");
}

#[test]
fn the_declared_type_survives_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = {
        let f = build(&dir);
        drop(f);
        dir.path().join("db")
    };
    let mut db = Database::open(&path, config()).unwrap();
    // Nothing in the row bytes says `born_ts` is a timestamp -- the catalog
    // descriptor does, and a reopened database still reads it back as one.
    let rows = rows_of(
        db.sql(
            "SELECT k, born_ts FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1948 LIMIT 3",
            &[],
        )
        .unwrap(),
    );
    assert!(!rows.is_empty());
    for row in &rows {
        assert!(
            text(&row.values[1]).starts_with("1948-"),
            "a reopened timestamp prints as ISO text"
        );
    }
}

// ── the fixes this file was extended for ──────────────────────────────────

/// The verifier and the index must agree about what an EXPRESSION index
/// stores.
///
/// `verify_expected` and `verify_actual` recompute the posting a row should
/// have. Computing it from the row's own `kind` rather than from
/// `lower(kind)` reports a healthy `evt_kind_lower` as damage twice over --
/// the expected posting is Missing and the real one is "extra/mismatched" --
/// on every row whose stored value is not already lower case. The fixture's
/// `KINDS` are deliberately mixed case, so most rows are such a row.
#[test]
fn the_verifier_is_clean_over_a_lower_index_with_mixed_case_values() {
    let dir = TempDir::new().unwrap();
    let path = {
        let f = build(&dir);
        // A corpus that would not exercise the bug is not a test of it.
        let mixed = f
            .rows
            .iter()
            .filter(|row| row.kind != row.kind.to_lowercase())
            .count();
        assert!(mixed > 100, "the fixture must hold mixed-case kinds");
        drop(f);
        dir.path().join("db")
    };
    let mut seen: Vec<String> = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        if seen.len() < 8 {
            seen.push(format!("{issue:?}"));
        }
    })
    .unwrap();
    assert!(
        report.clean && report.complete,
        "a healthy lower(kind) index must verify clean; first issues: {seen:?}"
    );
    assert_eq!(report.derived_issues, 0);
    assert_eq!(report.primary_issues, 0);
    assert_eq!(report.catalog_issues, 0);
}

/// An expression predicate that is NOT the driver is still the expression's.
///
/// `born` is an ordinary Int btree over a value no expression touches, and a
/// one-year `born` window is far narrower than `lower(kind) = 'home'`, so the
/// `born` range drives and the fold becomes a per-candidate filter. That
/// filter recomputes the index's value from the row; recomputing `kind`
/// instead of `lower(kind)` silently drops every row whose stored `kind` is
/// not already lower case, which on this fixture is two spellings in three.
#[test]
fn a_non_driving_expression_predicate_equals_the_brute_force_filter() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    for (low, high) in [(19400101i64, 19401231i64), (19500101, 19521231)] {
        let statement = format!(
            "SELECT k FROM evt WHERE lower(kind) = 'home' AND born BETWEEN {low} AND {high}"
        );
        let want = oracle(&f, |row| {
            row.kind.to_lowercase() == "home" && (low..=high).contains(&row.born)
        });
        assert!(!want.is_empty(), "the window must hold rows: {statement}");
        // And the answer must not be the one the raw column would give.
        let raw = oracle(&f, |row| {
            row.kind == "home" && (low..=high).contains(&row.born)
        });
        assert!(
            raw.len() < want.len(),
            "the window must hold mixed-case kinds, or the bug is invisible"
        );
        assert_eq!(keys(&mut f, &statement), want, "{statement}");
        // The `born` range is the driver and the fold is the residual: EXPLAIN
        // names the index the walk rides.
        let plan = f.db.sql_explain(&statement, &[]).unwrap();
        assert!(plan.contains("driver: Scalar"), "{plan}");
        assert!(plan.contains("evt_born"), "the born index drives: {plan}");
    }
    // The prefix form of the same fold, also non-driving.
    let statement =
        "SELECT k FROM evt WHERE lower(kind) LIKE 'ho%' AND born BETWEEN 19400101 AND 19451231";
    assert_eq!(
        keys(&mut f, statement),
        oracle(&f, |row| row.kind.to_lowercase().starts_with("ho")
            && (19400101..=19451231).contains(&row.born)),
        "{statement}"
    );
    // The MEMBERSHIP-SET OVERFLOW path is NOT reachable from this fixture and
    // is not asserted here rather than being faked. A non-driving scalar
    // RANGE builds a `MembershipSet` from the index's own postings
    // (`src/query/membership.rs`), which is the correct path either way; the
    // set degrades to `Overflow` only when one bit per sequence in the
    // collection's span will not fit `MEMBERSHIP_BITMAP_CAP_BYTES` -- about
    // 67 million entities -- or when the caller's `QueryBudget` cannot afford
    // the posting walk at all. This collection spans 2,005 sequences, whose
    // bitmap is 251 bytes, and SQL exposes no budget knob, so neither
    // condition is reachable here. The `Overflow` arm lands in the same
    // `scalar_filter_matches` the equality above already exercises.
}

/// `date_trunc(u, t) OP lit` with `lit` NOT on a `u` boundary.
///
/// The left-hand side only ever takes boundary values, so an interior literal
/// moves every cut to the boundary above it. Truncating the literal and then
/// cutting at ITS boundary returns all of January for `>= '1950-01-15'`,
/// which Postgres excludes, and drops all of January for `< '1950-01-15'`,
/// which Postgres keeps.
#[test]
fn date_trunc_inequalities_with_an_off_boundary_literal_equal_the_oracle() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // A month-start comparison written out by the oracle, with no help from
    // the engine: (year, month) as one ordered number.
    let month_of = |row: &Row| row.year * 100 + i64::from(row.month);
    let jan = 1950 * 100 + 1;
    let feb = 1950 * 100 + 2;
    let jun = 1950 * 100 + 6;

    // `>= '1950-01-15'`: January is EXCLUDED, because 1950-01-01 is not
    // greater than or equal to 1950-01-15.
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('month', born_ts) >= '1950-01-15'"
        ),
        oracle(&f, |row| month_of(row) >= feb),
        "date_trunc >= an interior literal starts at the NEXT month"
    );
    // `< '1950-01-15'`: January is KEPT, because 1950-01-01 is less than
    // 1950-01-15.
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('month', born_ts) < '1950-01-15'"
        ),
        oracle(&f, |row| month_of(row) <= jan),
        "date_trunc < an interior literal keeps the literal's own month"
    );
    // The two that were already right, pinned so they stay right.
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('month', born_ts) <= '1950-01-15'"
        ),
        oracle(&f, |row| month_of(row) <= jan)
    );
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('month', born_ts) > '1950-01-15'"
        ),
        oracle(&f, |row| month_of(row) >= feb)
    );
    // BETWEEN carries the `>=` rule in its lower half and nothing in its
    // upper half: `<= '1950-06-10'` and `<= '1950-06-01'` admit the same
    // truncated instants.
    assert_eq!(
        keys(
            &mut f,
            "SELECT k FROM evt WHERE date_trunc('month', born_ts) \
             BETWEEN '1950-01-15' AND '1950-06-10'"
        ),
        oracle(&f, |row| (feb..=jun).contains(&month_of(row))),
        "BETWEEN's lower bound moves to the next month, its upper does not"
    );
    // An interior literal can never EQUAL a truncation: still one range,
    // still no rows, still not a refusal.
    assert!(keys(
        &mut f,
        "SELECT k FROM evt WHERE date_trunc('month', born_ts) = '1950-01-15'"
    )
    .is_empty());
    // The same rule at day granularity, where `::date` shares the shape.
    let day = f.rows[11].clone();
    let literal = format!("{:04}-{:02}-{:02}", day.year, day.month, day.day);
    assert_eq!(
        keys(
            &mut f,
            &format!("SELECT k FROM evt WHERE date_trunc('day', born_ts) >= '{literal}'")
        ),
        oracle(&f, |row| row.micros
            >= days_from_civil(day.year, day.month, day.day) * MICROS_PER_DAY),
        "a literal that IS on the boundary still starts at the boundary"
    );
}

/// `EXTRACT(YEAR FROM t) = <huge>` is a literal a user can write.
///
/// `days_from_civil(n, 1, 1) * MICROS_PER_DAY` leaves i64 past about year
/// 294,000; the release profile has no overflow checks, so the unchecked
/// form wrapped to a garbage window there and a debug build panicked. The
/// bound is saturating, which is not a clamp of the answer: nothing a column
/// can hold lies outside it.
#[test]
fn an_extract_year_literal_outside_the_representable_range_is_answered_not_wrapped() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    // The largest literals the lexer still reads as whole numbers: past
    // 2^53 it hands back a float and the compare refuses it by name, which
    // is a different (and correct) answer than a wrapped range.
    for year in [300_000i64, 9_000_000_000_000] {
        assert!(
            keys(
                &mut f,
                &format!("SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) = {year}")
            )
            .is_empty(),
            "no row is in year {year}"
        );
        assert_eq!(
            keys(
                &mut f,
                &format!("SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) < {year}")
            )
            .len(),
            f.rows.len(),
            "every row is before year {year}"
        );
    }
    for year in [-300_000i64, -9_000_000_000_000] {
        assert!(keys(
            &mut f,
            &format!("SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) <= {year}")
        )
        .is_empty());
        assert_eq!(
            keys(
                &mut f,
                &format!("SELECT k FROM evt WHERE EXTRACT(YEAR FROM born_ts) > {year}")
            )
            .len(),
            f.rows.len()
        );
    }
}

/// `||` propagates NULL, `concat` ignores it -- Postgres's own split, and a
/// declared timestamp is its ISO spelling on both sides of either.
#[test]
fn concat_ignores_a_missing_value_and_the_operator_propagates_it() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let rows = rows_of(
        f.db.sql(
            "SELECT k, concat(name, note), name || note, born_ts || '', \
             concat(born_ts, ''), born_day || '' \
             FROM evt WHERE born BETWEEN 19400101 AND 19411231",
            &[],
        )
        .unwrap(),
    );
    assert!(!rows.is_empty());
    let mut with_note = 0;
    let mut without = 0;
    for row in &rows {
        let key = text(&row.values[0]);
        let want = f.rows.iter().find(|r| r.key == key).unwrap();
        match &want.note {
            Some(note) => {
                with_note += 1;
                assert_eq!(text(&row.values[1]), format!("{}{note}", want.name));
                assert_eq!(text(&row.values[2]), format!("{}{note}", want.name));
            }
            None => {
                without += 1;
                assert_eq!(text(&row.values[1]), want.name, "concat ignores it");
                assert_eq!(row.values[2], SqlValue::Null, "|| propagates it");
            }
        }
        // A declared timestamp is ISO text on BOTH sides of the split: `||`
        // printed the decimal of its microseconds before this fix while
        // `concat` printed the text.
        assert_eq!(text(&row.values[3]), want.literal);
        assert_eq!(text(&row.values[4]), want.literal);
        assert_eq!(
            text(&row.values[5]),
            format!("{:04}-{:02}-{:02}", want.year, want.month, want.day)
        );
    }
    assert!(with_note > 0 && without > 0, "both shapes are in the answer");
}

/// `SELECT *` prints a declared timestamp the way `SELECT born_ts` does.
#[test]
fn select_star_prints_a_declared_timestamp_as_iso_text() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let result = f
        .db
        .sql(
            "SELECT * FROM evt WHERE born BETWEEN 19400101 AND 19401231 LIMIT 5",
            &[],
        )
        .unwrap();
    let (columns, rows) = match result {
        SqlResult::Rows { columns, rows, .. } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    };
    assert!(!rows.is_empty());
    let at = |name: &str| columns.iter().position(|c| c == name).expect(name);
    let (k, ts, day) = (at("k"), at("born_ts"), at("born_day"));
    for row in &rows {
        let key = text(&row.values[k]);
        let want = f.rows.iter().find(|r| r.key == key).unwrap();
        assert_eq!(text(&row.values[ts]), want.literal, "SELECT * is ISO too");
        assert_eq!(
            text(&row.values[day]),
            format!("{:04}-{:02}-{:02}", want.year, want.month, want.day)
        );
    }
}

/// `GROUP BY <a declared timestamp>` reports the key as ISO text.
#[test]
fn a_group_by_on_a_declared_timestamp_reports_the_key_as_iso_text() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let rows = rows_of(
        f.db.sql(
            "SELECT born_ts, count(*) FROM evt WHERE born BETWEEN 19400101 AND 19401231 \
             GROUP BY born_ts",
            &[],
        )
        .unwrap(),
    );
    assert!(!rows.is_empty());
    let mut counted = 0i64;
    for row in &rows {
        let key = text(&row.values[0]);
        // Every group key is one of the fixture's own literals, spelled the
        // same way `SELECT born_ts` spells it.
        let want = f
            .rows
            .iter()
            .filter(|r| r.literal == key && (19400101..=19401231).contains(&r.born))
            .count();
        assert!(want > 0, "group key `{key}` is not a fixture literal");
        assert_eq!(int(&row.values[1]), want as i64, "group `{key}`");
        counted += want as i64;
    }
    assert_eq!(
        counted,
        f.rows
            .iter()
            .filter(|r| (19400101..=19401231).contains(&r.born))
            .count() as i64
    );
}

/// `age()` against a clock this TEST read, not the engine's own folded one.
#[test]
fn age_lies_between_two_clock_reads_this_test_made() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let before = now_micros();
    let rows = rows_of(
        f.db.sql(
            "SELECT k, age(born_ts) FROM evt WHERE EXTRACT(YEAR FROM born_ts) = 1944",
            &[],
        )
        .unwrap(),
    );
    let after = now_micros();
    assert!(!rows.is_empty());
    for row in &rows {
        let key = text(&row.values[0]);
        let want = f.rows.iter().find(|r| r.key == key).unwrap();
        let age = int(&row.values[1]);
        assert!(
            (before - want.micros..=after - want.micros).contains(&age),
            "age({key}) = {age} is outside the window this test measured"
        );
    }
}

/// `substring` and `split_part` at their edges, against Rust's own answer.
#[test]
fn substring_and_split_part_edges_equal_rusts_own_computation() {
    let dir = TempDir::new().unwrap();
    let mut f = build(&dir);
    let rows = rows_of(
        f.db.sql(
            "SELECT k, substring(name, 0, 3), substring(name, -2, 4), \
             substring(name, 99, 3), substring(name, 2), \
             split_part(descr, '|', 1), split_part(descr, '|', 2), \
             split_part(name, ' ', 1) \
             FROM evt WHERE born BETWEEN 19400101 AND 19401231",
            &[],
        )
        .unwrap(),
    );
    assert!(!rows.is_empty());
    // The oracle: Postgres counts `substring(s, start, count)` over the
    // half-open character window `[start, start + count)` intersected with
    // the string, so a start at or below zero SHORTENS the answer.
    let window = |s: &str, start: i64, count: i64| -> String {
        let chars: Vec<char> = s.chars().collect();
        let from = start.max(1);
        let end = start + count;
        if end <= from {
            return String::new();
        }
        let from = (from - 1) as usize;
        if from >= chars.len() {
            return String::new();
        }
        let to = ((end - 1) as usize).min(chars.len());
        chars[from..to].iter().collect()
    };
    for row in &rows {
        let key = text(&row.values[0]);
        let want = f.rows.iter().find(|r| r.key == key).unwrap();
        assert_eq!(text(&row.values[1]), window(&want.name, 0, 3), "start 0");
        assert_eq!(text(&row.values[2]), window(&want.name, -2, 4), "start -2");
        assert_eq!(text(&row.values[3]), String::new(), "start past the end");
        assert_eq!(
            text(&row.values[4]),
            want.name.chars().skip(1).collect::<String>(),
            "no FOR"
        );
        // A separator the string does not contain: field 1 is the whole
        // string and every later field is empty.
        assert_eq!(text(&row.values[5]), want.descr, "absent separator, n = 1");
        assert_eq!(text(&row.values[6]), String::new(), "absent separator, n = 2");
        assert_eq!(text(&row.values[7]), want.name, "no space in a name");
    }
}

