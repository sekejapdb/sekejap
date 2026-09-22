//! Every query example in sekejap's documentation, RUN.
//!
//! A documentation example that nothing executes is a claim, not an example:
//! it drifts the moment the language moves, and the reader who copies it pays
//! for the drift. This file is the harness that stops that. It reads the
//! documentation files named in [`DOCS`] out of the repository, takes their
//! fenced blocks, and either RUNS them or REFUSES to let them exist.
//!
//! ## What the info string means
//!
//! | fence | what the harness does |
//! |---|---|
//! | ```` ```sql ```` | a `;`-separated SCRIPT. Every statement runs on the fixture database and must answer without error. |
//! | ```` ```sql refused ```` | ONE statement that must be REFUSED. The block's first line is `-- refused <SQLSTATE>: <construct>` and the refusal must carry that SQLSTATE. |
//! | ```` ```rust ```` | a RUNNABLE example. It must be claimed by a `<!-- doc_example: <anchor> -->` marker on the line above the fence, and it must equal the body of `#[test] fn doc_<anchor>()` in THIS file, byte for byte after the indentation is trimmed. |
//! | ```` ```rust,signatures ```` | declarations only -- a type, a struct, a trait, a module list. Ignored. |
//! | anything else (`sh`, `text`, `json`, `toml`, `rust,ignore`, no info string) | ignored. |
//!
//! ## The SQL convention, stated once
//!
//! ONE convention, chosen so a doc can show a worked sequence rather than a
//! single line: **a ```` ```sql ```` block is a `;`-separated script**, and
//! its statements run in order against one database. `--` line comments are
//! the language's own (`lang/src/lexer.rs`), so a block can explain itself.
//!
//! A statement that needs parameters is preceded by
//! `-- params: <json array>`, which supplies the `$n` values of the statement
//! AFTER it and of no other. The same convention is written down for doc
//! authors in `docs/lang/EXAMPLE_FIXTURE.md`.
//!
//! ## The fixture
//!
//! One database, built ONCE per test process through the `sekejap` facade,
//! holding TWO SHAPES, because two sets of documents were written against two
//! different ones and both sets are kept:
//!
//! - the small one -- `posts` (12 rows, one column of every `Kind`), `people`
//!   (4 rows, joined to `posts` by typed edges) and `readings` (3 rows, the
//!   one collection with automatic timestamps on). `README.md`, `docs/dist/*`
//!   and `docs/core/*` are written against it.
//! - the wide one -- `place` (200 rows) and a 199-edge `near` chain, which
//!   `docs/lang/QL_CONTRACT.md` §0 declares column for column and whose §8
//!   blocks are written against.
//!
//! Between them they carry a scalar index, an expression index, a text index,
//! a spatial point index, a geometry index, an exact vector index and a
//! quantized one. They share one database and touch none of each other's
//! names. The schema IS documentation: `docs/lang/EXAMPLE_FIXTURE.md`
//! describes what a doc example may assume, and [`build_fixture`] and
//! [`build_place`] below build exactly that.
//!
//! Each documentation file gets its OWN copy of the built database, so a doc
//! that writes -- an INSERT, a DROP -- cannot reach the doc after it. Within
//! one file the blocks run in DOCUMENT ORDER on that one copy, which is what
//! lets `QL_CONTRACT` §8 create `ex_town`, index it, alter it and drop it
//! across separate blocks.

use sekejap::{Db, Error, FieldKind};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

// ── the manifest ──────────────────────────────────────────────────────────

/// The documentation this harness covers, repo-relative.
///
/// A file in this list with no examples is fine. A file NOT in this list is
/// NOT covered: adding documentation with a query in it means adding it here.
const DOCS: &[&str] = &[
    "README.md",
    "core/kernel/README.md",
    "core/engine/README.md",
    "lang/README.md",
    "dist/README.md",
    "dist/rust/README.md",
    "dist/ffi/README.md",
    "docs/dist/RUST_API.md",
    "docs/dist/C_ABI.md",
    "docs/dist/PG_SURFACE.md",
    "docs/dist/WIRE_CONTRACT.md",
    "docs/dist/FFI_CONTRACT.md",
    "docs/dist/OPS_CONTRACT.md",
    "docs/lang/QL_CONTRACT.md",
    "docs/lang/INDEX_CONTRACT.md",
    "docs/lang/EXAMPLE_FIXTURE.md",
    "docs/core/COLLECTIONS.md",
    "docs/core/GRAPH_CONTRACT.md",
    "docs/core/SPATIAL_FUNCTIONS.md",
    "docs/core/V2_BENCHMARK_PROTOCOL.md",
    "docs/TIMESTAMPS.md",
];

/// The repository root: `dist/rust`, two levels up.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("dist/rust has two ancestors")
        .to_path_buf()
}

// ── fences ────────────────────────────────────────────────────────────────

/// One fenced block: its info string, the LINE its fence opens on (1-based,
/// so a failure is one `grep -n` away), its body, and the anchor a
/// `<!-- doc_example: ... -->` marker claims it with.
#[derive(Debug)]
struct Fence {
    info: String,
    line: usize,
    body: String,
    marker: Option<String>,
}

