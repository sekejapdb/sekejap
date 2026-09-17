//! Source-preserving verification of current typed rows and derived indexes.
use super::*;
use crate::pagewal::{CurrentReaderLimits, CurrentSourceReader};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueClass {
    Catalog,
    Primary,
    Derived,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueKind {
    Missing,
    Extra,
    Mismatch,
    Malformed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationIssue {
    pub class: IssueClass,
    pub kind: IssueKind,
    pub key: Vec<u8>,
    pub index: Option<IndexId>,
    pub entity: Option<EntityId>,
    pub message: String,
}

#[derive(Clone, Copy, Debug)]
pub struct VerificationLimits {
    pub source: CurrentReaderLimits,
    pub max_rows: u64,
    pub max_point_reads: u64,
    pub max_metadata: usize,
    pub max_issues: u64,
    pub preview: usize,
}
impl Default for VerificationLimits {
    fn default() -> Self {
        Self {
            source: CurrentReaderLimits::default(),
            max_rows: 10_000_000,
            max_point_reads: 50_000_000,
            max_metadata: 4096,
            max_issues: 1_000_000,
            preview: 64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct VerificationReport {
    pub complete: bool,
    pub clean: bool,
    pub catalog_issues: u64,
    pub primary_issues: u64,
    pub derived_issues: u64,
    pub primary_rows: u64,
    pub derived_rows: u64,
    pub point_reads: u64,
    pub preview: Vec<VerificationIssue>,
    pub limitations: Vec<&'static str>,
}
impl Default for VerificationReport {
    fn default() -> Self {
        Self {
            complete: false,
            clean: false,
            catalog_issues: 0,
            primary_issues: 0,
            derived_issues: 0,
            primary_rows: 0,
            derived_rows: 0,
            point_reads: 0,
            preview: Vec::new(),
            limitations: Vec::new(),
        }
    }
}

struct Run<F> {
    reader: Arc<CurrentSourceReader>,
    limits: VerificationLimits,
    report: VerificationReport,
    emit: F,
}
impl<F: FnMut(&VerificationIssue)> Run<F> {
    fn issue(&mut self, issue: VerificationIssue) -> Result<()> {
        let total =
            self.report.catalog_issues + self.report.primary_issues + self.report.derived_issues;
        if total >= self.limits.max_issues {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index verifier issue budget exceeded",
            )));
        }
        match issue.class {
            IssueClass::Catalog => self.report.catalog_issues += 1,
            IssueClass::Primary => self.report.primary_issues += 1,
            IssueClass::Derived => self.report.derived_issues += 1,
        }
        if self.report.preview.len() < self.limits.preview {
            self.report.preview.push(issue.clone());
        }
        (self.emit)(&issue);
        Ok(())
    }
    fn read(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.report.point_reads = self
            .report
            .point_reads
            .checked_add(1)
            .ok_or_else(|| corrupt("verifier read count"))?;
        if self.report.point_reads > self.limits.max_point_reads {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index verifier point-read budget exceeded",
            )));
        }
        self.reader.get(key).map_err(Error::from)
    }
    /// The same budgeted point read, in the tree that actually holds this
    /// index's entries. A version-1 index is the primary tree, unchanged.
    fn read_index(&mut self, i: &IndexInfo, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(t) = i.tree else { return self.read(key) };
        self.report.point_reads = self
            .report
            .point_reads
            .checked_add(1)
            .ok_or_else(|| corrupt("verifier read count"))?;
        if self.report.point_reads > self.limits.max_point_reads {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index verifier point-read budget exceeded",
            )));
        }
        self.reader.get_in(t.id, t.root, key).map_err(Error::from)
    }
    fn row(&mut self, derived: bool) -> Result<()> {
        let n = if derived {
            &mut self.report.derived_rows
        } else {
            &mut self.report.primary_rows
        };
        *n = n
            .checked_add(1)
            .ok_or_else(|| corrupt("verifier row count"))?;
        if *n > self.limits.max_rows {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index verifier row budget exceeded",
            )));
        }
        Ok(())
    }
    fn mismatch(
        &mut self,
        class: IssueClass,
        key: &[u8],
        index: Option<IndexId>,
        entity: Option<EntityId>,
        actual: Option<&[u8]>,
        expected: Option<&[u8]>,
        message: &str,
    ) -> Result<()> {
        if actual == expected {
            return Ok(());
        }
        let kind = match (actual, expected) {
            (None, Some(_)) => IssueKind::Missing,
            (Some(_), None) => IssueKind::Extra,
            _ => IssueKind::Mismatch,
        };
        self.issue(VerificationIssue {
            class,
            kind,
            key: key.to_vec(),
            index,
            entity,
            message: message.into(),
        })
    }
}

fn tag_end(tag: u8) -> Vec<u8> {
    vec![tag.saturating_add(1)]
}
fn visit(
    reader: &CurrentSourceReader,
    start: &[u8],
    end: Option<&[u8]>,
    mut f: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<u64> {
    let mut detail = None;
    let raw = reader.visit_range(start, end, |k, v| match f(k, v) {
        Ok(()) => Ok(()),
        Err(e) => {
            detail = Some(e);
            Err(kernel::Error::Corrupt {
                page_no: 0,
                why: "typed verifier callback refused",
            })
        }
    });
    if let Some(e) = detail {
        return Err(e);
    }
    raw.map_err(Error::from)
}
/// Walk one index's entries wherever they live. Every page of a per-index
/// tree must carry that index's tree id, so a descriptor root pointing at a
/// page of another tree -- or at a free page -- is refused here, which is the
/// check that makes a wrong root visible instead of silently empty.
fn visit_index(
    reader: &CurrentSourceReader,
    i: &IndexInfo,
    start: &[u8],
    end: Option<&[u8]>,
    mut f: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<u64> {
    let Some(t) = i.tree else { return visit(reader, start, end, f) };
    let mut detail = None;
    let raw = reader.visit_tree_range(t.id, t.root, start, end, |k, v| match f(k, v) {
        Ok(()) => Ok(()),
        Err(e) => {
            detail = Some(e);
            Err(kernel::Error::Corrupt {
                page_no: 0,
                why: "typed verifier callback refused",
            })
        }
    });
    if let Some(e) = detail {
        return Err(e);
    }
    raw.map_err(Error::from)
}
fn visit_all(
    reader: &CurrentSourceReader,
    f: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<u64> {
    visit(reader, &[], None, f)
}
fn decode_layout(bytes: &[u8]) -> Result<Layout> {
    match Layout::from_descriptor(bytes) {
        Ok(layout) => Ok(layout),
        Err(error) => {
            let message = error.to_string();
            if (bytes.len() == 2081
                && bytes.starts_with(b"E4")
                && !bytes.starts_with(b"E4P0LAY\0")
                && crc32c::crc32c(&bytes[..2077])
                    == u32::from_le_bytes(bytes[2077..].try_into().unwrap()))
                || message.contains("unknown layout type")
            {
                Err(Error::Unsupported(format!(
                    "layout descriptor version/type: {message}"
                )))
            } else {
                Err(corrupt(message))
            }
        }
    }
}
fn layout<R: FnMut(&[u8]) -> Result<Option<Vec<u8>>>>(get: R, id: u32) -> Result<Layout> {
    replicas(get, |copy| layout_key(id, copy), decode_layout)
}
fn sequence(bytes: &[u8]) -> Result<u64> {
    let body = unpack(bytes, COUNTER_MAGIC)?;
    if body.len() != 8 {
        return Err(corrupt("sequence counter length"));
    }
    let next = u64::from_be_bytes(body.try_into().unwrap());
    if next == 0 {
        return Err(corrupt("zero sequence counter"));
    }
    Ok(next)
}
fn field(layout: &Layout, row: &[u8], name: &str) -> Result<crate::dense_v3::FieldValue> {
    crate::dense_v3::read_field(layout, row, name).map_err(corrupt)
}
fn malformed<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    class: IssueClass,
    key: &[u8],
    index: Option<IndexId>,
    entity: Option<EntityId>,
    message: &str,
) -> Result<()> {
    run.issue(VerificationIssue {
        class,
        kind: IssueKind::Malformed,
        key: key.to_vec(),
        index,
        entity,
        message: message.into(),
    })
}
fn verify_collections<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    header: HeaderInfo,
) -> Result<Vec<(Catalog, u64)>> {
    let mut catalogs: Vec<(Catalog, u64)> = Vec::new();
    let source = run.reader.clone();
    visit(&source, &[1], Some(&[2]), |key, value| {
        run.row(true)?;
        if key.len() < 3 || key[1] > 2 {
            return Err(corrupt("collection descriptor replica key"));
        }
        let mut at = 2;
        let raw = read_ordered(key, &mut at)?;
        let id = CollectionId(u32::try_from(raw).map_err(corrupt)?);
        if at != key.len() || id.0 == 0 || id.0 >= header.next_collection {
            return Err(corrupt("collection descriptor identity"));
        }
        match parse_catalog(value) {
            Ok(decoded) if decoded.id == id => {}
            Ok(_) => return Err(corrupt("collection descriptor key/value identity")),
            Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
            Err(_) => run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Malformed,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "collection descriptor replica damaged".into(),
            })?,
        }
        if catalogs.iter().any(|entry| entry.0.id == id) {
            return Ok(());
        }
        if catalogs.len() == run.limits.max_metadata {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index verifier metadata budget exceeded",
            )));
        }
        let canonical = replicas(
            |k| run.read(k),
            |copy| replica_key(1, id.0, copy),
            parse_catalog,
        )?;
        for copy in 0..3u8 {
            let replica = replica_key(1, id.0, copy);
            match run.read(&replica)? {
                None => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Missing,
                    key: replica,
                    index: None,
                    entity: None,
                    message: "collection descriptor replica missing".into(),
                })?,
                Some(bytes) if parse_catalog(&bytes).ok().as_ref() == Some(&canonical) => {}
                Some(bytes) => match parse_catalog(&bytes) {
                    Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                    _ => run.issue(VerificationIssue {
                        class: IssueClass::Catalog,
                        kind: IssueKind::Malformed,
                        key: replica,
                        index: None,
                        entity: None,
                        message: "collection descriptor replica damaged/conflicting".into(),
                    })?,
                },
            }
        }
        let name = name_key(&canonical.name);
        let actual = run.read(&name)?;
        run.mismatch(
            IssueClass::Catalog,
            &name,
            None,
            None,
            actual.as_deref(),
            Some(&canonical.id.0.to_be_bytes()),
            "collection-name mapping",
        )?;
        let current = layout(|k| run.read(k), canonical.layout)?;
        if current.id != u64::from(canonical.layout) || canonical.layout >= header.next_layout {
            return Err(corrupt("collection current layout identity"));
        }
        let counter = replicas(|k| run.read(k), |copy| replica_key(2, id.0, copy), sequence)?;
        for copy in 0..3u8 {
            let key = replica_key(2, id.0, copy);
            match run.read(&key)? {
                None => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Missing,
                    key,
                    index: None,
                    entity: None,
                    message: "sequence counter replica missing".into(),
                })?,
                Some(bytes) if sequence(&bytes).ok() == Some(counter) => {}
                Some(_) => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Malformed,
                    key,
                    index: None,
                    entity: None,
                    message: "sequence counter replica damaged/conflicting".into(),
                })?,
            }
        }
        catalogs.push((canonical, counter));
        Ok(())
    })?;
    let source = run.reader.clone();
    visit(&source, &[0x10], Some(&[0x11]), |key, value| {
        run.row(true)?;
        let name = std::str::from_utf8(&key[1..]).map_err(corrupt)?;
        if value.len() != 4 {
            return Err(corrupt("collection-name mapping value"));
        }
        let id = CollectionId(u32::from_be_bytes(value.try_into().unwrap()));
        if !catalogs.iter().any(|(c, _)| c.id == id && c.name == name) {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "orphan collection-name mapping".into(),
            })?;
        }
        Ok(())
    })?;
    let source = run.reader.clone();
    visit(&source, &[2], Some(&[3]), |key, value| {
        run.row(true)?;
        if key.len() < 3 || key[1] > 2 {
            return Err(corrupt("sequence counter replica key"));
        }
        let mut at = 2;
        let id = CollectionId(u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?);
        if at != key.len() || id.0 == 0 {
            return Err(corrupt("sequence counter identity"));
        }
        if sequence(value).is_err() {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Malformed,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "sequence counter replica damaged".into(),
            })?;
            return Ok(());
        }
        if id.0 >= header.next_collection || !catalogs.iter().any(|(catalog, _)| catalog.id == id) {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "orphan sequence counter replica".into(),
            })?;
        }
        Ok(())
    })?;
    let expected_collections = usize::try_from(header.next_collection - 1)
        .map_err(|_| corrupt("collection allocator domain"))?;
    if expected_collections > run.limits.max_metadata {
        return Err(Error::Kernel(kernel::Error::ResourceLimit(
            "index verifier collection metadata budget exceeded",
        )));
    }
    if catalogs.len() != expected_collections {
        run.issue(VerificationIssue {
            class: IssueClass::Catalog,
            kind: IssueKind::Missing,
            key: vec![1],
            index: None,
            entity: None,
            message: "collection allocator has missing descriptor identities".into(),
        })?;
    }
    verify_layouts(run, header)?;
    Ok(catalogs)
}

