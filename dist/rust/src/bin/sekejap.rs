//! sekejap -- the SQL shell.
//!
//! ```text
//! sekejap <path>                 open a database on disk and read SQL
//! sekejap <path> "<SQL>"         run the statements, print the answers, exit
//! echo "SELECT ...;" | sekejap <path>
//! ```
//!
//! e1 shipped this as `skcli`; this is the same shell over e4's public
//! `sekejap::Db`, and nothing else. A statement that returns rows (`SELECT`,
//! `SHOW`) is printed as a table, `EXPLAIN` prints its plan, and every other
//! statement is run and committed and prints how many rows it moved.
//! Statements are separated by `;`, outside quotes and `--` comments.

use sekejap::{value_to_json, Db, Rows};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::time::Instant;

const MAX_COL_WIDTH: usize = 52;

fn usage() -> String {
    format!(
        "sekejap {}

USAGE:
  sekejap <path>              open a database on disk (created if missing) and read SQL
  sekejap <path> \"<SQL>\"      run the statements and exit
  echo \"SQL;\" | sekejap <path>

In the shell, end a statement with `;`. `\\q` leaves.

OPTIONS:
  -h, --help       show this help
  -V, --version    show the version",
        env!("CARGO_PKG_VERSION")
    )
}

fn main() {
    let mut path = None;
    let mut sql = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                return;
            }
            "-V" | "--version" => {
                println!("sekejap {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ if path.is_none() => path = Some(arg),
            _ if sql.is_none() => sql = Some(arg),
            _ => {
                eprintln!("error: unexpected argument `{arg}`\n\n{}", usage());
                std::process::exit(2);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("{}", usage());
        std::process::exit(2);
    };
    let db = match Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open `{path}`: {e}");
            std::process::exit(1);
        }
    };
    let failed = if let Some(sql) = sql {
        run_script(&db, &sql)
    } else if !io::stdin().is_terminal() {
        let mut script = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut script) {
            eprintln!("error: reading standard input: {e}");
            std::process::exit(1);
        }
        run_script(&db, &script)
    } else {
        shell(&db, &path);
        false
    };
    if failed {
        std::process::exit(1);
    }
}

/// Every statement of `script`, in order. Returns whether any failed; the
/// rest still run, as a psql script does.
fn run_script(db: &Db, script: &str) -> bool {
    let mut failed = false;
    for statement in split(script) {
        failed |= !run(db, &statement);
    }
    failed
}

fn shell(db: &Db, path: &str) {
    println!(
        "sekejap {} -- {path}\nEnd a statement with `;`. `\\q` leaves.",
        env!("CARGO_PKG_VERSION")
    );
    let stdin = io::stdin();
    let mut pending = String::new();
    loop {
        print!("{}", if pending.is_empty() { "sekejap> " } else { "     ... " });
        let _ = io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if pending.is_empty() && matches!(line.trim(), "\\q" | "quit" | "exit") {
            break;
        }
        pending.push_str(&line);
        let statements = split(&pending);
        if ends_statement(&pending) {
            for statement in statements {
                run(db, &statement);
            }
            pending.clear();
        }
    }
}

/// Run one statement and print its answer. Returns whether it succeeded.
fn run(db: &Db, statement: &str) -> bool {
    let started = Instant::now();
    let first = statement
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let outcome = match first.as_str() {
        // `Db::query` answers rows and `Db::execute` commits a write. The
        // choice is made up front: trying one and falling back to the other
        // could run a write twice.
        "SELECT" | "SHOW" | "WITH" | "VALUES" | "TABLE" => {
            db.query(statement, &[]).map(|rows| print_rows(&rows))
        }
        "EXPLAIN" => db.explain(statement, &[]).map(|plan| println!("{plan}")),
        _ => db.execute(statement, &[]).map(|moved| {
            println!("{}", if moved == 1 { "1 row".into() } else { format!("{moved} rows") })
        }),
    };
    match outcome {
        Ok(()) => {
            println!("[{}]", elapsed(started));
            true
        }
        Err(e) => {
            eprintln!("error: {e}");
            false
        }
    }
}

fn print_rows(rows: &Rows) {
    let columns = rows.column_names();
    if rows.is_empty() {
        println!("(0 rows)");
        return;
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.values
                .iter()
                .map(|value| match value_to_json(value) {
                    serde_json::Value::String(s) => truncate(&s),
                    serde_json::Value::Null => String::new(),
                    other => truncate(&other.to_string()),
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            cells
                .iter()
                .map(|row| row[i].chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let rule = |left: &str, mid: &str, right: &str| {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        println!("{left}{}{right}", parts.join(mid));
    };
    let line = |values: &[String]| {
        let parts: Vec<String> = values
            .iter()
            .zip(&widths)
            .map(|(v, w)| format!(" {v}{} ", " ".repeat(w - v.chars().count())))
            .collect();
        println!("│{}│", parts.join("│"));
    };
    rule("┌", "┬", "┐");
    line(columns);
    rule("├", "┼", "┤");
    for row in &cells {
        line(row);
    }
    rule("└", "┴", "┘");
    let n = rows.len();
    println!("{}", if n == 1 { "1 row".into() } else { format!("{n} rows") });
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_COL_WIDTH {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(MAX_COL_WIDTH - 1).collect();
        format!("{cut}…")
    }
}

fn elapsed(started: Instant) -> String {
    let ns = started.elapsed().as_nanos();
    if ns < 1_000_000 {
        format!("{:.2} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.3} s", ns as f64 / 1_000_000_000.0)
    }
}

/// The statements of `script`: split at `;` outside a quoted string and
/// outside a `--` comment, trimmed, empty ones dropped.
fn split(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '-' if chars.peek() == Some(&'-') => {
                    // A comment runs to the end of the line.
                    for next in chars.by_ref() {
                        if next == '\n' {
                            current.push('\n');
                            break;
                        }
                    }
                }
                ';' => {
                    let statement = current.trim();
                    if !statement.is_empty() {
                        out.push(statement.to_owned());
                    }
                    current.clear();
                }
                _ => current.push(c),
            },
        }
    }
    let statement = current.trim();
    if !statement.is_empty() {
        out.push(statement.to_owned());
    }
    out
}

/// Whether the text typed so far ends a statement: its last character outside
/// quotes and comments is `;`.
fn ends_statement(text: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut last = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '-' if chars.peek() == Some(&'-') => {
                    for next in chars.by_ref() {
                        if next == '\n' {
                            break;
                        }
                    }
                }
                c if c.is_whitespace() => {}
                c => last = Some(c),
            },
        }
    }
    quote.is_none() && last == Some(';')
}