/// Every fenced block of one markdown file, in document order.
fn fences(text: &str) -> Vec<Fence> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < lines.len() {
        let Some(info) = lines[at].strip_prefix("```") else {
            at += 1;
            continue;
        };
        // The marker, if any, is the nearest non-blank line above the fence.
        let mut marker = None;
        let mut above = at;
        while above > 0 {
            above -= 1;
            let previous = lines[above].trim();
            if previous.is_empty() {
                continue;
            }
            if let Some(rest) = previous.strip_prefix("<!-- doc_example:") {
                marker = rest.strip_suffix("-->").map(|name| name.trim().to_owned());
            }
            break;
        }
        let start = at + 1;
        let mut end = start;
        while end < lines.len() && !lines[end].trim_end().starts_with("```") {
            end += 1;
        }
        out.push(Fence {
            info: info.trim().to_owned(),
            line: at + 1,
            body: lines[start..end].join("\n"),
            marker,
        });
        at = end + 1;
    }
    out
}

// ── the SQL convention ────────────────────────────────────────────────────

/// One statement of a script: the line it starts on WITHIN the block
/// (1-based), the parameters a `-- params:` line gave it, and its text.
struct Statement {
    line: usize,
    params: Vec<Value>,
    text: String,
}

/// Split a `;`-separated script. A `;` inside a string literal or a `--`
/// comment is not a separator, which is the whole reason this is written out
/// rather than done with `split(';')`.
fn split_script(block: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut line = 1usize;
    let mut start = 1usize;
    let mut in_string = false;
    let mut in_comment = false;
    let mut characters = block.chars().peekable();
    while let Some(c) = characters.next() {
        if current.trim().is_empty() && !c.is_whitespace() {
            start = line;
        }
        match c {
            '\n' => {
                in_comment = false;
                line += 1;
                current.push(c);
            }
            '\'' if !in_comment => {
                in_string = !in_string;
                current.push(c);
            }
            '-' if !in_string && !in_comment && characters.peek() == Some(&'-') => {
                in_comment = true;
                current.push(c);
            }
            ';' if !in_string && !in_comment => {
                if !is_blank_sql(&current) {
                    out.push((start, current.clone()));
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !is_blank_sql(&current) {
        out.push((start, current));
    }
    out
}

/// Whether a fragment is nothing but whitespace and `--` comments.
fn is_blank_sql(text: &str) -> bool {
    text.lines()
        .all(|line| line.trim().is_empty() || line.trim().starts_with("--"))
}

/// Take the `-- params: [...]` lines off the front of a statement.
fn take_params(line: usize, text: &str) -> Result<Statement, String> {
    let mut params = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("-- params:") {
            let parsed: Value = serde_json::from_str(rest.trim())
                .map_err(|e| format!("`-- params: {}` is not JSON: {e}", rest.trim()))?;
            match parsed {
                Value::Array(items) => params = items,
                other => return Err(format!("`-- params:` takes a JSON ARRAY, found {other}")),
            }
        }
    }
    Ok(Statement {
        line,
        params,
        text: text.trim().to_owned(),
    })
}

/// Every statement of one `sql` block.
fn statements(block: &str) -> Result<Vec<Statement>, String> {
    split_script(block)
        .into_iter()
        .map(|(line, text)| take_params(line, &text))
        .collect()
}

/// The first word of a statement, upper case, with `--` comment lines and
/// blank lines skipped. It decides which `Db` call runs it, and nothing else.
fn leading_word(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        return trimmed
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .find(|word| !word.is_empty())
            .unwrap_or_default()
            .to_ascii_uppercase();
    }
    String::new()
}

/// Run one statement through the facade. A row-returning statement goes to
/// `Db::query`, an `EXPLAIN` to `Db::explain`, everything else to
/// `Db::execute`: the three doors `docs/dist/RUST_API.md` §3 names, chosen by
/// the statement's own first word rather than by trying one and catching the
/// refusal.
fn run(db: &Db, statement: &Statement) -> Result<String, Error> {
    let text = statement.text.as_str();
    let params = statement.params.as_slice();
    match leading_word(text).as_str() {
        "EXPLAIN" => db.explain(text, params).map(|plan| {
            let first = plan.lines().next().unwrap_or_default().to_owned();
            format!("plan: {first}")
        }),
        "SELECT" | "SHOW" | "TABLE" => db.query(text, params).map(|rows| {
            format!(
                "{} row(s), columns {:?}",
                rows.len(),
                rows.column_names()
            )
        }),
        _ => db.execute(text, params).map(|n| format!("{n} row(s)")),
    }
}

/// The SQLSTATE a PostgreSQL client would be given for one refusal.
///
/// Not a second map: `dist/src/pg/types.rs::wire_error` is the one the wire
/// surface uses (`docs/dist/WIRE_CONTRACT.md` §8), and this reaches it
/// through the re-exported `dist` layer so the two can never disagree.
fn sqlstate(error: &Error) -> String {
    use sekejap::dist::pg::types::wire_error;
    use sekejap::dist::service::ServiceError;
    match error {
        Error::Sql(e) => wire_error(&ServiceError::Sql(e.clone())).sqlstate.to_owned(),
        // This crate's own named refusal is a construct with no atomic, which
        // is what `feature_not_supported` means.
        Error::Refused { .. } => "0A000".to_owned(),
        other => format!("<not a refusal: {other}>"),
    }
}

// ── the fixture ───────────────────────────────────────────────────────────

/// The rows of `posts`, as `docs/lang/EXAMPLE_FIXTURE.md` writes them.
const POSTS: usize = 12;
/// The dimension of the `emb` column.
const DIM: usize = 4;
/// The four people, in key order, and the city each is in.
const PEOPLE: [(&str, &str, &str); 4] = [
    ("alice", "Alice", "Jakarta"),
    ("bob", "Bob", "Bandung"),
    ("carol", "Carol", "Surabaya"),
    ("dave", "Dave", "Jakarta"),
];
/// One title per post, in key order.
const TITLES: [&str; POSTS] = [
    "kebun raya",
    "pasar pagi",
    "warung kopi",
    "sawah luas",
    "danau biru",
    "hutan kota",
    "kantor pos",
    "sekolah dasar",
    "jembatan tua",
    "bengkel motor",
    "desa wisata",
    "pasar malam",
];
const CITIES: [&str; 3] = ["jakarta", "bandung", "surabaya"];

/// The longitude and latitude of post `n` (1-based).
fn post_point(n: usize) -> (f64, f64) {
    (106.80 + n as f64 * 0.01, -6.20 - n as f64 * 0.01)
}

// ── the second shape: `place`, of `docs/lang/QL_CONTRACT.md` §0 ───────────

/// The rows of `place`, as `docs/lang/QL_CONTRACT.md` §0 declares them.
const PLACES: usize = 200;
/// The eight values `kind` takes, in the order §0 lists them.
const KINDS: [&str; 8] = [
    "depot", "farm", "home", "mill", "park", "port", "school", "shop",
];
/// The small vocabulary `name` and `body` are written from. `kebun` and
/// `sawah` are the two words §0 promises a `tsquery` matches rows with.
const VOCABULARY: [&str; 8] = [
    "kebun", "sawah", "pasar", "kopi", "danau", "hutan", "desa", "taman",
];
/// The first year of `born`: row `n` (0-based) is born in `FIRST_YEAR + n`,
/// so the 200 rows cover 1900 … 2099 and 1990, 1991 and 1993 are each one row.
const FIRST_YEAR: usize = 1900;

/// The longitude and latitude of place `n` (0-based): a line of 200 points
/// through `(106.82, -6.17)`, which is the point every spatial example in
/// `docs/lang/QL_CONTRACT.md` §8.5 probes with. `p100` sits ON it, so a
/// containment test over `area` has rows; the line spans 0.4° of longitude
/// and 0.2° of latitude, so a 20 km radius admits some rows and not others;
/// and the whole line is inside the envelope `(106, -7, 108, -6)`.
fn place_point(n: usize) -> (f64, f64) {
    let step = n as f64 - 100.0;
    (106.82 + step * 0.002, -6.17 + step * 0.001)
}

/// The `weight` of the `near` edge LEAVING place `n` (0-based).
///
/// Nine values between 0.1 and 0.9, ordered so that the first three hops out
/// of `p000` are heavy and the fourth is light: an inline
/// `WHERE r.weight > 0.2` over `-[:near]->{1,4}` then keeps three hops and
/// prunes the fourth, which is the thing the example is showing.
const WEIGHTS: [f64; 9] = [0.7, 0.8, 0.9, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
fn weight(n: usize) -> f64 {
    WEIGHTS[n % WEIGHTS.len()]
}

/// A small square around one point, as GeoJSON text.
fn square(lon: f64, lat: f64) -> String {
    let d = 0.004;
    format!(
        "{{\"type\":\"Polygon\",\"coordinates\":[[[{:.4},{:.4}],[{:.4},{:.4}],[{:.4},{:.4}],\
         [{:.4},{:.4}],[{:.4},{:.4}]]]}}",
        lon - d,
        lat - d,
        lon + d,
        lat - d,
        lon + d,
        lat + d,
        lon - d,
        lat + d,
        lon - d,
        lat - d
    )
}

/// The embedding of post `n` (1-based): one lane per post, wrapped, so the
/// nearest neighbour of a query vector is known without a search.
fn embedding(n: usize) -> Vec<f64> {
    let mut v = vec![0.0f64; DIM];
    v[(n - 1) % DIM] = 1.0;
    v[n % DIM] = 0.25;
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    v.iter().map(|x| x / norm).collect()
}

/// Build the fixture of `docs/lang/EXAMPLE_FIXTURE.md` at `path`.
///
/// Through the facade and, where a statement can say it, through the SQL the
/// documentation itself uses -- so the declared `TIMESTAMPTZ` and `DATE`
/// spellings reach the catalog, which is the one thing
/// `Db::create_collection` cannot record.
fn build_fixture(path: &Path) -> Result<(), Error> {
    let db = Db::open(path)?;

    db.execute(
        "CREATE TABLE posts (
            title TEXT,
            body TEXT,
            views INT,
            score REAL,
            live BOOLEAN,
            meta JSONB,
            at TIMESTAMPTZ,
            day DATE,
            loc GEOMETRY(Point, 4326),
            area GEOMETRY(Polygon, 4326),
            emb VECTOR(4)
        ) WITH (index: none)",
        &[],
    )?;

    for n in 1..=POSTS {
        let (lon, lat) = post_point(n);
        db.execute(
            "INSERT INTO posts (_key, title, body, views, score, live, meta, at, day, loc, area, emb) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            &[
                json!(format!("p{n:02}")),
                json!(TITLES[n - 1]),
                json!(format!("{} di kota {}", TITLES[n - 1], CITIES[(n - 1) % 3])),
                json!(n as i64 * 10),
                json!(n as f64 / 2.0),
                json!(n % 2 == 0),
                json!({ "author": if n % 2 == 0 { "bob" } else { "alice" }, "pinned": n == 1 }),
                json!(format!("2026-01-{n:02}T09:00:00Z")),
                json!(format!("2026-01-{n:02}")),
                json!(format!(
                    "{{\"type\":\"Point\",\"coordinates\":[{lon:.4},{lat:.4}]}}"
                )),
                json!(square(lon, lat)),
                json!(embedding(n)),
            ],
        )?;
    }

    for ddl in [
        "CREATE INDEX posts_views ON posts USING btree (views)",
        "CREATE INDEX posts_at ON posts USING btree (at)",
        "CREATE INDEX posts_title ON posts USING btree (title)",
        "CREATE INDEX posts_title_lower ON posts (lower(title))",
        "CREATE INDEX posts_body ON posts USING gin (to_tsvector('simple', body))",
        "CREATE INDEX posts_loc ON posts USING gist (loc)",
        "CREATE INDEX posts_area ON posts USING gist (area)",
        "CREATE INDEX posts_emb ON posts USING exact (emb)",
        "CREATE INDEX posts_emb_ann ON posts USING diskann (emb vector_cosine_ops)",
    ] {
        db.execute(ddl, &[])?;
    }

    db.execute("CREATE TABLE people (name TEXT, city TEXT)", &[])?;
    for (key, name, city) in PEOPLE {
        db.execute(
            "INSERT INTO people (_key, name, city) VALUES ($1, $2, $3)",
            &[json!(key), json!(name), json!(city)],
        )?;
    }
    db.execute("CREATE INDEX people_city ON people USING btree (city)", &[])?;

    // The graph: a chain of three `knows` edges, and one cross-collection
    // `wrote` edge per person onto a post.
    db.link(("people", "alice"), "knows", ("people", "bob"))?;
    db.link(("people", "bob"), "knows", ("people", "carol"))?;
    db.link(("people", "carol"), "knows", ("people", "dave"))?;
    for (n, (key, _, _)) in PEOPLE.iter().enumerate() {
        db.link_with(
            ("people", *key),
            "wrote",
            ("posts", format!("p{:02}", n + 1).as_str()),
            &json!({ "year": 2026 }),
        )?;
    }

    // `readings` is the one collection with AUTOMATIC timestamps
    // (`docs/TIMESTAMPS.md`). The option is a `CollectionOptions` field and
    // there is no Tier-1 SQL spelling for it, so it is declared through the
    // engine handle a transaction lends out.
    {
        use sekejap::core::collections::CollectionOptions;
        let mut tx = db.transaction()?;
        tx.database().create_collection(
            "readings",
            vec![
                ("sensor".to_owned(), FieldKind::Text),
                ("celsius".to_owned(), FieldKind::Real),
            ],
            CollectionOptions { timestamps: true },
        )?;
        tx.commit()?;
    }
    db.invalidate_plans();
    for n in 1..=3 {
        db.put(
            ("readings", format!("r{n:02}").as_str()),
            &json!({ "sensor": format!("s{n}"), "celsius": 20.0 + n as f64 }),
        )?;
    }

    build_place(&db)?;

    db.close()
}

/// Build `place` and its `near` chain, the second shape of
/// `docs/lang/EXAMPLE_FIXTURE.md` and the one `docs/lang/QL_CONTRACT.md` §0
/// declares: 200 rows, keys `p000` … `p199`, every column of that section's
/// table, every index family it names, and an edge type carrying a property.
fn build_place(db: &Db) -> Result<(), Error> {
    db.execute(
        "CREATE TABLE place (
            name TEXT,
            body TEXT,
            kind TEXT,
            born INT,
            rating DOUBLE PRECISION,
            active BOOLEAN,
            at TIMESTAMPTZ,
            day DATE,
            loc GEOMETRY(Point, 4326),
            area GEOMETRY(Polygon, 4326),
            emb VECTOR(4),
            tag TEXT,
            note TEXT
        ) WITH (index: none)",
        &[],
    )?;

    // The two INSERT spellings are the `tag` column: every fifth row LEAVES
    // IT OUT, so `tag IS MISSING` has rows to find and is a different
    // question from `rating IS NULL`, whose column IS written, as null.
    let with_tag = "INSERT INTO place \
        (_key, name, body, kind, born, rating, active, at, day, loc, area, emb, note, tag) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)";
    let without_tag = "INSERT INTO place \
        (_key, name, body, kind, born, rating, active, at, day, loc, area, emb, note) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)";

    for n in 0..PLACES {
        let (lon, lat) = place_point(n);
        let year = FIRST_YEAR + n;
        let word = VOCABULARY[n % VOCABULARY.len()];
        let next = VOCABULARY[(n + 1) % VOCABULARY.len()];
        let mut params = vec![
            json!(format!("p{n:03}")),
            json!(format!("{word} {n:03}")),
            json!(format!("{word} {next} di kota {}", CITIES[n % 3])),
            json!(KINDS[n % KINDS.len()]),
            json!(year as i64),
            // Every seventh row's `rating` is WRITTEN as null.
            if n % 7 == 0 {
                Value::Null
            } else {
                json!((n % 50) as f64 / 10.0)
            },
            json!(n % 2 == 0),
            json!(format!("{year:04}-06-15T09:00:00Z")),
            json!(format!("{year:04}-06-15")),
            json!(format!(
                "{{\"type\":\"Point\",\"coordinates\":[{lon:.4},{lat:.4}]}}"
            )),
            json!(square(lon, lat)),
            json!(embedding(n + 1)),
            json!(format!("note {n:03}")),
        ];
        let sql = if n % 5 == 0 {
            without_tag
        } else {
            params.push(json!(format!("t{}", n % 4)));
            with_tag
        };
        db.execute(sql, &params)?;
    }

    for ddl in [
        "CREATE INDEX place_name ON place USING btree (name)",
        "CREATE INDEX place_body ON place USING gin (to_tsvector('simple', body))",
        "CREATE INDEX place_kind ON place USING btree (kind)",
        "CREATE INDEX place_kind_lower ON place (lower(kind))",
        "CREATE INDEX place_born ON place USING btree (born)",
        "CREATE INDEX place_rating ON place USING btree (rating)",
        "CREATE INDEX place_active ON place USING btree (active)",
        "CREATE INDEX place_at ON place USING btree (at)",
        "CREATE INDEX place_day ON place USING btree (day)",
        "CREATE INDEX place_loc ON place USING gist (loc)",
        "CREATE INDEX place_area ON place USING gist (area)",
        "CREATE INDEX place_emb ON place USING exact (emb)",
        "CREATE INDEX place_emb_ann ON place USING quantized (emb vector_cosine_ops)",
        "CREATE INDEX place_tag ON place USING btree (tag)",
    ] {
        db.execute(ddl, &[])?;
    }
    // `note` is deliberately left WITHOUT an index, so a predicate over it is
    // refused rather than demoted to a scan and a doc example can show that.
    // That is what `WITH (index: none)` on the CREATE TABLE above is for:
    // `docs/lang/INDEX_CONTRACT.md` indexes every eligible column of a table
    // by default, and the fixture declares its indexes BY HAND so the tables
    // in `docs/lang/EXAMPLE_FIXTURE.md` are exactly what the database holds
    // and `note` has nothing over it.

    // The `near` chain of §0, in the base graph context: `p000 -> p001 -> …
    // -> p199`, 199 edges, each carrying its own `weight`.
    for n in 0..PLACES - 1 {
        db.link_with(
            ("place", format!("p{n:03}").as_str()),
            "near",
            ("place", format!("p{:03}", n + 1).as_str()),
            &json!({ "weight": weight(n) }),
        )?;
    }
    Ok(())
}