fn verify_layouts<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    header: HeaderInfo,
) -> Result<()> {
    let expected =
        usize::try_from(header.next_layout - 1).map_err(|_| corrupt("layout allocator domain"))?;
    if expected > run.limits.max_metadata {
        return Err(Error::Kernel(kernel::Error::ResourceLimit(
            "index verifier layout metadata budget exceeded",
        )));
    }
    let source = run.reader.clone();
    visit(&source, &[0, 240], Some(&[0, 241]), |key, value| {
        run.row(true)?;
        if key.len() != 10 {
            return Err(corrupt("layout descriptor replica key"));
        }
        let slot = u64::from_be_bytes(key[2..].try_into().unwrap());
        let id = slot / 3;
        if id == 0 {
            return Err(corrupt("layout descriptor slot identity"));
        }
        let decoded = match decode_layout(value) {
            Ok(decoded) => decoded,
            Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
            Err(_) => {
                run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Malformed,
                    key: key.to_vec(),
                    index: None,
                    entity: None,
                    message: "layout descriptor replica damaged".into(),
                })?;
                return Ok(());
            }
        };
        if decoded.id != id {
            return Err(corrupt("layout descriptor key/value identity"));
        }
        if id >= u64::from(header.next_layout) {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "layout descriptor identity exceeds preserved allocator".into(),
            })?;
        }
        Ok(())
    })?;
    for id in 1..header.next_layout {
        let mut canonical = None;
        for copy in 0..3u8 {
            let key = layout_key(id, copy);
            let Some(bytes) = run.read(&key)? else {
                run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Missing,
                    key,
                    index: None,
                    entity: None,
                    message: "layout descriptor replica missing".into(),
                })?;
                continue;
            };
            match decode_layout(&bytes) {
                Ok(candidate) if candidate.id == u64::from(id) => {
                    if let Some(old) = &canonical {
                        if old != &candidate {
                            return Err(corrupt("conflicting layout descriptor replicas"));
                        }
                    } else {
                        canonical = Some(candidate);
                    }
                }
                Ok(_) => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Mismatch,
                    key,
                    index: None,
                    entity: None,
                    message: "layout descriptor key/value identity mismatch".into(),
                })?,
                Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                Err(_) => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Malformed,
                    key,
                    index: None,
                    entity: None,
                    message: "layout descriptor replica damaged".into(),
                })?,
            }
        }
        if canonical.is_none() {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Missing,
                key: layout_key(id, 0),
                index: None,
                entity: None,
                message: "layout allocator identity has no usable descriptor replica".into(),
            })?;
        }
    }
    Ok(())
}
/// Verify the newest committed source without opening a writer or ordinary
/// snapshot. Issues stream to `emit`; only the bounded preview is retained.
pub fn verify_indexed_source(
    path: impl AsRef<Path>,
    limits: VerificationLimits,
    emit: impl FnMut(&VerificationIssue),
) -> Result<VerificationReport> {
    if limits.max_rows == 0 || limits.max_point_reads == 0 || limits.max_metadata == 0 {
        return Err(invalid("index verifier limits must be nonzero"));
    }
    let reader =
        Arc::new(CurrentSourceReader::open(path.as_ref(), limits.source).map_err(Error::from)?);
    let mut run = Run {
        reader,
        limits,
        report: VerificationReport::default(),
        emit,
    };
    let header = replicas(|k| run.read(k), |copy| vec![0, 0, copy], parse_header)?;
    for copy in 0..3u8 {
        let key = vec![0, 0, copy];
        match run.read(&key)? {
            None => run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Missing,
                key,
                index: None,
                entity: None,
                message: "typed header replica missing".into(),
            })?,
            Some(bytes) => match parse_header(&bytes) {
                Ok(candidate) if candidate == header => {}
                Ok(_) => return Err(corrupt("conflicting typed header replicas")),
                Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                Err(_) => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Malformed,
                    key,
                    index: None,
                    entity: None,
                    message: "typed header replica damaged".into(),
                })?,
            },
        }
    }
    let ih = header.indexes;
    let catalogs = verify_collections(&mut run, header)?;
    let mut indexes = Vec::new();
    let source = run.reader.clone();
    visit(
        &source,
        &[indexes::REGISTRY],
        Some(&tag_end(indexes::REGISTRY)),
        |key, value| {
            run.row(true)?;
            let mut at = 1;
            let id = IndexId(read_ordered(key, &mut at)?);
            if at != key.len()
                || id.0 == 0
                || value.len() != 4
                || ih.is_some_and(|h| id.0 >= h.next)
            {
                return Err(corrupt("verifier index registry"));
            }
            let info = indexes::read_index(|k| run.read(k), id)?;
            if value != info.collection.0.to_be_bytes() {
                return Err(corrupt("verifier registry collection"));
            }
            if indexes.len() == run.limits.max_metadata {
                return Err(Error::Kernel(kernel::Error::ResourceLimit(
                    "index verifier metadata budget exceeded",
                )));
            }
            indexes.push(info);
            Ok(())
        },
    )?;
    if indexes.len() != ih.map_or(0, |h| h.count as usize) {
        run.issue(VerificationIssue {
            class: IssueClass::Catalog,
            kind: IssueKind::Mismatch,
            key: vec![indexes::REGISTRY],
            index: None,
            entity: None,
            message: "index header count disagrees with registry".into(),
        })?;
    }
    if catalogs.is_empty() && header.next_collection != 1 {
        run.issue(VerificationIssue {
            class: IssueClass::Catalog,
            kind: IssueKind::Mismatch,
            key: vec![1],
            index: None,
            entity: None,
            message: "collection allocator has no corresponding descriptors".into(),
        })?;
    }
    let source = run.reader.clone();
    visit(
        &source,
        &[indexes::DESCRIPTOR],
        Some(&tag_end(indexes::DESCRIPTOR)),
        |key, value| {
            run.row(true)?;
            if key.len() < 3 || key[1] > 2 {
                return Err(corrupt("index descriptor replica key"));
            }
            let mut at = 2;
            let id = IndexId(read_ordered(key, &mut at)?);
            if at != key.len() || id.0 == 0 {
                return Err(corrupt("index descriptor replica identity"));
            }
            if ih.is_some_and(|header| id.0 >= header.next) {
                return Err(corrupt("index descriptor identity exceeds allocator"));
            }
            let decoded = match indexes::decode(value) {
                Ok(decoded) => decoded,
                Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                Err(_) => {
                    run.issue(VerificationIssue {
                        class: IssueClass::Catalog,
                        kind: IssueKind::Malformed,
                        key: key.to_vec(),
                        index: Some(id),
                        entity: None,
                        message: "index descriptor replica damaged".into(),
                    })?;
                    return Ok(());
                }
            };
            if decoded.id != id {
                return Err(corrupt("index descriptor key/value identity"));
            }
            let registry = indexes::ikey(indexes::REGISTRY, id);
            if run.read(&registry)?.is_none() {
                run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Extra,
                    key: key.to_vec(),
                    index: Some(id),
                    entity: None,
                    message: "orphan index descriptor replica".into(),
                })?;
            }
            if let Some(header) = ih {
                let required = match decoded.family {
                    IndexFamily::Scalar => 1,
                    IndexFamily::ExactVector => vector_indexes::VECTOR_FEATURE,
                    IndexFamily::QuantizedVector => {
                        quantized_vector_indexes::QUANTIZED_VECTOR_FEATURE
                    }
                    IndexFamily::SpatialPoint => spatial_indexes::SPATIAL_FEATURE,
                    IndexFamily::Text => text_indexes::TEXT_FEATURE,
                };
                if header.features & required == 0 {
                    return Err(corrupt("index descriptor lacks required feature bit"));
                }
            }
            Ok(())
        },
    )?;
    for info in &indexes {
        for copy in 0..3u8 {
            let key = indexes::dkey(info.id, copy);
            match run.read(&key)? {
                None => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Missing,
                    key,
                    index: Some(info.id),
                    entity: None,
                    message: "index descriptor replica missing".into(),
                })?,
                Some(bytes) => match indexes::decode(&bytes) {
                    Ok(candidate) if candidate == *info => {}
                    Ok(_) => return Err(corrupt("conflicting index descriptor replicas")),
                    Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                    Err(_) => run.issue(VerificationIssue {
                        class: IssueClass::Catalog,
                        kind: IssueKind::Malformed,
                        key,
                        index: Some(info.id),
                        entity: None,
                        message: "index descriptor replica damaged".into(),
                    })?,
                },
            }
        }
        let mapped = run.read(&indexes::ckey(info.collection, info.id))?;
        run.mismatch(
            IssueClass::Catalog,
            &indexes::ckey(info.collection, info.id),
            Some(info.id),
            None,
            mapped.as_deref(),
            Some(&[]),
            "collection-index mapping",
        )?;
        let want = ordered(info.id.0);
        let named = run.read(&indexes::nkey(info.collection, &info.name))?;
        run.mismatch(
            IssueClass::Catalog,
            &indexes::nkey(info.collection, &info.name),
            Some(info.id),
            None,
            named.as_deref(),
            Some(&want),
            "index-name mapping",
        )?;
        let c = replicas(
            |k| run.read(k),
            |copy| replica_key(1, info.collection.0, copy),
            parse_catalog,
        )?;
        let l = layout(|k| run.read(k), c.layout)?;
        if !l
            .fields
            .iter()
            .any(|(n, k)| n == &info.field && k == &info.kind)
        {
            return Err(corrupt("indexed field/layout mismatch"));
        }
    }
    verify_index_mappings(&mut run, &indexes)?;
    for info in &indexes {
        if info.state != IndexState::Ready {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Mismatch,
                key: indexes::dkey(info.id, 0),
                index: Some(info.id),
                entity: None,
                message:
                    "BUILDING/DROPPING index is not eligible for complete derived verification"
                        .into(),
            })?;
        }
    }
    // Intact unknown key tags cannot be silently copied by a typed verifier.
    let source = run.reader.clone();
    visit_all(&source, |key, _| {
        run.row(true)?;
        let known = matches!(
            key.first(),
            Some(
                0 | 1
                    | 2
                    | 3
                    | 4
                    | 5
                    | 6
                    | 7
                    | 0x10
                    | 0x11
                    | 0x12
                    | 0x20
                    | 0x40
                    | 0x60
                    | 0x70
                    | 0x71
                    | 0x72
                    | 0x73
                    | 0x74
                    | 0x75
                    | 0x76
                    | 0x77
                    | 0x78
                    | 0x79
                    | 0x7a
                    | 0x7b
            )
        );
        if !known {
            return Err(Error::Unsupported(format!(
                "unknown current key tag {:?}",
                key.first()
            )));
        }
        let Some(tag @ (0x70 | 0x73 | 0x74 | 0x75 | 0x76 | 0x77 | 0x78 | 0x79 | 0x7b)) =
            key.first().copied()
        else {
            return Ok(());
        };
        let mut at = 1;
        let id = IndexId(read_ordered(key, &mut at)?);
        let expected = match tag {
            0x70 => IndexFamily::Scalar,
            0x73 => IndexFamily::ExactVector,
            0x74 => IndexFamily::SpatialPoint,
            0x79 => IndexFamily::QuantizedVector,
            _ => IndexFamily::Text,
        };
        match indexes.iter().find(|index| index.id == id) {
            Some(index) if index.family == expected && index.state == IndexState::Ready => {}
            Some(index) if index.family != expected => run.issue(VerificationIssue {
                class: IssueClass::Derived,
                kind: IssueKind::Mismatch,
                key: key.to_vec(),
                index: Some(id),
                entity: None,
                message: "derived namespace belongs to a different index family".into(),
            })?,
            Some(_) => run.issue(VerificationIssue {
                class: IssueClass::Derived,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: Some(id),
                entity: None,
                message: "derived entry belongs to an incompletely verified lifecycle".into(),
            })?,
            None => run.issue(VerificationIssue {
                class: IssueClass::Derived,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: Some(id),
                entity: None,
                message: "orphan derived index namespace".into(),
            })?,
        }
        Ok(())
    })?;

    let ready = indexes
        .iter()
        .filter(|i| i.state == IndexState::Ready)
        .cloned()
        .collect::<Vec<_>>();
    let source = run.reader.clone();
    visit(&source, &[0x40], Some(&[0x41]), |key, row| {
        run.row(false)?;
        let id = row_id(key)?;
        let lid = layout_id(row)?;
        if lid >= header.next_layout {
            return Err(corrupt("row layout exceeds allocator"));
        }
        let next = catalogs
            .iter()
            .find(|(c, _)| c.id == id.collection)
            .map(|(_, next)| *next)
            .ok_or_else(|| corrupt("row belongs to unknown collection"))?;
        if id.sequence >= next {
            return Err(corrupt("row identity exceeds sequence allocator"));
        }
        let l = layout(|k| run.read(k), lid)?;
        for (ordinal, (name, kind)) in l.fields.iter().enumerate() {
            let decoded = field(&l, row, name)?;
            let Kind::Vector(dimension) = kind else {
                continue;
            };
            if let crate::dense_v3::FieldValue::Vector {
                ordinal: found,
                dimension: found_dimension,
            } = decoded
            {
                if found != ordinal || found_dimension != *dimension {
                    return Err(corrupt("declared vector ordinal/dimension"));
                }
                let side_key = vector_key(id, ordinal);
                match run.read(&side_key)? {
                    Some(bytes) => {
                        if vector_indexes::validate_vector(&bytes, *dimension).is_err() {
                            run.issue(VerificationIssue {
                                class: IssueClass::Primary,
                                kind: IssueKind::Malformed,
                                key: side_key,
                                index: None,
                                entity: Some(id),
                                message: "authoritative vector sidecar is malformed".into(),
                            })?;
                        }
                    }
                    None => run.issue(VerificationIssue {
                        class: IssueClass::Primary,
                        kind: IssueKind::Missing,
                        key: side_key,
                        index: None,
                        entity: Some(id),
                        message: "authoritative declared vector sidecar missing".into(),
                    })?,
                }
            }
        }
        match field(&l, row, KEY_FIELD)? {
            crate::dense_v3::FieldValue::Inline(Value::String(external)) => {
                let map = mapping_key(id.collection, &external);
                let actual = run.read(&map)?;
                let expected = ordered(id.sequence);
                run.mismatch(
                    IssueClass::Derived,
                    &map,
                    None,
                    Some(id),
                    actual.as_deref(),
                    Some(&expected),
                    "external-key mapping",
                )?;
            }
            _ => return Err(corrupt("entity hidden external key")),
        }
        for i in ready.iter().filter(|i| i.collection == id.collection) {
            verify_expected(&mut run, i, id, &l, row)?;
        }
        Ok(())
    })?;
    verify_sidecars_and_mappings(&mut run)?;
    for i in &ready {
        verify_actual(&mut run, i)?;
    }
    verify_graph(
        &mut run,
        ih.is_some_and(|h| h.features & graph_collections::GRAPH_FEATURE != 0),
    )?;
    if ih.is_some_and(|h| h.features & graph_collections::GRAPH_FEATURE != 0) {
        run.report.limitations.push("A primary edge and its reverse deleted together is indistinguishable from a legitimate unlink without an external manifest or operation log.");
    }
    run.reader.recheck_source().map_err(Error::from)?;
    run.report.complete = true;
    run.report.clean =
        run.report.catalog_issues + run.report.primary_issues + run.report.derived_issues == 0;
    Ok(run.report)
}

