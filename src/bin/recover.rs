//! Explicit, source-preserving administrative recovery commands.
use kernel::{
    io::{open_recovery_source, IoMode},
    meta::Meta,
    page::{PageKind, PageRef, PAGE_SIZE},
    recover::recover_to,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{fs, io::Write, path::Path, time::Instant};

fn bounded_read(path: &Path, max: u64) -> e4_prototype::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err("recovery metadata exceeds its bound".into());
    }
    Ok(bytes)
}

fn verify_schema(destination: &Path) -> e4_prototype::Result<Value> {
    let report: Value =
        serde_json::from_slice(&bounded_read(&destination.join("report.json"), 64 << 10)?)?;
    let count = |name: &str| {
        report[name]
            .as_u64()
            .ok_or_else(|| format!("missing count: {name}"))
    };
    let limit = usize::try_from(count("max_value_bytes")?)?;
    if limit > 64 << 20 {
        return Err("CLI verification supports encoded values up to 64 MiB".into());
    }
    let rows =
        e4_prototype::recovery::visit_raw_records(&destination.join("records.raw"), limit, |r| {
            if r.overflow_marker {
                return Err("complete row archive contains an unresolved marker".into());
            }
            Ok(())
        })?;
    let unresolved = e4_prototype::recovery::visit_raw_records(
        &destination.join("unresolved.raw"),
        limit.max(4096),
        |_| Ok(()),
    )?;
    if rows != count("raw_records")? || unresolved != count("unresolved_values")? {
        return Err("schema archive counts differ from report".into());
    }
    let (mut layouts, mut conflicts) = (0u64, 0u64);
    for entry in fs::read_dir(destination.join("layouts"))? {
        let path = entry?.path();
        match path.extension().and_then(|s| s.to_str()) {
            Some("layout") | Some("candidate") => {
                let layout = e4_prototype::Layout::from_descriptor(&bounded_read(&path, 2081)?)?;
                let filename = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .ok_or("layout filename")?;
                let id: u64 = filename
                    .split('.')
                    .next()
                    .ok_or("layout filename")?
                    .parse()?;
                if id != layout.id {
                    return Err("schema file identity differs from descriptor".into());
                }
                if path.extension().is_some_and(|e| e == "layout") {
                    layouts += 1;
                }
            }
            Some("conflict") => conflicts += 1,
            _ => (),
        }
    }
    if layouts != count("layouts")? + count("conflicting_layouts")?
        || conflicts != count("conflicting_layouts")?
    {
        return Err("layout inventory differs from report".into());
    }
    Ok(
        json!({"verified":true,"membership":"candidate","raw_records":rows,"unresolved_values":unresolved,
        "scope":"raw archive framing, checksums, counts and schema descriptor integrity; decoded JSONL is a derived export; current membership is not established"}),
    )
}

fn inspect(source: &Path) -> e4_prototype::Result<Value> {
    let file = open_recovery_source(&source.join("data"))?;
    let bytes = file.len()?;
    let mut good = [0u64; 5];
    let mut failed = 0u64;
    let mut meta = Vec::new();
    let mut b = [0; PAGE_SIZE];
    for no in 0..bytes / PAGE_SIZE as u64 {
        let no = u32::try_from(no)?;
        file.read_at(&mut b, no as u64 * PAGE_SIZE as u64)?;
        match PageRef::open(&b, no) {
            Ok(p) => {
                good[p.kind() as usize] += 1;
                if no < 2 && p.kind() == PageKind::Meta {
                    if let Ok(m) = Meta::from_page(&p) {
                        meta.push(json!({"slot":no,"format":m.format_version,"generation":m.generation,"roots":m.roots}));
                    }
                }
            }
            Err(_) => failed += 1,
        }
    }
    Ok(
        json!({"version":1,"source":fs::canonicalize(source)?,"data_bytes":bytes,
        "verified_pages_by_kind":good,"failed_pages_unknown_kind":failed,
        "truncated_tail_bytes":bytes % PAGE_SIZE as u64,"meta":meta,
        "scope":"physical page inspection; valid pages alone do not prove current membership or valid values"}),
    )
}