/// Copy a built database directory, so one document's writes cannot reach
/// the next document.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The built fixture, once per test process, and a fresh copy per caller.
struct Fixture {
    _dir: tempfile::TempDir,
    template: PathBuf,
    copies: std::cell::Cell<usize>,
}

impl Fixture {
    fn build() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let template = dir.path().join("template");
        build_fixture(&template).expect("the fixture of docs/lang/EXAMPLE_FIXTURE.md builds");
        Self {
            _dir: dir,
            template,
            copies: std::cell::Cell::new(0),
        }
    }

    /// A private database with the fixture's contents.
    fn open(&self) -> Db {
        let n = self.copies.get();
        self.copies.set(n + 1);
        let work = self._dir.path().join(format!("copy{n:03}"));
        copy_dir(&self.template, &work).expect("copy the fixture");
        Db::open(&work).expect("open the fixture copy")
    }
}

// ── the tests ─────────────────────────────────────────────────────────────

#[test]
fn the_manifest_names_only_files_that_exist() {
    let root = repo_root();
    let missing: Vec<&str> = DOCS
        .iter()
        .copied()
        .filter(|doc| !root.join(doc).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "DOCS names {} file(s) that are not in the repository: {missing:?}",
        missing.len()
    );
}

#[test]
fn every_sql_example_in_the_documentation_runs_on_the_fixture() {
    let root = repo_root();
    let fixture = Fixture::build();
    let mut failures: Vec<String> = Vec::new();
    let mut counts: Vec<(String, usize, usize)> = Vec::new();

    for doc in DOCS {
        let path = root.join(doc);
        let Ok(text) = fs::read_to_string(&path) else {
            failures.push(format!("{doc}: cannot be read"));
            continue;
        };
        let blocks = fences(&text);
        let mut answered = 0usize;
        let mut refused = 0usize;
        let mut db: Option<Db> = None;
        for block in &blocks {
            match block.info.as_str() {
                "sql" => {
                    let handle = db.get_or_insert_with(|| fixture.open());
                    match statements(&block.body) {
                        Err(e) => failures.push(format!("{doc}:{}: {e}", block.line)),
                        Ok(list) => {
                            for statement in list {
                                let at = block.line + statement.line;
                                match run(handle, &statement) {
                                    Ok(_) => answered += 1,
                                    Err(e) => failures.push(format!(
                                        "{doc}:{at}: the statement did not answer: {e}\n    \
                                         {}",
                                        one_line(&statement.text)
                                    )),
                                }
                            }
                        }
                    }
                }
                "sql refused" => {
                    let handle = db.get_or_insert_with(|| fixture.open());
                    match expect_refusal(handle, block) {
                        Ok(()) => refused += 1,
                        Err(e) => failures.push(format!("{doc}:{}: {e}", block.line)),
                    }
                }
                _ => {}
            }
        }
        counts.push((doc.to_string(), answered, refused));
    }

    for (doc, answered, refused) in &counts {
        println!("{doc}: {answered} statement(s) answered, {refused} refused by name");
    }
    let answered: usize = counts.iter().map(|(_, a, _)| a).sum();
    let refused: usize = counts.iter().map(|(_, _, r)| r).sum();
    println!("total: {answered} statement(s) answered, {refused} refused by name");

    assert!(
        failures.is_empty(),
        "{} documentation example(s) did not do what the doc says:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// One `sql refused` block: the first line names the SQLSTATE and the
/// construct, and the statement under it must be refused with that SQLSTATE.
fn expect_refusal(db: &Db, block: &Fence) -> Result<(), String> {
    let mut lines = block.body.lines();
    let header = lines
        .next()
        .ok_or("a `sql refused` block is empty")?
        .trim()
        .to_owned();
    let claim = header
        .strip_prefix("-- refused ")
        .ok_or_else(|| format!("the first line of a `sql refused` block is `-- refused <SQLSTATE>: <construct>`, found `{header}`"))?;
    let (code, construct) = claim
        .split_once(':')
        .ok_or_else(|| format!("`-- refused {claim}` has no `: <construct>`"))?;
    let (code, construct) = (code.trim(), construct.trim());
    let rest: String = lines.collect::<Vec<_>>().join("\n");
    let statement = take_params(1, &rest)?;
    if statement.text.trim().is_empty() {
        return Err(format!("`-- refused {code}: {construct}` has no statement under it"));
    }
    match run(db, &statement) {
        Ok(answer) => Err(format!(
            "`{}` was expected to be refused {code} ({construct}) and ANSWERED: {answer}",
            one_line(&statement.text)
        )),
        Err(e) => {
            let got = sqlstate(&e);
            if got == code {
                Ok(())
            } else {
                Err(format!(
                    "`{}` was refused {got}, not {code} ({construct}): {e}",
                    one_line(&statement.text)
                ))
            }
        }
    }
}

/// A statement on one line, for a failure message that stays greppable.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_runnable_rust_example_is_a_test_in_this_file_and_has_not_drifted() {
    let root = repo_root();
    let source = include_str!("doc_examples.rs");
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for doc in DOCS {
        let Ok(text) = fs::read_to_string(root.join(doc)) else {
            continue;
        };
        for block in fences(&text) {
            if block.info != "rust" {
                continue;
            }
            let Some(anchor) = &block.marker else {
                failures.push(format!(
                    "{doc}:{}: a ```rust block with no `<!-- doc_example: <anchor> -->` above it. \
                     A runnable example is claimed by a test in dist/rust/tests/doc_examples.rs; \
                     a block that only DECLARES things is tagged ```rust,signatures",
                    block.line
                ));
                continue;
            };
            match test_body(source, anchor) {
                None => failures.push(format!(
                    "{doc}:{}: claims `doc_{anchor}`, and dist/rust/tests/doc_examples.rs has no \
                     `#[test] fn doc_{anchor}()`",
                    block.line
                )),
                Some(body) => {
                    checked += 1;
                    if body.trim_end() != block.body.trim_end() {
                        failures.push(format!(
                            "{doc}:{}: the block and `doc_{anchor}` have drifted apart.\n\
                             --- the doc says ---\n{}\n--- the test runs ---\n{}\n---",
                            block.line,
                            block.body.trim_end(),
                            body.trim_end()
                        ));
                    }
                }
            }
        }
    }

    println!("{checked} runnable Rust example(s) checked against their tests");
    assert!(
        failures.is_empty(),
        "{} Rust example(s) are not proved by a test:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The body of `#[test] fn doc_<anchor>()` in this file's own source, with
/// one level of indentation taken off.
fn test_body(source: &str, anchor: &str) -> Option<String> {
    let needle = format!("\nfn doc_{anchor}(");
    let at = source.find(&needle)?;
    let open = source[at..].find('{')? + at;
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut close = open;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = open + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &source[open + 1..close];
    let mut out = String::new();
    for line in inner.lines() {
        if out.is_empty() && line.trim().is_empty() {
            continue;
        }
        out.push_str(line.strip_prefix("    ").unwrap_or(line));
        out.push('\n');
    }
    Some(out)
}

// ── the runnable Rust examples ────────────────────────────────────────────
//
// Each body below is, byte for byte, the fenced block the named document
// shows. The test above proves that; `cargo test` proves the block compiles
// and runs. A change to either one without the other fails the test above
// and names both halves.

/// `README.md`, "Using sekejap 0.17".
#[test]
fn doc_readme_quickstart() -> Result<(), sekejap::Error> {
    use sekejap::{Db, Direction, FieldKind};
    use serde_json::json;

    // A sekejap database is a DIRECTORY, created on first open.
    let dir = std::env::temp_dir().join("sekejap-readme");
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir)?;

    db.create_collection(
        "dishes",
        &[("name", FieldKind::Text), ("price", FieldKind::Int)],
    )?;
    db.put(("dishes", "laksa"), &json!({ "name": "Laksa", "price": 1200 }))?;
    db.put(("dishes", "satay"), &json!({ "name": "Satay", "price": 900 }))?;

    // A predicate is answered INDEX-SIDE or refused: sekejap never takes a
    // scan silently, so a column a WHERE names carries an index.
    db.execute("CREATE INDEX dishes_price ON dishes USING btree (price)", &[])?;

    // SQL with `$n` parameters. Every write above is already durable.
    let rows = db.query(
        "SELECT _key, name, price FROM dishes WHERE price < $1 ORDER BY price DESC",
        &[json!(1500)],
    )?;
    assert_eq!(rows.len(), 2);
    for row in rows.iter() {
        println!("{:?}", row.to_object());
    }

    // A scan is lazy: one page of rows at a time, never the whole collection.
    let mut seen = 0;
    for dish in db.scan("dishes")?.page_size(1) {
        let dish = dish?;
        seen += 1;
        println!("{} {}", dish.key, dish.fields["price"]);
    }
    assert_eq!(seen, 2);

    // Edges are typed and directed, and both endpoints must already exist.
    db.create_collection("cooks", &[("name", FieldKind::Text)])?;
    db.put(("cooks", "ayu"), &json!({ "name": "Ayu" }))?;
    db.link(("cooks", "ayu"), "makes", ("dishes", "laksa"))?;

    let made = db.neighbours(
        ("cooks", "ayu"),
        Some("makes"),
        Direction::Outgoing,
        16,
    )?;
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].key, "laksa");

    db.close()?;
    Ok(())
}