fn verify_index_mappings<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    indexes: &[IndexInfo],
) -> Result<()> {
    let source = run.reader.clone();
    visit(
        &source,
        &[indexes::COLLECTION_INDEX],
        Some(&tag_end(indexes::COLLECTION_INDEX)),
        |key, value| {
            run.row(true)?;
            let mut at = 1;
            let collection =
                CollectionId(u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?);
            let id = IndexId(read_ordered(key, &mut at)?);
            if at != key.len() || collection.0 == 0 || id.0 == 0 || !value.is_empty() {
                return Err(corrupt("collection-index mapping"));
            }
            if !indexes
                .iter()
                .any(|info| info.id == id && info.collection == collection)
            {
                run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Extra,
                    key: key.to_vec(),
                    index: Some(id),
                    entity: None,
                    message: "orphan collection-index mapping".into(),
                })?;
            }
            Ok(())
        },
    )?;
    let source = run.reader.clone();
    visit(
        &source,
        &[indexes::INDEX_NAME],
        Some(&tag_end(indexes::INDEX_NAME)),
        |key, value| {
            run.row(true)?;
            let mut at = 1;
            let collection =
                CollectionId(u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?);
            let name = std::str::from_utf8(&key[at..]).map_err(corrupt)?;
            let mut value_at = 0;
            let id = IndexId(read_ordered(value, &mut value_at)?);
            if value_at != value.len() || collection.0 == 0 || id.0 == 0 {
                return Err(corrupt("index-name mapping"));
            }
            if !indexes
                .iter()
                .any(|info| info.id == id && info.collection == collection && info.name == name)
            {
                run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Extra,
                    key: key.to_vec(),
                    index: Some(id),
                    entity: None,
                    message: "orphan index-name mapping".into(),
                })?;
            }
            Ok(())
        },
    )?;
    Ok(())
}

