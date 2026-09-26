//! Guard (owner decision, findings X6/X7): no test, bench or tool may embed
//! a built-in default host, port, user, database, container or password for
//! an EXTERNAL PostgreSQL/PostGIS server ever again. The connection comes
//! from the environment alone -- `SEKEJAP_PG_DSN` / `SEKEJAP_BENCH_PG_DSN`
//! for the Rust suites, the standard libpq variables (`PGHOST`, `PGPORT`,
//! `PGUSER`, `PGDATABASE`, `PGPASSWORD`) plus `SEKEJAP_POSTGIS_CONTAINER`
//! for the shell/Python fixture generators.
//!
//! This scans source text for the shapes the removed defaults actually had.
//! It does not touch `dist/tests` (sekejap's OWN PG-wire protocol server,
//! which legitimately dials `host=127.0.0.1` to reach ITSELF, not an
//! external Postgres) or `core/engine/tests/fixtures` (committed JSON, not
//! source).
use std::fs;
use std::path::{Path, PathBuf};

/// The repository root: `core/engine` has two ancestors.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("core/engine has two ancestors")
        .to_path_buf()
}

/// This file's own path, excluded from the scan: it quotes the forbidden
/// shapes as literal strings in order to look for them.
fn this_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/no_hardcoded_pg_connection_defaults.rs")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("rs" | "py" | "sh")
        ) {
            out.push(path);
        }
    }
}

/// Exact substrings the removed defaults had. Kept literal (no regex
/// dependency) and specific enough not to catch ordinary code: a bare
/// `port=` would also match inside `report=`, so every shape here names a
/// whole default assignment or literal DSN, not a single field.
const LITERAL_PATTERNS: [&str; 7] = [
    "${PGHOST:=",
    "${PGPORT:=",
    "${PGUSER:=",
    "${PGPASSWORD:=",
    "${PGDATABASE:=",
    "unwrap_or_else(|_| \"host=",
    "host=127.0.0.1 port=",
];

/// The Python shape a removed default had: `os.environ.get("PGX", "y")`,
/// a default that is itself a non-empty string literal. A lookup with no
/// second argument -- `os.environ.get("PGX")` or `os.environ["PGX"]` -- is
/// the required-variable form this guard wants to see instead.
fn has_python_default_get(line: &str) -> bool {
    line.contains("os.environ.get(\"PG") && line.contains("\", \"")
}

#[test]
fn no_test_bench_or_tool_hard_codes_an_external_postgres_connection_default() {
    let root = repo_root();
    let excluded = this_file();
    let scan_dirs = [
        root.join("bench"),
        root.join("tools"),
        root.join("core/engine/tests"),
        root.join("core/kernel/tests"),
        root.join("lang/tests"),
    ];
    let mut files = Vec::new();
    for dir in &scan_dirs {
        walk(dir, &mut files);
    }
    let mut offenders = Vec::new();
    for file in files {
        if file == excluded || file.components().any(|c| c.as_os_str() == "fixtures") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            for pattern in LITERAL_PATTERNS {
                if line.contains(pattern) {
                    offenders.push(format!(
                        "{}:{}: hard-coded connection default {pattern:?}",
                        file.display(),
                        number + 1
                    ));
                }
            }
            if has_python_default_get(line) {
                offenders.push(format!(
                    "{}:{}: os.environ.get(\"PG...\", \"...\") supplies a built-in default",
                    file.display(),
                    number + 1
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a connection default is hard-coded again -- settings must come from the \
         environment alone (SEKEJAP_PG_DSN / SEKEJAP_BENCH_PG_DSN / PGHOST / PGPORT / \
         PGUSER / PGDATABASE / PGPASSWORD / SEKEJAP_POSTGIS_CONTAINER):\n{}",
        offenders.join("\n")
    );
}
