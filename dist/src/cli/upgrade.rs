//! `sekejap-upgrade` -- rewrite a database's older index formats into this
//! build's, on purpose (`docs/core/UPGRADE.md`).
//!
//! ```text
//! sekejap-upgrade --check <db-path>
//! sekejap-upgrade --apply <db-path> [--backup <dir>]
//! ```
//!
//! A newer build reads an older database as it is, with no upgrade (Law 8).
//! What an upgrade gives is the newer build's speed on indexes an older
//! release built. It is never automatic: once an index is rewritten, a
//! release that predates its format can no longer open the file, so the
//! choice is the operator's, made here.
//!
//! `--check` opens the database read-only and prints a JSON report: every
//! index, its family, its format (`current` or `older`), the file's logical
//! feature word, and whether each known release can still open the file.
//!
//! A database in the 0.18 FORMAT (written by a 0.18 release) is moved to the
//! 0.19 format first (`docs/core/SUPPORTIVE.md` section 3): `--check` says
//! so, and `--apply` builds the 0.19 file beside it, verifies it, and swaps
//! it in, keeping the original directory untouched as
//! `<db-path>.v018-backup`. 0.19 opens no 0.18-format file until then.
//!
//! `--apply` does nothing when nothing is older. Otherwise it takes the
//! database's writer, copies every file to the backup directory (default
//! `<db-path>.before-upgrade-<unix seconds>`, refused if it exists), then
//! runs `REINDEX INDEX` for each older index -- bounded, resumable steps --
//! and prints the report again.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, IndexFamily, IndexInfo, TextAnalyzer};
use sekejap_core::internal::logical_features;
use sekejap_lang::SqlDatabase;
use serde_json::{json, Value};

/// Released builds and the logical feature mask each opens
/// (`SUPPORTED_LOGICAL_FEATURES` of that release). A file whose feature
/// word has a bit outside a mask is refused by that release.
const RELEASES: &[(&str, u64)] = &[("0.18.3", 0x7f_ffff)];

fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// Is this index in a format older than this build writes? `Some(reason)`
/// when it is. No index format has been superseded yet; the text family's
/// posting segment format 2 (0.19 A2) will be the first entry here.
fn older_format(_db: &Database, _info: &IndexInfo) -> Option<&'static str> {
    None
}

fn family(info: &IndexInfo) -> &'static str {
    match (info.family, info.analyzer) {
        (IndexFamily::Scalar, _) => "btree",
        (IndexFamily::Text, Some(TextAnalyzer::Trigram)) => "trigram",
        (IndexFamily::Text, _) => "text",
        (IndexFamily::SpatialPoint, _) => "spatial_point",
        (IndexFamily::SpatialGeometry, _) => "spatial_geometry",
        (IndexFamily::ExactVector, _) => "exact_vector",
        (IndexFamily::QuantizedVector, _) => "quantized_vector",
        (IndexFamily::VamanaGraph, _) => "vamana_graph",
    }
}

/// The report, and the names of the indexes that are older.
fn report(db: &Database, shown: &str) -> Result<(Value, Vec<String>), String> {
    let features = logical_features(db);
    let mut indexes = Vec::new();
    let mut older = Vec::new();
    for (schema, table) in db.list_qualified_collections().map_err(|e| e.to_string())? {
        let Some(c) = db.collection_in(&schema, &table).map_err(|e| e.to_string())? else {
            continue;
        };
        for info in db.list_indexes(c).map_err(|e| e.to_string())? {
            let why = older_format(db, &info);
            if why.is_some() {
                older.push(info.name.clone());
            }
            indexes.push(json!({
                "table": if schema == "public" { table.clone() } else { format!("{schema}.{table}") },
                "index": info.name,
                "family": family(&info),
                "format": if why.is_some() { "older" } else { "current" },
                "why": why,
            }));
        }
    }
    // A 0.19-format file opens in no 0.18 release, whatever its features.
    let register = db.has_column_ids();
    let readable: serde_json::Map<String, Value> = RELEASES
        .iter()
        .map(|(release, mask)| (release.to_string(), json!(!register && features & !mask == 0)))
        .collect();
    let advice = if older.is_empty() {
        "nothing to upgrade: every index is in this build's format".to_owned()
    } else {
        format!(
            "{} index(es) in an older format; `--apply` rebuilds them after a backup, and afterwards a release older than this build may no longer open the file",
            older.len()
        )
    };
    Ok((
        json!({
            "database": shown,
            "logical_features": format!("{features:#x}"),
            "readable_by": readable,
            "indexes": indexes,
            "older": older.len(),
            "advice": advice,
        }),
        older,
    ))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir(to).map_err(|e| format!("backup `{}`: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_file() {
            fs::copy(entry.path(), to.join(entry.file_name())).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let usage = "usage: sekejap-upgrade --check <db-path>\n       sekejap-upgrade --apply <db-path> [--backup <dir>]";
    let (mode, path) = match args {
        [mode, path, ..] if mode == "--check" || mode == "--apply" => (mode.as_str(), PathBuf::from(path)),
        _ => return Err(usage.into()),
    };
    let shown = path.display().to_string();
    let legacy = sekejap_core::collections::upgrade::is_legacy_format(&path)
        .map_err(|e| format!("read `{shown}`: {e}"))?;
    if legacy && mode == "--check" {
        let value = json!({
            "database": shown,
            "format": "0.18",
            "advice": "this file is in the 0.18 format; `sekejap-upgrade --apply` moves it to the 0.19 format, keeping the original directory as the backup",
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return Ok(());
    }
    if legacy {
        let moved = sekejap_core::collections::upgrade::upgrade_format(&path, Default::default())
            .map_err(|e| format!("upgrade `{shown}`: {e}"))?;
        if let Some(moved) = moved {
            eprintln!("0.19 format: {shown}; the 0.18 original is kept at {}", moved.backup.display());
        }
    }
    if mode == "--check" {
        let db = Database::open_snapshot(&path, cfg()).map_err(|e| format!("open `{shown}`: {e}"))?;
        let (value, _) = report(&db, &shown)?;
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return Ok(());
    }
    let backup = match args.get(2).map(String::as_str) {
        Some("--backup") => PathBuf::from(args.get(3).ok_or(usage)?),
        None => {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            PathBuf::from(format!("{shown}.before-upgrade-{seconds}"))
        }
        Some(_) => return Err(usage.into()),
    };
    let mut db = Database::open(&path, cfg()).map_err(|e| format!("open `{shown}`: {e}"))?;
    let (_, older) = report(&db, &shown)?;
    if older.is_empty() {
        println!("nothing to upgrade: every index is in this build's format; no file changed");
        return Ok(());
    }
    // The writer is held from here on: the copy is the database as it is.
    db.checkpoint().map_err(|e| e.to_string())?;
    copy_dir(&path, &backup)?;
    eprintln!("backup: {}", backup.display());
    for name in &older {
        db.sql(&format!("REINDEX INDEX \"{name}\""), &[])
            .map_err(|e| format!("REINDEX INDEX {name}: {e}"))?;
        eprintln!("rebuilt: {name}");
    }
    db.checkpoint().map_err(|e| e.to_string())?;
    let (value, _) = report(&db, &shown)?;
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sekejap-upgrade: {message}");
            ExitCode::FAILURE
        }
    }
}