fn verify_sidecars_and_mappings<F: FnMut(&VerificationIssue)>(run: &mut Run<F>) -> Result<()> {
    let source = run.reader.clone();
    visit(&source, &[0x60], Some(&[0x61]), |key, value| {
        run.row(false)?;
        let mut at = 1;
        let collection = CollectionId(u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?);
        let sequence = read_ordered(key, &mut at)?;
        let ordinal = usize::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?;
        if at != key.len() || collection.0 == 0 || sequence == 0 {
            return Err(corrupt("vector sidecar key"));
        }
        let id = EntityId {
            collection,
            sequence,
        };
        let primary = row_key(id);
        let Some(row) = run.read(&primary)? else {
            run.issue(VerificationIssue {
                class: IssueClass::Primary,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: None,
                entity: Some(id),
                message: "authoritative vector sidecar survives a missing primary row".into(),
            })?;
            return Ok(());
        };
        let l = layout(|k| run.read(k), layout_id(&row)?)?;
        let Some((name, Kind::Vector(dimension))) = l.fields.get(ordinal) else {
            run.issue(VerificationIssue {
                class: IssueClass::Primary,
                kind: IssueKind::Mismatch,
                key: key.to_vec(),
                index: None,
                entity: Some(id),
                message: "vector sidecar ordinal is absent or non-vector".into(),
            })?;
            return Ok(());
        };
        if vector_indexes::validate_vector(value, *dimension).is_err() {
            run.issue(VerificationIssue {
                class: IssueClass::Primary,
                kind: IssueKind::Malformed,
                key: key.to_vec(),
                index: None,
                entity: Some(id),
                message: "authoritative vector sidecar is malformed".into(),
            })?;
            return Ok(());
        }
        if !matches!(field(&l,&row,name)?,crate::dense_v3::FieldValue::Vector{ordinal:found,dimension:found_dimension} if found==ordinal&&found_dimension==*dimension)
        {
            run.issue(VerificationIssue {
                class: IssueClass::Primary,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: None,
                entity: Some(id),
                message: "vector sidecar exists for missing/null row field".into(),
            })?;
        }
        Ok(())
    })?;
    let source = run.reader.clone();
    visit(&source, &[0x20], Some(&[0x21]), |key, value| {
        run.row(true)?;
        let mut at = 1;
        let collection = CollectionId(u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?);
        let external = std::str::from_utf8(&key[at..]).map_err(corrupt)?;
        let mut pos = 0;
        let sequence = read_ordered(value, &mut pos)?;
        if pos != value.len() || sequence == 0 {
            return Err(corrupt("external-key mapping identity"));
        }
        let id = EntityId {
            collection,
            sequence,
        };
        let primary = row_key(id);
        let Some(row) = run.read(&primary)? else {
            run.issue(VerificationIssue {
                class: IssueClass::Derived,
                kind: IssueKind::Extra,
                key: key.to_vec(),
                index: None,
                entity: Some(id),
                message: "external-key mapping has no primary row".into(),
            })?;
            return Ok(());
        };
        let l = layout(|k| run.read(k), layout_id(&row)?)?;
        if !matches!(field(&l,&row,KEY_FIELD)?,crate::dense_v3::FieldValue::Inline(Value::String(found)) if found==external)
        {
            run.issue(VerificationIssue {
                class: IssueClass::Derived,
                kind: IssueKind::Mismatch,
                key: key.to_vec(),
                index: None,
                entity: Some(id),
                message: "external-key mapping disagrees with row".into(),
            })?;
        }
        Ok(())
    })?;
    Ok(())
}

fn verify_expected<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
    id: EntityId,
    l: &Layout,
    row: &[u8],
) -> Result<()> {
    match i.family {
        IndexFamily::Scalar => {
            let f = field(l, row, &i.field)?;
            let value = match &f {
                crate::dense_v3::FieldValue::Inline(v) => Some(v),
                crate::dense_v3::FieldValue::Null => Some(&Value::Null),
                crate::dense_v3::FieldValue::Missing => None,
                _ => return Err(corrupt("scalar field changed kind")),
            };
            let encoded = crate::scalar_key::encode(&i.kind, value)?;
            let key = indexes::skey(i, &encoded, id.sequence);
            let actual = run.read_index(i, &key)?;
            run.mismatch(
                IssueClass::Derived,
                &key,
                Some(i.id),
                Some(id),
                actual.as_deref(),
                Some(&[]),
                "scalar posting",
            )?;
        }
        IndexFamily::ExactVector => {
            if let crate::dense_v3::FieldValue::Vector { ordinal, dimension } =
                field(l, row, &i.field)?
            {
                if dimension != vector_indexes::dimension(i)? {
                    return Err(corrupt("vector dimension changed"));
                }
                let side_key = vector_key(id, ordinal);
                let Some(side) = run.read(&side_key)? else {
                    run.issue(VerificationIssue {
                        class: IssueClass::Primary,
                        kind: IssueKind::Missing,
                        key: side_key,
                        index: Some(i.id),
                        entity: Some(id),
                        message: "authoritative vector sidecar missing".into(),
                    })?;
                    return Ok(());
                };
                if vector_indexes::validate_vector(&side, dimension).is_err() {
                    // The authoritative-row pass already reports this sidecar
                    // as primary damage. It cannot support a certified index
                    // rebuild, but the remaining namespaces are still checked.
                    return Ok(());
                }
                let expected = vector_indexes::encode_locator(l.id as u32, ordinal)?;
                let key = vector_indexes::locator_key(i.id, id.sequence);
                let actual = run.read(&key)?;
                run.mismatch(
                    IssueClass::Derived,
                    &key,
                    Some(i.id),
                    Some(id),
                    actual.as_deref(),
                    Some(&expected),
                    "vector locator",
                )?;
            }
        }
        IndexFamily::QuantizedVector => {
            let expected = quantized_expected(run, i, id, l, row)?;
            if let Some(expected) = expected {
                let key = quantized_vector_indexes::entry_key(i.id, id.sequence);
                let actual = run.read(&key)?;
                run.mismatch(
                    IssueClass::Derived,
                    &key,
                    Some(i.id),
                    Some(id),
                    actual.as_deref(),
                    Some(&expected),
                    "quantized vector entry",
                )?;
            }
        }
        IndexFamily::SpatialPoint => {
            let f = field(l, row, &i.field)?;
            if let crate::dense_v3::FieldValue::Inline(v) = f {
                let mut doc = serde_json::Map::new();
                doc.insert(i.field.clone(), v);
                if let Some(p) = spatial_indexes::selected_point(&Value::Object(doc), &i.field)? {
                    let e = spatial_indexes::point_entry(i, id, p);
                    let a = run.read_index(i, &e.key)?;
                    run.mismatch(
                        IssueClass::Derived,
                        &e.key,
                        Some(i.id),
                        Some(id),
                        a.as_deref(),
                        Some(&e.value),
                        "spatial posting",
                    )?;
                }
            }
        }
        IndexFamily::Text => {
            let f = field(l, row, &i.field)?;
            if let crate::dense_v3::FieldValue::Inline(Value::String(text)) = f {
                let a = crate::text_analyzer::analyze(&text).map_err(invalid)?;
                for (term, tf) in &a.terms {
                    let key = text_indexes::posting_key(i.id, term, id.sequence);
                    let mut actual = run.read(&key)?;
                    // No head row: the posting may be packed inside a segment.
                    // A head row that IS present overrides the segment, so it
                    // is compared as it stands -- including the `tf = 0`
                    // tombstone, which reads as a missing posting.
                    if actual.is_none() {
                        actual = segment_tf(run, i, term, id.sequence)?
                            .map(|packed| packed.to_be_bytes().to_vec());
                    }
                    run.mismatch(
                        IssueClass::Derived,
                        &key,
                        Some(i.id),
                        Some(id),
                        actual.as_deref(),
                        Some(&tf.to_be_bytes()),
                        "text posting",
                    )?;
                }
                let key = text_indexes::norm_key(i.id, id.sequence);
                let mut actual = run.read(&key)?;
                // No head row: the length may be packed inside a `0x7B`
                // block. A head row that IS present overrides the block and
                // is compared as it stands -- including the EMPTY value, the
                // tombstone a delete of a folded document leaves, which reads
                // here as a missing norm for text that still exists.
                if actual.is_none() {
                    actual = packed_norm(run, i, id.sequence)?
                        .map(|length| length.to_be_bytes().to_vec());
                }
                run.mismatch(
                    IssueClass::Derived,
                    &key,
                    Some(i.id),
                    Some(id),
                    actual.as_deref(),
                    Some(&a.length.to_be_bytes()),
                    "text norm",
                )?;
            }
        }
    }
    Ok(())
}

