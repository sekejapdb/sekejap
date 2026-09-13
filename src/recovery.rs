//! Extensible typed recovery above the kernel. Rootless output is candidate
//! evidence, never a published database or a claim of current membership.
use crate::{Layout, Result};
use kernel::recover::{CandidateReader, LeafCandidate, LeafEvent};
use serde_json::json;
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordClass {
    Layout,
    Entity,
    Other,
}

#[derive(Clone, Copy)]
pub struct CandidateContext<'a> {
    pub key: &'a [u8],
    pub page_no: u32,
    pub generation: u64,
}

/// Codec/keyspace policy belongs to E4, not the pager. Future encodings and
/// external vector resolvers can supply another implementation without changing
/// the scan, evidence format or recovery ownership protocol.
pub trait RecoveryCodec {
    fn name(&self) -> &'static str;
    fn classify(&self, key: &[u8]) -> RecordClass;
    fn layout_id(&self, row: &[u8]) -> Result<u64>;
    fn decode(&self, layout: &Layout, context: CandidateContext<'_>, row: &[u8]) -> Result<Value>;
}
pub struct DenseV3;
impl RecoveryCodec for DenseV3 {
    fn name(&self) -> &'static str {
        "dense-v3"
    }
    fn classify(&self, key: &[u8]) -> RecordClass {
        if key.len() == 10 && key[..2] == [0, 240] {
            RecordClass::Layout
        } else if key
            .first()
            .is_some_and(|k| (0x81..=0x88).contains(k) && key.len() == (k - 0x80) as usize + 1)
        {
            RecordClass::Entity
        } else {
            RecordClass::Other
        }
    }
    fn layout_id(&self, row: &[u8]) -> Result<u64> {
        let mut r = crate::Read { b: row, p: 0 };
        Ok(r.uv()? >> 2)
    }
    fn decode(&self, layout: &Layout, _context: CandidateContext<'_>, row: &[u8]) -> Result<Value> {
        crate::decode_dense_v3(layout, row, |_| {
            Err("external vector resolver not configured".into())
        })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct RecoveryOptions {
    pub max_value_bytes: usize,
    pub layout_cache_entries: usize,
}
impl Default for RecoveryOptions {
    fn default() -> Self {
        Self {
            max_value_bytes: 1 << 20,
            layout_cache_entries: 16,
        }
    }
}
#[derive(Debug)]
pub struct SchemaRecoveryReport {
    pub destination: PathBuf,
    pub layouts: u64,
    pub conflicting_layouts: u64,
    pub invalid_descriptors: u64,
    pub raw_records: u64,
    pub decoded_records: u64,
    pub missing_layout_records: u64,
    pub unresolved_records: u64,
    pub damaged_pages: u64,
}

pub fn recover_typed_candidates(
    source: &Path,
    destination: &Path,
    codec: &impl RecoveryCodec,
    options: RecoveryOptions,
) -> Result<SchemaRecoveryReport> {
    if options.max_value_bytes == 0 || options.layout_cache_entries > 256 {
        return Err("nonzero value limit and at most 256 cached layouts required".into());
    }
    let started = Instant::now();
    let source = fs::canonicalize(source)?;
    let parent = fs::canonicalize(destination.parent().ok_or("destination parent required")?)?;
    let destination = parent.join(destination.file_name().ok_or("destination name required")?);
    if source.starts_with(&destination) || destination.starts_with(&source) {
        return Err("source and destination must not overlap".into());
    }
    let reader = CandidateReader::open(&source)?;
    fs::create_dir(&destination)?;
    kernel::io::sync_directory(&parent)?;
    let mut catalog = Catalog::new(destination.join("layouts"), options.layout_cache_entries)?;
    let mut report = SchemaRecoveryReport {
        destination: destination.clone(),
        layouts: 0,
        conflicting_layouts: 0,
        invalid_descriptors: 0,
        raw_records: 0,
        decoded_records: 0,
        missing_layout_records: 0,
        unresolved_records: 0,
        damaged_pages: 0,
    };
    let mut issues = output(&destination.join("issues.jsonl"))?;
    // Discover the entire schema evidence set before decoding any row. Conflicting
    // immutable IDs must not acquire a first-copy-wins interpretation.
    reader.scan::<Box<dyn std::error::Error>>(1, |event| {
        if let LeafEvent::Record(r) = event {
            if codec.classify(r.key) == RecordClass::Layout {
                match reader.read_value(r, 2081) {
                    Ok(bytes) => match Layout::from_descriptor(&bytes) {
                        Ok(layout) => catalog.insert(&layout, &bytes, r, &mut report)?,
                        Err(error) => {
                            report.invalid_descriptors += 1;
                            line(&mut issues, &json!({"kind":"invalid_descriptor","page":r.page_no,"key_hex":hex(r.key),"reason":error.to_string()}))?;
                        }
                    },
                    Err(kernel::Error::Io(e)) => return Err(e.into()),
                    Err(error) => {
                        report.invalid_descriptors += 1;
                        line(&mut issues, &json!({"kind":"invalid_descriptor","page":r.page_no,"key_hex":hex(r.key),"reason":error.to_string()}))?;
                    }
                }
            }
        }
        Ok(())
    })?;
    let mut raw = output(&destination.join("records.raw"))?;
    let mut unresolved = output(&destination.join("unresolved.raw"))?;
    raw.write_all(b"E4ROWS01")?;
    unresolved.write_all(b"E4ROWS01")?;
    let mut decoded = output(&destination.join("decoded.jsonl"))?;
    let mut unresolved_values = 0;
    let scan = reader.scan::<Box<dyn std::error::Error>>(1, |event| {
        let r = match event {
            LeafEvent::Record(r) if codec.classify(r.key) == RecordClass::Entity => r,
            LeafEvent::DamagedPage { page_no } => {
                line(&mut issues, &json!({"kind":"unknown_page_extent","page":page_no}))?;
                return Ok(());
            }
            LeafEvent::MalformedCell { page_no, slot } => {
                line(&mut issues, &json!({"kind":"malformed_cell","page":page_no,"slot":slot}))?;
                return Ok(());
            }
            _ => return Ok(()),
        };
        let value = match reader.read_value(r, options.max_value_bytes) {
            Ok(v) => v,
            Err(kernel::Error::Io(e)) => return Err(e.into()),
            Err(error) => {
                report.unresolved_records += 1; unresolved_values += 1;
                frame(&mut unresolved, r, r.stored_value, r.overflow)?;
                line(&mut issues, &json!({"kind":"unresolved_value","page":r.page_no,"key_hex":hex(r.key),"reason":error.to_string(),"source_marker_preserved":r.overflow}))?;
                return Ok(());
            }
        };
        // Preserve exact complete encoded bytes even if schema or decoding fails.
        frame(&mut raw, r, &value, false)?; report.raw_records += 1;
        let id = match codec.layout_id(&value) {
            Ok(id) => id,
            Err(error) => {
                report.unresolved_records += 1;
                line(&mut issues, &json!({"kind":"invalid_row_header","page":r.page_no,"key_hex":hex(r.key),"reason":error.to_string()}))?;
                return Ok(());
            }
        };
        match catalog.resolve(id)? {
            Resolution::Missing => {
                report.missing_layout_records += 1;
                line(&mut issues, &json!({"kind":"missing_layout","layout_id":id,"page":r.page_no,"key_hex":hex(r.key),"raw_preserved":true}))?;
            }
            Resolution::Conflict => {
                report.unresolved_records += 1;
                line(&mut issues, &json!({"kind":"conflicting_layout","layout_id":id,"page":r.page_no,"key_hex":hex(r.key),"raw_preserved":true}))?;
            }
            Resolution::Found(layout) => match codec.decode(&layout, CandidateContext {
                key: r.key, page_no: r.page_no, generation: r.generation,
            }, &value) {
                Ok(document) => {
                    line(&mut decoded, &json!({"membership":"candidate","page":r.page_no,"generation":r.generation,
                        "key_hex":hex(r.key),"layout_id":id,"document":document}))?;
                    report.decoded_records += 1;
                }
                Err(error) => {
                    report.unresolved_records += 1;
                    line(&mut issues, &json!({"kind":"decode_failed","layout_id":id,"page":r.page_no,"key_hex":hex(r.key),"reason":error.to_string(),"raw_preserved":true}))?;
                }
            }
        }
        Ok(())
    })?;
    report.damaged_pages = scan.damaged_pages;
    if scan.truncated_tail_bytes != 0 {
        line(
            &mut issues,
            &json!({"kind":"truncated_tail","bytes":scan.truncated_tail_bytes}),
        )?;
    }
    for out in [&mut raw, &mut unresolved, &mut decoded, &mut issues] {
        out.flush()?;
        out.get_ref().sync_all()?;
    }
    drop((raw, unresolved, decoded, issues));
    // Independent reread checks framing, bounds, CRC and counts before completion.
    if visit_raw_records(
        &destination.join("records.raw"),
        options.max_value_bytes,
        |_| Ok(()),
    )? != report.raw_records
        || visit_raw_records(
            &destination.join("unresolved.raw"),
            options.max_value_bytes.max(4096),
            |_| Ok(()),
        )? != unresolved_values
    {
        return Err("raw export count mismatch".into());
    }
    let summary = json!({"version":1,"codec":codec.name(),"source":source,"destination":destination,
        "membership":"candidate","source_preserved":true,"normal_storage_bytes_added":0,
        "layouts":report.layouts,"conflicting_layouts":report.conflicting_layouts,"invalid_descriptors":report.invalid_descriptors,
        "raw_records":report.raw_records,"decoded_records":report.decoded_records,
        "missing_layout_records":report.missing_layout_records,"unresolved_records":report.unresolved_records,"unresolved_values":unresolved_values,
        "damaged_pages":report.damaged_pages,"malformed_cells":scan.malformed_cells,"truncated_tail_bytes":scan.truncated_tail_bytes,
        "max_value_bytes":options.max_value_bytes,"layout_cache_entries":options.layout_cache_entries,
        "elapsed_seconds":started.elapsed().as_secs_f64(),"schema_evidence":"verified leaf cells and descriptor CRC; immutable IDs; conflicts quarantined",
        "scope":"candidate export only; WAL and current membership not reconstructed; JSONL is an export, not database storage"});
    let mut out = output(&destination.join("report.json"))?;
    line(&mut out, &summary)?;
    out.flush()?;
    out.get_ref().sync_all()?;
    kernel::io::sync_directory(&catalog.directory)?;
    kernel::io::sync_directory(&destination)?;
    let mut complete = output(&destination.join("COMPLETE.pending"))?;
    complete.write_all(b"e4-schema-candidates-v1\n")?;
    complete.flush()?;
    complete.get_ref().sync_all()?;
    drop(complete);
    fs::rename(
        destination.join("COMPLETE.pending"),
        destination.join("COMPLETE"),
    )?;
    kernel::io::sync_directory(&destination)?;
    Ok(report)
}

fn output(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(
        64 << 10,
        OpenOptions::new().write(true).create_new(true).open(path)?,
    ))
}
fn line(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[derive(Clone)]
enum Resolution {
    Missing,
    Conflict,
    Found(Layout),
}
struct Catalog {
    directory: PathBuf,
    cache: VecDeque<(u64, Resolution)>,
    limit: usize,
}
impl Catalog {
    fn new(directory: PathBuf, limit: usize) -> Result<Self> {
        fs::create_dir(&directory)?;
        Ok(Self {
            directory,
            cache: VecDeque::new(),
            limit,
        })
    }
    fn path(&self, id: u64, extension: &str) -> PathBuf {
        self.directory.join(format!("{id:020}.{extension}"))
    }
    fn insert(
        &self,
        layout: &Layout,
        bytes: &[u8],
        r: LeafCandidate<'_>,
        report: &mut SchemaRecoveryReport,
    ) -> Result<()> {
        let primary = self.path(layout.id, "layout");
        match fs::read(&primary) {
            Ok(old) if old != bytes => {
                let conflict = self.path(layout.id, "conflict");
                if !conflict.try_exists()? {
                    let mut f = output(&conflict)?;
                    f.write_all(b"immutable layout ID has conflicting descriptors\n")?;
                    f.flush()?;
                    f.get_ref().sync_all()?;
                    report.layouts -= 1;
                    report.conflicting_layouts += 1;
                }
                let path = self.path(
                    layout.id,
                    &format!("page{}-slot{}.candidate", r.page_no, r.slot),
                );
                let mut f = output(&path)?;
                f.write_all(bytes)?;
                f.flush()?;
                f.get_ref().sync_all()?;
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut f = output(&primary)?;
                f.write_all(bytes)?;
                f.flush()?;
                f.get_ref().sync_all()?;
                report.layouts += 1;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    fn resolve(&mut self, id: u64) -> Result<Resolution> {
        if let Some((_, found)) = self.cache.iter().find(|(key, _)| *key == id) {
            return Ok(found.clone());
        }
        let found = if self.path(id, "conflict").try_exists()? {
            Resolution::Conflict
        } else {
            match fs::read(self.path(id, "layout")) {
                Ok(bytes) => {
                    let layout = Layout::from_descriptor(&bytes)?;
                    if layout.id != id {
                        return Err("persisted layout identity mismatch".into());
                    }
                    Resolution::Found(layout)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Resolution::Missing,
                Err(e) => return Err(e.into()),
            }
        };
        if self.limit != 0 {
            if self.cache.len() == self.limit {
                self.cache.pop_front();
            }
            self.cache.push_back((id, found.clone()));
        }
        Ok(found)
    }
}

fn frame(out: &mut impl Write, r: LeafCandidate<'_>, value: &[u8], marker: bool) -> Result<()> {
    let mut header = Vec::with_capacity(21);
    header.extend_from_slice(&r.page_no.to_le_bytes());
    header.extend_from_slice(&r.generation.to_le_bytes());
    header.extend_from_slice(&u32::try_from(r.key.len())?.to_le_bytes());
    header.extend_from_slice(&u32::try_from(value.len())?.to_le_bytes());
    header.push(u8::from(marker));
    let crc = crc32c::crc32c_append(crc32c::crc32c_append(crc32c::crc32c(&header), r.key), value);
    out.write_all(&header)?;
    out.write_all(r.key)?;
    out.write_all(value)?;
    out.write_all(&crc.to_le_bytes())?;
    Ok(())
}
pub struct RawCandidate {
    pub page_no: u32,
    pub generation: u64,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub overflow_marker: bool,
}
/// Reusable, bounded reader for exported evidence. Integrity is checked before
/// invoking the callback. Callers may resolve schemas later without the tree.
pub fn visit_raw_records(
    path: &Path,
    max_value_bytes: usize,
    mut visit: impl FnMut(RawCandidate) -> Result<()>,
) -> Result<u64> {
    let mut input = BufReader::new(File::open(path)?);
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    if &magic != b"E4ROWS01" {
        return Err("unsupported row archive".into());
    }
    let mut count = 0;
    loop {
        let mut header = [0; 21];
        if input.read(&mut header[..1])? == 0 {
            break;
        }
        input.read_exact(&mut header[1..])?;
        let key_len = u32::from_le_bytes(header[12..16].try_into()?) as usize;
        let value_len = u32::from_le_bytes(header[16..20].try_into()?) as usize;
        if key_len > 4096 || value_len > max_value_bytes || header[20] > 1 {
            return Err("row archive bounds".into());
        }
        let mut key = vec![0; key_len];
        let mut value = vec![0; value_len];
        let mut sum = [0; 4];
        input.read_exact(&mut key)?;
        input.read_exact(&mut value)?;
        input.read_exact(&mut sum)?;
        let crc =
            crc32c::crc32c_append(crc32c::crc32c_append(crc32c::crc32c(&header), &key), &value);
        if crc != u32::from_le_bytes(sum) {
            return Err("row archive checksum".into());
        }
        visit(RawCandidate {
            page_no: u32::from_le_bytes(header[..4].try_into()?),
            generation: u64::from_le_bytes(header[4..12].try_into()?),
            key,
            value,
            overflow_marker: header[20] != 0,
        })?;
        count += 1;
    }
    Ok(count)
}