fn run() -> e4_prototype::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [command, source] if command == "inspect" => inspect(Path::new(source))?,
        [command, source, destination] if command == "schema" => {
            let report = e4_prototype::recovery::recover_typed_candidates(
                Path::new(source), Path::new(destination), &e4_prototype::recovery::DenseV3,
                e4_prototype::recovery::RecoveryOptions::default(),
            )?;
            serde_json::from_reader(fs::File::open(report.destination.join("report.json"))?)?
        }
        [command, source, destination] if command == "salvage" => {
            let started = Instant::now();
            let r = recover_to(Path::new(source), Path::new(destination), Config {
                budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Full,
            })?;
            let value = json!({"version":r.version,"source":r.source,"destination":r.destination,
                "database":r.database,"class":format!("{:?}",r.class),"elapsed_seconds":started.elapsed().as_secs_f64(),
                "entries_recovered":r.entries_recovered,"entries_before_wal":r.entries_before_wal,
                "known_value_losses":r.known_value_losses,"unknown_extents":r.unknown_extents,
                "tree_and_archive_page_reads":r.pages_read,"raw_candidate_records":r.raw_candidate_records,
                "loss_journal":r.loss_journal,"candidate_archive":r.candidate_archive,
                "candidate_semantics":"raw source cells, possibly obsolete; overflow markers refer to source pages",
                "wal_quarantined":r.wal.wal_quarantined,"wal_bytes_kept":r.wal.wal_bytes_kept,
                "wal_bytes_set_aside":r.wal.wal_bytes_set_aside,
                "typed_layout_validation":"pending; this command verifies kernel records and values",
                "source_preserved":true,"published_over_source":false});
            let path = r.destination.join("report.json");
            let mut out = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
            serde_json::to_writer_pretty(&mut out, &value)?;
            out.write_all(b"\n")?;
            out.sync_all()?;
            kernel::io::sync_directory(&r.destination)?;
            value
        }
        [command, destination] if command == "verify" => {
            let dest = Path::new(destination);
            let marker = String::from_utf8(bounded_read(&dest.join("COMPLETE"),4096)?)?;
            if marker == "e4-schema-candidates-v1\n" {
                println!("{}",serde_json::to_string_pretty(&verify_schema(dest)?)?);
                return Ok(());
            }
            if !marker.starts_with("e4-recovery-v1\n") { return Err("unknown recovery marker".into()); }
            let expected: u64 = marker.lines().find_map(|l| l.strip_prefix("rows=")).ok_or("missing row count")?.parse()?;
            let data = dest.join("database/data");
            let file = open_recovery_source(&data)?;
            let mut chosen: Option<Meta> = None;
            let mut buf = [0; PAGE_SIZE];
            for no in 0..2 {
                file.read_at(&mut buf, no as u64 * PAGE_SIZE as u64)?;
                let p = PageRef::open(&buf, no)?;
                if let Ok(m) = Meta::from_page(&p) {
                    if chosen.as_ref().is_none_or(|old| m.generation > old.generation) { chosen = Some(m); }
                }
            }
            let m = chosen.ok_or("no verified output meta")?;
            let (rows, pages) = kernel::verify::verify_published_tree(&data, IoMode::Buffered, m.roots[0], 1)?;
            if rows != expected { return Err("reopened output count differs from completion marker".into()); }
            json!({"verified":true,"rows":rows,"tree_pages":pages,
                "scope":"output tree, values and recorded count; does not establish completeness or resolve candidate membership"})
        }
        _ => return Err("usage: recover inspect SOURCE | salvage SOURCE NEW_DESTINATION | schema SOURCE NEW_DESTINATION | verify RESULT_DIRECTORY".into()),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("recovery failed: {e}");
        std::process::exit(1);
    }
}