fn quantized_expected<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    index: &IndexInfo,
    id: EntityId,
    layout: &Layout,
    row: &[u8],
) -> Result<Option<Vec<u8>>> {
    let expected_dimension = quantized_vector_indexes::dimension(index)?;
    let field = field(layout, row, &index.field)?;
    let crate::dense_v3::FieldValue::Vector { ordinal, dimension } = field else {
        return Ok(None);
    };
    if dimension != expected_dimension
        || !matches!(layout.fields.get(ordinal),Some((name,Kind::Vector(found))) if name==&index.field && *found==dimension)
    {
        return Err(corrupt(
            "quantized vector row locator field/layout mismatch",
        ));
    }
    let side_key = vector_key(id, ordinal);
    let Some(sidecar) = run.read(&side_key)? else {
        // The authoritative row pass reports the missing sidecar.
        return Ok(None);
    };
    if vector_indexes::validate_vector(&sidecar, dimension).is_err() {
        // The authoritative row pass reports malformed vector bytes.
        return Ok(None);
    }
    let locator =
        vector_indexes::encode_locator(u32::try_from(layout.id).map_err(corrupt)?, ordinal)?;
    quantized_vector_indexes::encode_entry(locator, &sidecar, dimension).map(Some)
}

fn primary_field<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
    seq: u64,
) -> Result<Option<(EntityId, Layout, Vec<u8>)>> {
    let id = EntityId {
        collection: i.collection,
        sequence: seq,
    };
    let key = row_key(id);
    let Some(row) = run.read(&key)? else {
        run.issue(VerificationIssue {
            class: IssueClass::Derived,
            kind: IssueKind::Extra,
            key,
            index: Some(i.id),
            entity: Some(id),
            message: "derived entry has no primary row".into(),
        })?;
        return Ok(None);
    };
    let l = layout(|k| run.read(k), layout_id(&row)?)?;
    Ok(Some((id, l, row)))
}
fn verify_actual<F: FnMut(&VerificationIssue)>(run: &mut Run<F>, i: &IndexInfo) -> Result<()> {
    match i.family {
        IndexFamily::Scalar => {
            let p = indexes::ikey(indexes::SCALAR, i.id);
            let end = prefix_end(&p);
            let source = run.reader.clone();
            let mut last_unique_value: Option<Vec<u8>> = None;
            visit_index(&source, i, &p, end.as_deref(), |key, value| {
                run.row(true)?;
                let (_, n) = match crate::scalar_key::decode(&i.kind, &key[p.len()..]) {
                    Ok(decoded) => decoded,
                    Err(_) => {
                        run.issue(VerificationIssue {
                            class: IssueClass::Derived,
                            kind: IssueKind::Malformed,
                            key: key.to_vec(),
                            index: Some(i.id),
                            entity: None,
                            message: "scalar posting key is malformed".into(),
                        })?;
                        return Ok(());
                    }
                };
                let encoded = &key[p.len()..p.len() + n];
                if i.unique && encoded != [0] {
                    if last_unique_value.as_deref() == Some(encoded) {
                        run.issue(VerificationIssue {
                            class: IssueClass::Derived,
                            kind: IssueKind::Mismatch,
                            key: key.to_vec(),
                            index: Some(i.id),
                            entity: None,
                            message: "unique scalar value names multiple entities".into(),
                        })?;
                    }
                    last_unique_value = Some(encoded.to_vec());
                }
                let mut at = p.len() + n;
                let seq = match read_ordered(key, &mut at) {
                    Ok(seq) => seq,
                    Err(_) => {
                        run.issue(VerificationIssue {
                            class: IssueClass::Derived,
                            kind: IssueKind::Malformed,
                            key: key.to_vec(),
                            index: Some(i.id),
                            entity: None,
                            message: "scalar posting entity suffix is malformed".into(),
                        })?;
                        return Ok(());
                    }
                };
                if at != key.len() || !value.is_empty() {
                    run.issue(VerificationIssue {
                        class: IssueClass::Derived,
                        kind: IssueKind::Malformed,
                        key: key.to_vec(),
                        index: Some(i.id),
                        entity: None,
                        message: "scalar posting key/value is malformed".into(),
                    })?;
                    return Ok(());
                }
                if let Some((id, l, row)) = primary_field(run, i, seq)? {
                    let f = field(&l, &row, &i.field)?;
                    let v = match &f {
                        crate::dense_v3::FieldValue::Inline(v) => Some(v),
                        crate::dense_v3::FieldValue::Null => Some(&Value::Null),
                        crate::dense_v3::FieldValue::Missing => None,
                        _ => return Err(corrupt("scalar field changed kind")),
                    };
                    let e = crate::scalar_key::encode(&i.kind, v)?;
                    let want = indexes::skey(i, &e, seq);
                    run.mismatch(
                        IssueClass::Derived,
                        key,
                        Some(i.id),
                        Some(id),
                        Some(value),
                        (want == key).then_some(&[]),
                        "extra/mismatched scalar posting",
                    )?;
                }
                Ok(())
            })?;
        }
        IndexFamily::ExactVector => {
            let p = vector_indexes::locator_prefix(i.id);
            let end = prefix_end(&p);
            let source = run.reader.clone();
            visit(&source, &p, end.as_deref(), |key, value| {
                run.row(true)?;
                let mut at = p.len();
                let seq = match read_ordered(key, &mut at) {
                    Ok(seq) => seq,
                    Err(_) => {
                        run.issue(VerificationIssue {
                            class: IssueClass::Derived,
                            kind: IssueKind::Malformed,
                            key: key.to_vec(),
                            index: Some(i.id),
                            entity: None,
                            message: "vector locator key is malformed".into(),
                        })?;
                        return Ok(());
                    }
                };
                if at != key.len() {
                    run.issue(VerificationIssue {
                        class: IssueClass::Derived,
                        kind: IssueKind::Malformed,
                        key: key.to_vec(),
                        index: Some(i.id),
                        entity: None,
                        message: "vector locator key is malformed".into(),
                    })?;
                    return Ok(());
                }
                if vector_indexes::decode_locator(value).is_err() {
                    run.issue(VerificationIssue {
                        class: IssueClass::Derived,
                        kind: IssueKind::Malformed,
                        key: key.to_vec(),
                        index: Some(i.id),
                        entity: Some(EntityId {
                            collection: i.collection,
                            sequence: seq,
                        }),
                        message: "vector locator value is malformed".into(),
                    })?;
                    return Ok(());
                }
                if let Some((id, l, row)) = primary_field(run, i, seq)? {
                    let f = field(&l, &row, &i.field)?;
                    let expected = match f {
                        crate::dense_v3::FieldValue::Vector { ordinal, dimension }
                            if dimension == vector_indexes::dimension(i)? =>
                        {
                            Some(vector_indexes::encode_locator(l.id as u32, ordinal)?)
                        }
                        crate::dense_v3::FieldValue::Missing
                        | crate::dense_v3::FieldValue::Null => None,
                        _ => return Err(corrupt("vector locator primary kind")),
                    };
                    run.mismatch(
                        IssueClass::Derived,
                        key,
                        Some(i.id),
                        Some(id),
                        Some(value),
                        expected.as_ref().map(|v| v.as_slice()),
                        "extra/mismatched vector locator",
                    )?;
                }
                Ok(())
            })?;
        }
        IndexFamily::QuantizedVector => {
            let p = quantized_vector_indexes::entry_prefix(i.id);
            let end = prefix_end(&p);
            let source = run.reader.clone();
            visit(&source, &p, end.as_deref(), |key, value| {
                run.row(true)?;
                let mut at = p.len();
                let seq = match read_ordered(key, &mut at) {
                    Ok(seq) if at == key.len() => seq,
                    _ => {
                        malformed(
                            run,
                            IssueClass::Derived,
                            key,
                            Some(i.id),
                            None,
                            "quantized vector entry key is malformed",
                        )?;
                        return Ok(());
                    }
                };
                let dimension = quantized_vector_indexes::dimension(i)?;
                if quantized_vector_indexes::decode_entry(value, dimension).is_err() {
                    malformed(
                        run,
                        IssueClass::Derived,
                        key,
                        Some(i.id),
                        Some(EntityId {
                            collection: i.collection,
                            sequence: seq,
                        }),
                        "quantized vector entry value is malformed",
                    )?;
                    return Ok(());
                }
                if let Some((id, layout, row)) = primary_field(run, i, seq)? {
                    let expected = quantized_expected(run, i, id, &layout, &row)?;
                    run.mismatch(
                        IssueClass::Derived,
                        key,
                        Some(i.id),
                        Some(id),
                        Some(value),
                        expected.as_deref(),
                        "extra/mismatched quantized vector entry",
                    )?;
                }
                Ok(())
            })?;
        }
        IndexFamily::SpatialPoint => {
            let p = spatial_indexes::posting_prefix(i.id);
            let end = prefix_end(&p);
            let source = run.reader.clone();
            visit_index(&source, i, &p, end.as_deref(), |key, value| {
                run.row(true)?;
                let (_, seq, _) = match spatial_indexes::decode_posting(&p, key, value) {
                    Ok(decoded) => decoded,
                    Err(_) => {
                        run.issue(VerificationIssue {
                            class: IssueClass::Derived,
                            kind: IssueKind::Malformed,
                            key: key.to_vec(),
                            index: Some(i.id),
                            entity: None,
                            message: "spatial posting key/value is malformed".into(),
                        })?;
                        return Ok(());
                    }
                };
                if let Some((id, l, row)) = primary_field(run, i, seq)? {
                    let f = field(&l, &row, &i.field)?;
                    let expected = if let crate::dense_v3::FieldValue::Inline(v) = f {
                        let mut d = serde_json::Map::new();
                        d.insert(i.field.clone(), v);
                        spatial_indexes::selected_point(&Value::Object(d), &i.field)?
                            .map(|p| spatial_indexes::point_entry(i, id, p))
                    } else {
                        None
                    };
                    let good = expected
                        .as_ref()
                        .is_some_and(|e| e.key == key && e.value.as_slice() == value);
                    run.mismatch(
                        IssueClass::Derived,
                        key,
                        Some(i.id),
                        Some(id),
                        Some(value),
                        good.then_some(value),
                        "extra/mismatched spatial posting",
                    )?;
                }
                Ok(())
            })?;
        }
        IndexFamily::Text => verify_text_actual(run, i)?,
    }
    Ok(())
}
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut v = prefix.to_vec();
    for i in (0..v.len()).rev() {
        if v[i] != 255 {
            v[i] += 1;
            v.truncate(i + 1);
            return Some(v);
        }
    }
    None
}