/// `dist/rust/README.md`, the crates.io front page.
#[test]
fn doc_crate_readme_quickstart() -> Result<(), sekejap::Error> {
    use sekejap::{Db, FieldKind};
    use serde_json::json;

    let dir = std::env::temp_dir().join("sekejap-crate-readme");
    let _ = std::fs::remove_dir_all(&dir);

    let db = Db::open(&dir)?;
    db.create_collection(
        "dishes",
        &[("name", FieldKind::Text), ("price", FieldKind::Int)],
    )?;
    db.put(("dishes", "laksa"), &json!({ "name": "Laksa", "price": 1200 }))?;
    db.execute("CREATE INDEX dishes_price ON dishes USING btree (price)", &[])?;

    let rows = db.query(
        "SELECT name, price FROM dishes WHERE price < $1",
        &[json!(2000)],
    )?;
    for row in rows.iter() {
        println!("{:?} {:?}", row.value("name"), row.value("price"));
    }
    assert_eq!(rows.len(), 1);
    Ok(())
}

/// `docs/dist/RUST_API.md` §3, a query with parameters.
#[test]
fn doc_rust_api_parameters() -> Result<(), sekejap::Error> {
    use sekejap::{Db, FieldKind};
    use serde_json::json;

    let dir = std::env::temp_dir().join("sekejap-api-parameters");
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir)?;
    db.create_collection(
        "posts",
        &[("title", FieldKind::Text), ("views", FieldKind::Int)],
    )?;
    for n in 1..=5 {
        db.execute(
            "INSERT INTO posts (_key, title, views) VALUES ($1, $2, $3)",
            &[json!(format!("p{n}")), json!(format!("post {n}")), json!(n * 10)],
        )?;
    }
    db.execute("CREATE INDEX posts_views ON posts USING btree (views)", &[])?;

    // One statement, two bindings. The second is a plan-cache HIT, which
    // REBINDS the compiled plan rather than parsing and compiling again.
    let sql = "SELECT _key, title FROM posts WHERE views >= $1 ORDER BY views DESC LIMIT 2";
    let busy = db.query(sql, &[json!(30)])?;
    let busier = db.query(sql, &[json!(40)])?;
    assert_eq!(busy.len(), 2);
    assert_eq!(busier.len(), 2);
    assert_eq!(db.cache_stats().hits, 1);

    // `Db::prepare` is the same mechanism made explicit: ONE parse for the
    // life of the handle, and `counters()` reports (binds, compiles).
    let mut statement = db.prepare("SELECT _key FROM posts WHERE views = $1")?;
    for n in 1..=5 {
        assert_eq!(statement.query_with(&[json!(n * 10)])?.len(), 1);
    }
    assert_eq!(statement.counters(), (5, 1));
    Ok(())
}