/// The packed term frequency of one `(term, document)` pair, or `None`.
///
/// Segments of a term are keyed by the LAST sequence they hold, so the first
/// key at or after `prefix ++ ordered(sequence)` is the only segment that can
/// contain this document; the rest of the range is walked but not read.
fn segment_tf<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
    term: &str,
    sequence: u64,
) -> Result<Option<u32>> {
    let prefix = text_indexes::segments::segment_prefix(i.id, term);
    let mut start = prefix.clone();
    start.extend(ordered(sequence));
    let end = prefix_end(&prefix);
    let source = run.reader.clone();
    let mut found = None;
    let mut first = true;
    visit(&source, &start, end.as_deref(), |_, value| {
        if !first {
            return Ok(());
        }
        first = false;
        if let Ok(postings) = text_indexes::segments::decode(value) {
            found = postings
                .into_iter()
                .find(|(candidate, _)| *candidate == sequence)
                .map(|(_, frequency)| frequency);
        }
        Ok(())
    })?;
    Ok(found)
}

/// Structural pass over the packed norm tier (`0x7B`).
///
/// Every block key must name its block; every value must decode (Law 5 lives
/// in `text_segments::decode_norm_block`); every entry must land in the block
/// its key names; and no two blocks may claim the same document. Returns the
/// `(documents, tokens)` the packed tier contributes to the corpus counters --
/// entries a head `0x76` row overrides are left to the head pass, including
/// the tombstone, which counts for neither.
///
/// Sacrifice (Law 4), the same one the packed posting pass already names: one
/// point read per packed norm to learn whether a head row overrides it. The
/// cross-check against the authoritative primary text is a SAMPLE -- the first
/// and last entry of every block, plus every entry whose sequence is a
/// multiple of 64 -- because the packed posting pass already re-analyzes the
/// primary text of every folded posting, and a second full pass would double
/// the verifier's read cost to re-derive the token count of text it has just
/// tokenized. A block whose lengths were rewritten wholesale still fails at
/// its samples and at the corpus counters, which are reconstructed from EVERY
/// entry, not from the sample.
fn verify_text_norm_blocks<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
) -> Result<(u64, u64)> {
    let p = text_indexes::index_prefix(text_indexes::segments::NORM_BLOCK, i.id);
    let end = prefix_end(&p);
    let source = run.reader.clone();
    let mut previous: Option<u64> = None;
    let mut entries: Vec<(u64, u32, bool)> = Vec::new();
    let mut decoded = Vec::new();
    visit(&source, &p, end.as_deref(), |key, value| {
        run.row(true)?;
        let mut at = p.len();
        let block = match read_ordered(key, &mut at) {
            Ok(block) if at == key.len() => block,
            _ => {
                return malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text norm block key is malformed",
                );
            }
        };
        if previous.is_some_and(|previous| previous >= block) {
            return malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text norm blocks do not ascend",
            );
        }
        previous = Some(block);
        if text_indexes::segments::decode_norm_block(value, &mut decoded).is_err() {
            return malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text norm block value is malformed",
            );
        }
        let base = block
            .checked_mul(text_indexes::segments::NORM_BLOCK_SPAN)
            .ok_or_else(|| corrupt("text norm block base"))?;
        let last = decoded.len().saturating_sub(1);
        for (at, (slot, length)) in decoded.iter().enumerate() {
            let sequence = base
                .checked_add(*slot as u64)
                .ok_or_else(|| corrupt("text norm block sequence"))?;
            if sequence == 0 {
                return malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text norm block names document zero",
                );
            }
            let sampled = at == 0 || at == last || sequence % 64 == 0;
            entries.push((sequence, *length, sampled));
        }
        Ok(())
    })?;

    let (mut documents, mut tokens) = (0u64, 0u64);
    for (sequence, length, sampled) in entries {
        let head = run.read(&text_indexes::norm_key(i.id, sequence))?;
        if head.is_some() {
            // A head row overrides this packed length; the head pass owns it,
            // and an empty head row says the document is gone.
            continue;
        }
        documents = documents
            .checked_add(1)
            .ok_or_else(|| corrupt("text corpus documents"))?;
        tokens = tokens
            .checked_add(u64::from(length))
            .ok_or_else(|| corrupt("text corpus tokens"))?;
        if !sampled {
            continue;
        }
        if let Some((id, layout, row)) = primary_field(run, i, sequence)? {
            let expected = match field(&layout, &row, &i.field)? {
                crate::dense_v3::FieldValue::Inline(Value::String(text)) => {
                    Some(crate::text_analyzer::analyze(&text).map_err(invalid)?.length)
                }
                _ => None,
            };
            run.mismatch(
                IssueClass::Derived,
                &text_indexes::segments::norm_block_key(
                    i.id,
                    text_indexes::segments::norm_block_of(sequence),
                ),
                Some(i.id),
                Some(id),
                Some(&length.to_be_bytes()),
                expected
                    .as_ref()
                    .map(|n| n.to_be_bytes())
                    .as_ref()
                    .map(|b| b.as_slice()),
                "extra/mismatched packed text norm",
            )?;
        }
    }
    Ok((documents, tokens))
}

/// The length a `0x7B` block holds for one document, if any.
fn packed_norm<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
    sequence: u64,
) -> Result<Option<u32>> {
    let key = text_indexes::segments::norm_block_key(
        i.id,
        text_indexes::segments::norm_block_of(sequence),
    );
    let Some(value) = run.read(&key)? else {
        return Ok(None);
    };
    Ok(text_indexes::segments::norm_in_block(&value, sequence).unwrap_or(None))
}

fn index_has_norm_blocks<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
) -> Result<bool> {
    let prefix = text_indexes::index_prefix(text_indexes::segments::NORM_BLOCK, i.id);
    let end = prefix_end(&prefix);
    let source = run.reader.clone();
    let mut any = false;
    visit(&source, &prefix, end.as_deref(), |_, _| {
        any = true;
        Ok(())
    })?;
    Ok(any)
}

fn index_has_segments<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
) -> Result<bool> {
    let prefix = text_indexes::index_prefix(text_indexes::segments::SEGMENT, i.id);
    let end = prefix_end(&prefix);
    let source = run.reader.clone();
    let mut any = false;
    visit(&source, &prefix, end.as_deref(), |_, _| {
        any = true;
        Ok(())
    })?;
    Ok(any)
}

/// Document frequency as the two tiers actually spell it: every head row with
/// a nonzero frequency, plus every packed posting no head row overrides.
///
/// Sacrifice (Law 4): this recount costs one point read per packed posting on
/// top of the pass that produced it. Verification is an explicit, budgeted,
/// offline operation and the alternative -- believing the stored count -- is
/// not verification.
fn merged_df<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
    term: &str,
) -> Result<u64> {
    let mut count = 0u64;
    let head = text_indexes::posting_prefix(i.id, term);
    let head_end = prefix_end(&head);
    let source = run.reader.clone();
    visit(&source, &head, head_end.as_deref(), |_, value| {
        if text_indexes::decode_u32(value, "text posting").is_ok_and(|tf| tf != 0) {
            count = count
                .checked_add(1)
                .ok_or_else(|| corrupt("text df overflow"))?;
        }
        Ok(())
    })?;
    let segments = text_indexes::segments::segment_prefix(i.id, term);
    let segments_end = prefix_end(&segments);
    let source = run.reader.clone();
    let mut decoded = Vec::new();
    let mut sequences = Vec::new();
    visit(&source, &segments, segments_end.as_deref(), |_, value| {
        if text_indexes::segments::decode_into(value, &mut decoded).is_ok() {
            sequences.extend(decoded.iter().map(|(sequence, _)| *sequence));
        }
        Ok(())
    })?;
    for sequence in sequences {
        if run
            .read(&text_indexes::posting_key(i.id, term, sequence))?
            .is_none()
        {
            count = count
                .checked_add(1)
                .ok_or_else(|| corrupt("text df overflow"))?;
        }
    }
    Ok(count)
}

/// Structural pass over the packed tier.
///
/// Every segment key must name a term and the last sequence its value holds;
/// every value must decode (Law 5 lives in `text_segments::decode`); segments
/// of one term must be disjoint and ascending; and every packed posting no
/// head row overrides must agree with the authoritative primary text.
fn verify_text_segments<F: FnMut(&VerificationIssue)>(
    run: &mut Run<F>,
    i: &IndexInfo,
) -> Result<()> {
    let p = text_indexes::index_prefix(text_indexes::segments::SEGMENT, i.id);
    let end = prefix_end(&p);
    let source = run.reader.clone();
    let mut term = String::new();
    let mut previous: Option<u64> = None;
    let mut checks: Vec<(String, u64, u32)> = Vec::new();
    visit(&source, &p, end.as_deref(), |key, value| {
        run.row(true)?;
        let tail = &key[p.len()..];
        let Some(zero) = tail.iter().position(|b| *b == 0) else {
            return malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text segment term terminator is malformed",
            );
        };
        let Ok(t) = std::str::from_utf8(&tail[..zero]) else {
            return malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text segment term is not UTF-8",
            );
        };
        let mut at = p.len() + zero + 1;
        let last = match read_ordered(key, &mut at) {
            Ok(last) if at == key.len() && last != 0 => last,
            _ => {
                return malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text segment identity is malformed",
                );
            }
        };
        if t != term {
            term = t.into();
            previous = None;
        }
        let postings = match text_indexes::segments::decode(value) {
            Ok(postings) => postings,
            Err(_) => {
                return malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text segment value is malformed",
                );
            }
        };
        if postings.last().map(|(sequence, _)| *sequence) != Some(last)
            || previous.is_some_and(|previous| {
                postings.first().is_none_or(|(first, _)| previous >= *first)
            })
        {
            return malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text segments overlap or disagree with their keys",
            );
        }
        previous = Some(last);
        for (sequence, frequency) in postings {
            checks.push((t.to_owned(), sequence, frequency));
        }
        Ok(())
    })?;
    for (term, sequence, frequency) in checks {
        let key = text_indexes::posting_key(i.id, &term, sequence);
        if run.read(&key)?.is_some() {
            // A head row overrides this packed posting; the head pass owns it.
            continue;
        }
        if let Some((id, layout, row)) = primary_field(run, i, sequence)? {
            let expected = match field(&layout, &row, &i.field)? {
                crate::dense_v3::FieldValue::Inline(Value::String(text)) => {
                    crate::text_analyzer::analyze(&text)
                        .map_err(invalid)?
                        .terms
                        .get(&term)
                        .copied()
                }
                _ => None,
            };
            run.mismatch(
                IssueClass::Derived,
                &text_indexes::segments::segment_key(i.id, &term, sequence),
                Some(i.id),
                Some(id),
                Some(&frequency.to_be_bytes()),
                expected
                    .as_ref()
                    .map(|n| n.to_be_bytes())
                    .as_ref()
                    .map(|b| b.as_slice()),
                "extra/mismatched packed text posting",
            )?;
        }
    }
    Ok(())
}

fn verify_text_actual<F: FnMut(&VerificationIssue)>(run: &mut Run<F>, i: &IndexInfo) -> Result<()> {
    let p = text_indexes::index_prefix(text_indexes::POSTING, i.id);
    let end = prefix_end(&p);
    let source = run.reader.clone();
    let segments = index_has_segments(run, i)?;
    let mut term = String::new();
    let finish = |run: &mut Run<F>, term: &str| -> Result<()> {
        if term.is_empty() {
            return Ok(());
        }
        // Counted across both tiers: head rows that are not tombstones, plus
        // packed postings no head row overrides.
        let df = merged_df(run, i, term)?;
        let key = text_indexes::term_stats_key(i.id, term);
        let actual = run.read(&key)?;
        // A term whose last live posting is gone has no statistic at all:
        // the head rows left behind are tombstones, which are not postings.
        let expected = (df != 0).then(|| df.to_be_bytes());
        run.mismatch(
            IssueClass::Derived,
            &key,
            Some(i.id),
            None,
            actual.as_deref(),
            expected.as_ref().map(|b| b.as_slice()),
            "text document frequency",
        )
    };
    visit(&source, &p, end.as_deref(), |key, value| {
        run.row(true)?;
        let tail = &key[p.len()..];
        let Some(zero) = tail.iter().position(|b| *b == 0) else {
            malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text posting term terminator is malformed",
            )?;
            return Ok(());
        };
        let Ok(t) = std::str::from_utf8(&tail[..zero]) else {
            malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text posting term is not UTF-8",
            )?;
            return Ok(());
        };
        let mut at = p.len() + zero + 1;
        let seq = match read_ordered(key, &mut at) {
            Ok(seq) if at == key.len() => seq,
            _ => {
                malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text posting identity is malformed",
                )?;
                return Ok(());
            }
        };
        // `tf = 0` is the tombstone a delete or an update writes when the
        // posting it retires is packed inside a segment. It is a posting-shaped
        // statement that there is NO posting, and it exists only where the
        // packed tier does.
        let frequency = match text_indexes::decode_u32(value, "text posting") {
            Ok(frequency) if frequency != 0 || segments => frequency,
            _ => {
                malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    Some(EntityId {
                        collection: i.collection,
                        sequence: seq,
                    }),
                    "text posting frequency is malformed",
                )?;
                return Ok(());
            }
        };
        if t != term {
            finish(run, &term)?;
            term = t.into();
        }
        if frequency == 0 {
            // A tombstone asserts that there is NO posting here. It outlives
            // the document it retired, so it must not be read as a derived
            // entry pointing at a primary row. The expected-side pass is what
            // catches a tombstone over text that does still contain the term.
            return Ok(());
        }
        if let Some((id, l, row)) = primary_field(run, i, seq)? {
            let expected = match field(&l, &row, &i.field)? {
                crate::dense_v3::FieldValue::Inline(Value::String(s)) => {
                    crate::text_analyzer::analyze(&s)
                        .map_err(invalid)?
                        .terms
                        .get(t)
                        .copied()
                }
                _ => None,
            };
            run.mismatch(
                IssueClass::Derived,
                key,
                Some(i.id),
                Some(id),
                Some(value),
                expected
                    .as_ref()
                    .map(|n| n.to_be_bytes())
                    .as_ref()
                    .map(|b| b.as_slice()),
                "extra/mismatched text posting",
            )?;
        }
        Ok(())
    })?;
    finish(run, &term)?;
    verify_text_segments(run, i)?;

    // The posting pass finds missing/mismatched statistics. This independent
    // statistics pass also finds a checksum-valid statistic with no postings.
    let stats = text_indexes::index_prefix(text_indexes::TERM_STATS, i.id);
    let stats_end = prefix_end(&stats);
    let source = run.reader.clone();
    visit(&source, &stats, stats_end.as_deref(), |key, value| {
        run.row(true)?;
        let tail = &key[stats.len()..];
        if tail.last() != Some(&0) {
            malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text term-stat key is malformed",
            )?;
            return Ok(());
        }
        let Ok(term) = std::str::from_utf8(&tail[..tail.len() - 1]) else {
            malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text term-stat term is not UTF-8",
            )?;
            return Ok(());
        };
        let stored = match text_indexes::decode_count(value, "text term-stat value") {
            Ok(stored) => stored,
            Err(_) => {
                malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text term-stat value is malformed",
                )?;
                return Ok(());
            }
        };
        if stored == 0 {
            malformed(
                run,
                IssueClass::Derived,
                key,
                Some(i.id),
                None,
                "text document frequency is zero",
            )?;
            return Ok(());
        }
        let count = merged_df(run, i, term)?;
        run.mismatch(
            IssueClass::Derived,
            key,
            Some(i.id),
            None,
            Some(value),
            Some(&count.to_be_bytes()),
            "orphan/mismatched text term statistics",
        )?;
        Ok(())
    })?;
    let p = text_indexes::index_prefix(text_indexes::NORM, i.id);
    let end = prefix_end(&p);
    let source = run.reader.clone();
    let packed_norms = index_has_norm_blocks(run, i)?;
    let (mut documents, mut tokens) = (0u64, 0u64);
    visit(&source, &p, end.as_deref(), |key, value| {
        run.row(true)?;
        let mut at = p.len();
        let seq = match read_ordered(key, &mut at) {
            Ok(seq) if at == key.len() => seq,
            _ => {
                malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    None,
                    "text norm key is malformed",
                )?;
                return Ok(());
            }
        };
        // The EMPTY value is the norm tombstone: a norm-shaped statement that
        // this document is NOT in the index, written when a delete retires a
        // length that is packed inside a block it cannot cheaply rewrite. Like
        // the `tf = 0` posting tombstone it outlives the document it retired,
        // so it is neither counted into the corpus nor read as a derived entry
        // pointing at a primary row. It exists only where a block does.
        if value.is_empty() && packed_norms {
            return Ok(());
        }
        let len = match text_indexes::decode_u32(value, "text norm") {
            Ok(len) => len,
            Err(_) => {
                malformed(
                    run,
                    IssueClass::Derived,
                    key,
                    Some(i.id),
                    Some(EntityId {
                        collection: i.collection,
                        sequence: seq,
                    }),
                    "text norm value is malformed",
                )?;
                return Ok(());
            }
        };
        documents += 1;
        tokens = tokens
            .checked_add(len.into())
            .ok_or_else(|| corrupt("text corpus tokens"))?;
        if let Some((id, l, row)) = primary_field(run, i, seq)? {
            let expected = match field(&l, &row, &i.field)? {
                crate::dense_v3::FieldValue::Inline(Value::String(s)) => {
                    Some(crate::text_analyzer::analyze(&s).map_err(invalid)?.length)
                }
                _ => None,
            };
            run.mismatch(
                IssueClass::Derived,
                key,
                Some(i.id),
                Some(id),
                Some(value),
                expected
                    .as_ref()
                    .map(|n| n.to_be_bytes())
                    .as_ref()
                    .map(|b| b.as_slice()),
                "extra/mismatched text norm",
            )?;
        }
        Ok(())
    })?;
    let (block_documents, block_tokens) = verify_text_norm_blocks(run, i)?;
    documents = documents
        .checked_add(block_documents)
        .ok_or_else(|| corrupt("text corpus documents"))?;
    tokens = tokens
        .checked_add(block_tokens)
        .ok_or_else(|| corrupt("text corpus tokens"))?;
    let key = text_indexes::corpus_key(i.id);
    let actual = run.read(&key)?;
    let mut expected = [0; 16];
    expected[..8].copy_from_slice(&documents.to_be_bytes());
    expected[8..].copy_from_slice(&tokens.to_be_bytes());
    if let Some(bytes) = actual.as_deref() {
        if text_indexes::decode_corpus(bytes).is_err() {
            malformed(
                run,
                IssueClass::Derived,
                &key,
                Some(i.id),
                None,
                "text corpus statistics are malformed",
            )?;
            return Ok(());
        }
    }
    run.mismatch(
        IssueClass::Derived,
        &key,
        Some(i.id),
        None,
        actual.as_deref(),
        Some(&expected),
        "text corpus statistics",
    )?;
    Ok(())
}