/// `docs/dist/RUST_API.md` §6, many writes under one barrier.
#[test]
fn doc_rust_api_transaction() -> Result<(), sekejap::Error> {
    use sekejap::{Db, FieldKind};
    use serde_json::json;

    let dir = std::env::temp_dir().join("sekejap-api-transaction");
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir)?;
    db.create_collection("accounts", &[("balance", FieldKind::Int)])?;

    // Many writes, ONE barrier. A `Tx` dropped without `commit` rolls back.
    let mut tx = db.transaction()?;
    tx.put(("accounts", "a"), &json!({ "balance": 100 }))?;
    tx.put(("accounts", "b"), &json!({ "balance": 0 }))?;
    tx.execute(
        "UPDATE accounts SET balance = $1 WHERE _key = $2",
        &[json!(40), json!("a")],
    )?;
    tx.commit()?;

    assert_eq!(db.get(("accounts", "a"))?, Some(json!({ "_key": "a", "balance": 40 })));

    // The other half of the bargain: nothing this transaction wrote survives.
    let mut discarded = db.transaction()?;
    discarded.put(("accounts", "c"), &json!({ "balance": 7 }))?;
    discarded.rollback()?;
    assert_eq!(db.get(("accounts", "c"))?, None);
    Ok(())
}

/// `docs/core/COLLECTIONS.md`, the typed collection API.
#[test]
fn doc_collections_typed_collection() -> Result<(), Box<dyn std::error::Error>> {
    use sekejap::core::collections::{CollectionOptions, Database};
    use sekejap::core::Kind;
    use sekejap::Db;
    use serde_json::json;

    let dir = std::env::temp_dir().join("sekejap-collections-example");
    let _ = std::fs::remove_dir_all(&dir);
    // `Db::config()` is `SyncMode::Full`, which is the one barrier the
    // page-WAL store accepts: a weaker mode is refused, never downgraded.
    let mut db = Database::create(&dir, Db::config())?;

    let people = db.create_collection(
        "people",
        vec![
            ("name".into(), Kind::Text),
            ("profile".into(), Kind::Json),
            ("position".into(), Kind::Point),
            ("embedding".into(), Kind::Vector(3)),
        ],
        CollectionOptions { timestamps: true }, // Default::default() means OFF
    )?;
    // A field the declaration does not name -- `observed_at` -- is kept in
    // the row's extras map: a declaration is a floor, not a fence.
    let id = db.put(
        people,
        "person/alice",
        &json!({
            "name": "Alice",
            "profile": { "roles": ["operator"], "sensor": null },
            "position": { "type": "Point", "coordinates": [144.75, -37.5] },
            "embedding": [0.5, 1.0, -2.0],
            "observed_at": 1788888800,
        }),
    )?;
    db.commit()?;

    // A shallow merge. Identity survives it, and the unchanged vector
    // sidecar is not rewritten.
    db.update(people, "person/alice", &json!({ "name": "Alice Tan" }))?;
    db.commit()?;
    let row = db.get(people, "person/alice")?.expect("the row");
    assert_eq!(row.id, id);
    assert_eq!(row.document["name"], json!("Alice Tan"));
    assert_eq!(row.document["embedding"], json!([0.5, 1.0, -2.0]));

    // The managed fields are the engine's, written because this collection
    // was created with `timestamps: true`.
    assert!(row.document.get("_created_unix").is_some());
    assert!(row.document.get("_updated_unix").is_some());

    // One entity at a time: a scan never holds the collection.
    for entity in db.scan(people, None)? {
        let entity = entity?;
        println!("{}", entity.key);
    }
    Ok(())
}

/// `docs/TIMESTAMPS.md`, the option as it is spelled today.
#[test]
fn doc_timestamps_options() -> Result<(), sekejap::Error> {
    use sekejap::core::collections::CollectionOptions;
    use sekejap::{Db, FieldKind};

    let dir = std::env::temp_dir().join("sekejap-timestamps-example");
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir)?;

    // OFF: `Db::create_collection` and `CREATE TABLE` both take the default.
    db.create_collection("sensor_readings", &[("celsius", FieldKind::Real)])?;
    assert!(!db.describe("sensor_readings")?.expect("declared").timestamps);

    // ON: the option is a `CollectionOptions` field, and there is no Tier-1
    // SQL spelling for it, so it is set through the engine handle a
    // transaction lends out.
    let mut tx = db.transaction()?;
    tx.database().create_collection(
        "people",
        vec![("name".to_owned(), FieldKind::Text)],
        CollectionOptions { timestamps: true },
    )?;
    tx.commit()?;
    db.invalidate_plans();
    assert!(db.describe("people")?.expect("declared").timestamps);
    Ok(())
}