fn verify_graph<F: FnMut(&VerificationIssue)>(run: &mut Run<F>, enabled: bool) -> Result<()> {
    let graph_header = if !enabled {
        for tag in [
            graph_collections::GRAPH_HEADER,
            graph_collections::NAME_DESCRIPTOR,
            graph_collections::NAME_LOOKUP,
            graph_collections::PRIMARY_EDGE,
            graph_collections::REVERSE_EDGE,
        ] {
            let source = run.reader.clone();
            let mut present = false;
            visit(&source, &[tag], Some(&tag_end(tag)), |_, _| {
                present = true;
                Ok(())
            })?;
            if present {
                run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Mismatch,
                    key: vec![tag],
                    index: None,
                    entity: None,
                    message: "graph rows exist without graph feature".into(),
                })?;
            }
        }
        None
    } else {
        let header = graph_collections::read_graph_header(|k| run.read(k))?;
        for copy in 0..3u8 {
            let key = graph_collections::graph_header_key(copy);
            match run.read(&key)? {
                None => run.issue(VerificationIssue {
                    class: IssueClass::Catalog,
                    kind: IssueKind::Missing,
                    key,
                    index: None,
                    entity: None,
                    message: "graph header replica missing".into(),
                })?,
                Some(bytes) => match graph_collections::decode_graph_header(&bytes) {
                    Ok(candidate) if candidate == header => {}
                    Ok(_) => return Err(corrupt("conflicting graph header replicas")),
                    Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                    Err(_) => run.issue(VerificationIssue {
                        class: IssueClass::Catalog,
                        kind: IssueKind::Malformed,
                        key,
                        index: None,
                        entity: None,
                        message: "graph header replica damaged".into(),
                    })?,
                },
            }
        }
        let source = run.reader.clone();
        let mut counts = [0u32; 2];
        visit(
            &source,
            &[graph_collections::NAME_LOOKUP],
            Some(&tag_end(graph_collections::NAME_LOOKUP)),
            |key, value| {
                run.row(true)?;
                if key.len() < 3 || key[1] > 1 {
                    return Err(corrupt("graph lookup key"));
                }
                let name = std::str::from_utf8(&key[2..]).map_err(corrupt)?;
                let mut at = 0;
                let id = read_ordered(value, &mut at)?;
                if at != value.len() || id == 0 {
                    return Err(corrupt("graph lookup identity"));
                }
                let next = if key[1] == 0 {
                    header.next_type
                } else {
                    header.next_context
                };
                if id >= next {
                    return Err(corrupt("graph lookup identity exceeds allocator"));
                }
                let descriptor = graph_collections::read_name(|k| run.read(k), key[1], id)?;
                if descriptor.name != name {
                    return Err(corrupt("graph lookup/descriptor mismatch"));
                }
                for copy in 0..3u8 {
                    let replica = graph_collections::name_descriptor_key(key[1], copy, id);
                    match run.read(&replica)? {
                        None => run.issue(VerificationIssue {
                            class: IssueClass::Catalog,
                            kind: IssueKind::Missing,
                            key: replica,
                            index: None,
                            entity: None,
                            message: "graph name descriptor replica missing".into(),
                        })?,
                        Some(bytes) => match graph_collections::decode_name(&bytes) {
                            Ok(candidate) if candidate == descriptor => {}
                            Ok(_) => return Err(corrupt("conflicting graph name replicas")),
                            Err(Error::Unsupported(message)) => {
                                return Err(Error::Unsupported(message))
                            }
                            Err(_) => run.issue(VerificationIssue {
                                class: IssueClass::Catalog,
                                kind: IssueKind::Malformed,
                                key: replica,
                                index: None,
                                entity: None,
                                message: "graph name descriptor replica damaged".into(),
                            })?,
                        },
                    }
                }
                counts[key[1] as usize] = counts[key[1] as usize]
                    .checked_add(1)
                    .ok_or_else(|| corrupt("graph name count overflow"))?;
                Ok(())
            },
        )?;
        if counts != [header.type_count, header.context_count] {
            run.issue(VerificationIssue {
                class: IssueClass::Catalog,
                kind: IssueKind::Mismatch,
                key: vec![graph_collections::GRAPH_HEADER],
                index: None,
                entity: None,
                message: "graph header name counts disagree with lookups".into(),
            })?;
        }
        let source = run.reader.clone();
        visit(
            &source,
            &[graph_collections::NAME_DESCRIPTOR],
            Some(&tag_end(graph_collections::NAME_DESCRIPTOR)),
            |key, value| {
                run.row(true)?;
                if key.len() < 4 || key[1] > 1 || key[2] > 2 {
                    return Err(corrupt("graph name descriptor key"));
                }
                let mut at = 3;
                let id = read_ordered(key, &mut at)?;
                if at != key.len() || id == 0 {
                    return Err(corrupt("graph name descriptor identity"));
                }
                let next = if key[1] == 0 {
                    header.next_type
                } else {
                    header.next_context
                };
                if id >= next {
                    return Err(corrupt("graph descriptor identity exceeds allocator"));
                }
                let name = match graph_collections::decode_name(value) {
                    Ok(name) => name,
                    Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                    Err(_) => {
                        run.issue(VerificationIssue {
                            class: IssueClass::Catalog,
                            kind: IssueKind::Malformed,
                            key: key.to_vec(),
                            index: None,
                            entity: None,
                            message: "graph name descriptor replica damaged".into(),
                        })?;
                        return Ok(());
                    }
                };
                if name.kind != key[1] || name.id != id {
                    return Err(corrupt("graph name descriptor key/value identity"));
                }
                let lookup = graph_collections::name_lookup_key(name.kind, &name.name);
                let expected = ordered(id);
                let actual = run.read(&lookup)?;
                run.mismatch(
                    IssueClass::Catalog,
                    &lookup,
                    None,
                    None,
                    actual.as_deref(),
                    Some(&expected),
                    "orphan graph name descriptor",
                )?;
                Ok(())
            },
        )?;
        Some(header)
    };
    let p = [graph_collections::PRIMARY_EDGE];
    let source = run.reader.clone();
    visit(&source, &p, Some(&tag_end(p[0])), |key, value| {
        run.row(false)?;
        let edge = match graph_collections::parse_edge_key(key, p[0]) {
            Ok(edge) => edge,
            Err(_) => {
                run.issue(VerificationIssue {
                    class: IssueClass::Primary,
                    kind: IssueKind::Malformed,
                    key: key.to_vec(),
                    index: None,
                    entity: None,
                    message: "authoritative graph edge key is malformed".into(),
                })?;
                return Ok(());
            }
        };
        if let Some(header) = graph_header {
            if edge.edge_type.0 >= header.next_type
                || (edge.context.0 != 0 && edge.context.0 >= header.next_context)
            {
                return Err(corrupt("graph edge identity exceeds name allocator"));
            }
        }
        match graph_collections::decode_properties(value) {
            Ok(_) => {}
            Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
            Err(_) => run.issue(VerificationIssue {
                class: IssueClass::Primary,
                kind: IssueKind::Malformed,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "authoritative graph edge properties are malformed".into(),
            })?,
        }
        for id in [edge.source, edge.destination] {
            let k = row_key(id);
            if run.read(&k)?.is_none() {
                run.issue(VerificationIssue {
                    class: IssueClass::Primary,
                    kind: IssueKind::Missing,
                    key: k,
                    index: None,
                    entity: Some(id),
                    message: "graph edge endpoint row missing".into(),
                })?;
            }
        }
        let reverse = graph_collections::edge_key(graph_collections::REVERSE_EDGE, edge);
        let actual = run.read(&reverse)?;
        run.mismatch(
            IssueClass::Derived,
            &reverse,
            None,
            None,
            actual.as_deref(),
            Some(&[]),
            "graph reverse marker missing",
        )?;
        Ok(())
    })?;
    let p = [graph_collections::REVERSE_EDGE];
    let source = run.reader.clone();
    visit(&source, &p, Some(&tag_end(p[0])), |key, value| {
        run.row(true)?;
        let edge = match graph_collections::parse_edge_key(key, p[0]) {
            Ok(edge) => edge,
            Err(_) => {
                run.issue(VerificationIssue {
                    class: IssueClass::Derived,
                    kind: IssueKind::Malformed,
                    key: key.to_vec(),
                    index: None,
                    entity: None,
                    message: "graph reverse key is malformed".into(),
                })?;
                return Ok(());
            }
        };
        if let Some(header) = graph_header {
            if edge.edge_type.0 >= header.next_type
                || (edge.context.0 != 0 && edge.context.0 >= header.next_context)
            {
                return Err(corrupt("graph reverse identity exceeds name allocator"));
            }
        }
        if !value.is_empty() {
            run.issue(VerificationIssue {
                class: IssueClass::Derived,
                kind: IssueKind::Mismatch,
                key: key.to_vec(),
                index: None,
                entity: None,
                message: "graph reverse marker is nonempty".into(),
            })?;
        }
        let primary = graph_collections::edge_key(graph_collections::PRIMARY_EDGE, edge);
        if run.read(&primary)?.is_none() {
            run.issue(VerificationIssue {
                class: IssueClass::Primary,
                kind: IssueKind::Missing,
                key: primary,
                index: None,
                entity: None,
                message:
                    "authoritative graph primary edge missing; reverse cannot rebuild properties"
                        .into(),
            })?;
        }
        Ok(())
    })?;
    Ok(())
}
