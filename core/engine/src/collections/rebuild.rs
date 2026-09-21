//! Source-preserving reconstruction of derived typed-index records.
//!
//! The source is held through [`CurrentSourceReader`] for the whole operation.
//! Only a new, non-overlapping destination is written; `COMPLETE` is created
//! after independent typed reopen, verification, byte comparison, and a final
//! source fingerprint check.
use super::*;
use crate::pagewal::{CurrentReaderLimits, CurrentSourceReader, HINT_FILE_BYTES};
use kernel::{
    io::{self, IoMode},
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{
    cell::Cell,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

const INCOMPLETE: &str = "REBUILD_INCOMPLETE";
const COMPLETE_PENDING: &str = "COMPLETE.pending";
const COMPLETE: &str = "COMPLETE";
const INCOMPLETE_CONTENT: &[u8] = b"e4-index-rebuild-v1 incomplete\n";

#[derive(Clone, Copy, Debug)]
pub struct RebuildLimits {
    pub source: CurrentReaderLimits,
    pub cache_bytes: usize,
    pub batch: usize,
    pub max_metadata: usize,
    pub max_records: u64,
    pub max_point_reads: u64,
    /// Sum of destination regular-file lengths. Filesystem allocation,
    /// directory entries, metadata, and reserved extents are outside this
    /// logical-byte budget.
    pub max_destination_logical_bytes: u64,
}

impl Default for RebuildLimits {
    fn default() -> Self {
        Self {
            source: CurrentReaderLimits::default(),
            cache_bytes: 4 << 20,
            batch: 256,
            max_metadata: 4096,
            max_records: 50_000_000,
            max_point_reads: 100_000_000,
            max_destination_logical_bytes: 1 << 40,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub collections: usize,
    pub layouts: usize,
    pub indexes: usize,
    pub primary_rows: u64,
    pub vector_sidecars: u64,
    pub primary_edges: u64,
    pub source_records_visited: u64,
    pub source_point_reads: u64,
    pub destination_logical_bytes: u64,
}

struct SourceView {
    reader: Arc<CurrentSourceReader>,
    limits: RebuildLimits,
    records: Cell<u64>,
    points: Cell<u64>,
}

impl SourceView {
    fn charge_records(&self, amount: u64) -> Result<()> {
        let records = self
            .records
            .get()
            .checked_add(amount)
            .ok_or_else(|| corrupt("index rebuild record counter overflow"))?;
        if records > self.limits.max_records {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index rebuild record budget exceeded",
            )));
        }
        self.records.set(records);
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let points = self
            .points
            .get()
            .checked_add(1)
            .ok_or_else(|| corrupt("rebuild point-read counter overflow"))?;
        if points > self.limits.max_point_reads {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "index rebuild point-read budget exceeded",
            )));
        }
        self.points.set(points);
        self.reader.get(key).map_err(Error::from)
    }

    fn visit(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        mut callback: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<u64> {
        let mut detail = None;
        let raw = self.reader.visit_range(start, end, |key, value| {
            if let Err(error) = self.charge_records(1) {
                detail = Some(error);
                return Err(kernel::Error::ResourceLimit(
                    "index rebuild record budget exceeded",
                ));
            }
            match callback(key, value) {
                Ok(()) => Ok(()),
                Err(error) => {
                    detail = Some(error);
                    Err(kernel::Error::Corrupt {
                        page_no: 0,
                        why: "typed rebuild callback refused",
                    })
                }
            }
        });
        if let Some(error) = detail {
            return Err(error);
        }
        raw.map_err(Error::from)
    }
}

struct Destination {
    store: PageWalStore,
    batch: usize,
    pending: usize,
}

impl Destination {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.store.put(key, value)?;
        self.pending += 1;
        if self.pending == self.batch {
            self.store.commit()?;
            self.pending = 0;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.store.is_dirty() {
            self.store.commit()?;
        }
        self.pending = 0;
        Ok(())
    }
}

fn tag_end(tag: u8) -> Vec<u8> {
    vec![tag.saturating_add(1)]
}

fn replicated_raw<T: PartialEq>(
    source: &SourceView,
    key: impl Fn(u8) -> Vec<u8>,
    parse: impl Fn(&[u8]) -> Result<T>,
) -> Result<(T, Vec<u8>)> {
    let canonical = replicas(|key| source.get(key), &key, &parse)?;
    for copy in 0..3u8 {
        if let Some(bytes) = source.get(&key(copy))? {
            match parse(&bytes) {
                Ok(candidate) if candidate == canonical => return Ok((canonical, bytes)),
                Err(Error::Unsupported(message)) => return Err(Error::Unsupported(message)),
                _ => {}
            }
        }
    }
    Err(corrupt("replicated metadata has no canonical bytes"))
}

fn catalog(bytes: &[u8]) -> Result<Catalog> {
    if bytes.len() == PAD && bytes.starts_with(b"E4CAT") && &bytes[..8] != CATALOG_MAGIC {
        unpack(bytes, bytes[..8].try_into().unwrap())?;
        return Err(Error::Unsupported(
            "collection descriptor version is newer than this binary".into(),
        ));
    }
    parse_catalog(bytes)
}

fn sequence(bytes: &[u8]) -> Result<u64> {
    if bytes.len() == PAD && bytes.starts_with(b"E4SEQ") && &bytes[..8] != COUNTER_MAGIC {
        unpack(bytes, bytes[..8].try_into().unwrap())?;
        return Err(Error::Unsupported(
            "sequence allocator version is newer than this binary".into(),
        ));
    }
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

fn decode_layout(bytes: &[u8]) -> Result<Layout> {
    match Layout::from_descriptor(bytes) {
        Ok(layout) => Ok(layout),
        Err(error) => {
            let message = error.to_string();
            if (bytes.len() == PAD
                && bytes.starts_with(b"E4")
                && !bytes.starts_with(b"E4P0LAY\0")
                && crc32c::crc32c(&bytes[..PAD - 4])
                    == u32::from_le_bytes(bytes[PAD - 4..].try_into().unwrap()))
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

fn destination_path(source: &Path, destination: &Path) -> Result<(PathBuf, PathBuf)> {
    let source = fs::canonicalize(source).map_err(kernel::Error::from)?;
    let parent = fs::canonicalize(
        destination
            .parent()
            .ok_or_else(|| invalid("rebuild destination requires a parent"))?,
    )
    .map_err(kernel::Error::from)?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("rebuild destination requires a name"))?;
    let destination = parent.join(name);
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(invalid("rebuild destination must not exist"));
    }
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err(invalid("rebuild source and destination overlap"));
    }
    Ok((source, destination))
}

fn config(cache_bytes: usize) -> Config {
    Config {
        budget_bytes: cache_bytes,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path).map_err(kernel::Error::from)? {
        let entry = entry.map_err(kernel::Error::from)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(kernel::Error::from)?;
        if !metadata.file_type().is_file() {
            return Err(corrupt(
                "rebuild destination is not a flat regular-file directory",
            ));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| corrupt("rebuild destination byte count overflow"))?;
    }
    Ok(total)
}

fn create_destination_store(
    destination: &Path,
    cache_bytes: usize,
    compact_cells: bool,
) -> Result<PageWalStore> {
    PageWalStore::create_with_compact_cells(destination, cache_bytes, compact_cells)
        .map_err(Error::from)
}

fn marker(path: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(name))
        .map_err(kernel::Error::from)?;
    file.write_all(bytes).map_err(kernel::Error::from)?;
    file.sync_all().map_err(kernel::Error::from)?;
    io::sync_directory(path)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct CompletionCounts {
    collections: u64,
    layouts: u64,
    indexes: u64,
    primary_rows: u64,
    vector_sidecars: u64,
    primary_edges: u64,
    destination_logical_bytes_before_completion: u64,
}

fn completion_bytes(
    source: &Path,
    destination: &Path,
    counts: CompletionCounts,
) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(&json!({
        "version": 1,
        "kind": "e4-derived-index-rebuild",
        "source": source,
        "destination": destination,
        "collections": counts.collections,
        "layouts": counts.layouts,
        "indexes": counts.indexes,
        "primary_rows": counts.primary_rows,
        "vector_sidecars": counts.vector_sidecars,
        "primary_edges": counts.primary_edges,
        "destination_logical_bytes_before_completion": counts.destination_logical_bytes_before_completion,
        "disk_budget_scope": "sum of regular-file lengths, bounded before destination creation; excludes filesystem allocation, directory metadata, and reserved extents",
        "source_preserved": true,
        "destination_verified": true,
        "publication": "separate destination; source not replaced",
        "limitations": [
            "A graph primary edge and reverse deleted together cannot be distinguished from a legitimate unlink without an external manifest or operation log."
        ]
    }))
    .map_err(|error| corrupt(format!("rebuild completion serialization: {error}")))
}

fn maximum_control_bytes(source: &Path, destination: &Path) -> Result<(u64, usize)> {
    let maximum = completion_bytes(
        source,
        destination,
        CompletionCounts {
            collections: u64::MAX,
            layouts: u64::MAX,
            indexes: u64::MAX,
            primary_rows: u64::MAX,
            vector_sidecars: u64::MAX,
            primary_edges: u64::MAX,
            destination_logical_bytes_before_completion: u64::MAX,
        },
    )?;
    let completion_len = maximum.len();
    let bytes = HINT_FILE_BYTES
        .checked_add(INCOMPLETE_CONTENT.len() as u64)
        .and_then(|value| value.checked_add(completion_len as u64))
        .ok_or_else(|| corrupt("rebuild control-byte reserve overflow"))?;
    Ok((bytes, completion_len))
}

fn collect_index_ids(source: &SourceView, header: Option<IndexHeader>) -> Result<Vec<IndexId>> {
    let mut ids = Vec::new();
    source.visit(
        &[catalog::DESCRIPTOR],
        Some(&tag_end(catalog::DESCRIPTOR)),
        |key, _| {
            if key.len() < 3 || key[1] > 2 {
                return Err(corrupt("index descriptor replica key"));
            }
            let mut at = 2;
            let id = IndexId(read_ordered(key, &mut at)?);
            if at != key.len() || id.0 == 0 || header.is_some_and(|h| id.0 >= h.next) {
                return Err(corrupt("index descriptor identity/allocator"));
            }
            if !ids.contains(&id) {
                if ids.len() == source.limits.max_metadata {
                    return Err(Error::Kernel(kernel::Error::ResourceLimit(
                        "index rebuild metadata budget exceeded",
                    )));
                }
                ids.push(id);
            }
            Ok(())
        },
    )?;
    if ids.len() != header.map_or(0, |h| h.count as usize) {
        return Err(corrupt(
            "index descriptor count does not prove the preserved header count",
        ));
    }
    ids.sort_unstable();
    Ok(ids)
}

fn put_replicas(
    destination: &mut Destination,
    key: impl Fn(u8) -> Vec<u8>,
    bytes: &[u8],
) -> Result<()> {
    for copy in 0..3u8 {
        destination.put(&key(copy), bytes)?;
    }
    Ok(())
}

struct Metadata {
    header: HeaderInfo,
    catalogs: Vec<(Catalog, u64)>,
    layouts: Vec<Layout>,
    indexes: Vec<IndexInfo>,
    graph_header: Option<(crate::index::graph::GraphHeader, Vec<u8>)>,
    graph_names: Vec<(u8, crate::index::graph::GraphName, Vec<u8>)>,
}

fn collect_metadata(source: &SourceView) -> Result<Metadata> {
    let (header, _) = replicated_raw(source, |copy| vec![0, 0, copy], parse_header)?;
    let collections = usize::try_from(header.next_collection - 1)
        .map_err(|_| corrupt("collection allocator domain"))?;
    let layouts =
        usize::try_from(header.next_layout - 1).map_err(|_| corrupt("layout allocator domain"))?;
    if collections > source.limits.max_metadata || layouts > source.limits.max_metadata {
        return Err(Error::Kernel(kernel::Error::ResourceLimit(
            "index rebuild metadata budget exceeded",
        )));
    }
    let mut catalogs = Vec::with_capacity(collections);
    for id in 1..header.next_collection {
        let (catalog, _) = replicated_raw(source, |copy| replica_key(1, id, copy), catalog)?;
        if catalog.id.0 != id || catalog.layout == 0 || catalog.layout >= header.next_layout {
            return Err(corrupt("collection descriptor identity/allocator"));
        }
        let (next, _) = replicated_raw(source, |copy| replica_key(2, id, copy), sequence)?;
        catalogs.push((catalog, next));
    }
    let mut decoded_layouts = Vec::with_capacity(layouts);
    for id in 1..header.next_layout {
        let (layout, _) = replicated_raw(source, |copy| layout_key(id, copy), decode_layout)?;
        if layout.id != u64::from(id) {
            return Err(corrupt("layout descriptor identity"));
        }
        decoded_layouts.push(layout);
    }
    for (catalog, _) in &catalogs {
        if !decoded_layouts
            .iter()
            .any(|layout| layout.id == u64::from(catalog.layout))
        {
            return Err(corrupt("collection current layout missing"));
        }
    }

    let ids = collect_index_ids(source, header.indexes)?;
    let mut decoded_indexes = Vec::with_capacity(ids.len());
    for id in ids {
        let (index, _) = replicated_raw(source, |copy| catalog::dkey(id, copy), catalog::decode)?;
        if index.id != id || index.state != IndexState::Ready {
            return Err(Error::Unsupported(
                "index rebuild requires every declared index to be READY".into(),
            ));
        }
        let Some((catalog, _)) = catalogs
            .iter()
            .find(|(catalog, _)| catalog.id == index.collection)
        else {
            return Err(corrupt("index belongs to an unknown collection"));
        };
        let layout = decoded_layouts
            .iter()
            .find(|layout| layout.id == u64::from(catalog.layout))
            .ok_or_else(|| corrupt("indexed collection layout missing"))?;
        if !layout
            .fields
            .iter()
            .any(|(name, kind)| name == &index.field && kind == &index.kind)
        {
            return Err(corrupt("index field does not match the current layout"));
        }
        let feature = match index.family {
            IndexFamily::Scalar => 1,
            IndexFamily::ExactVector => crate::index::vector::exact::VECTOR_FEATURE,
            IndexFamily::QuantizedVector => crate::index::vector::quantized::QUANTIZED_VECTOR_FEATURE,
            IndexFamily::SpatialPoint => crate::index::spatial::point::SPATIAL_FEATURE,
            IndexFamily::SpatialGeometry => crate::index::spatial::geometry_index::GEOMETRY_FEATURE,
            IndexFamily::Text => crate::index::text::TEXT_FEATURE,
        };
        if !header
            .indexes
            .is_some_and(|value| value.features & feature != 0)
        {
            return Err(corrupt("index descriptor lacks its required feature"));
        }
        decoded_indexes.push(index);
    }

    let graph_enabled = header
        .indexes
        .is_some_and(|value| value.features & crate::index::graph::GRAPH_FEATURE != 0);
    let mut graph_header = None;
    let mut graph_names = Vec::new();
    if graph_enabled {
        let (decoded, bytes) = replicated_raw(
            source,
            crate::index::graph::graph_header_key,
            crate::index::graph::decode_graph_header,
        )?;
        for (kind, next) in [(0, decoded.next_type), (1, decoded.next_context)] {
            let count = usize::try_from(next - 1).map_err(|_| corrupt("graph allocator domain"))?;
            if graph_names.len().saturating_add(count) > source.limits.max_metadata {
                return Err(Error::Kernel(kernel::Error::ResourceLimit(
                    "index rebuild graph metadata budget exceeded",
                )));
            }
            for id in 1..next {
                let (name, bytes) = replicated_raw(
                    source,
                    |copy| crate::index::graph::name_descriptor_key(kind, copy, id),
                    crate::index::graph::decode_name,
                )?;
                if name.kind != kind || name.id != id {
                    return Err(corrupt("graph name identity"));
                }
                graph_names.push((kind, name, bytes));
            }
        }
        if decoded.type_count as u64 + 1 != decoded.next_type
            || decoded.context_count as u64 + 1 != decoded.next_context
        {
            return Err(corrupt("graph allocator/count"));
        }
        graph_header = Some((decoded, bytes));
    }
    Ok(Metadata {
        header,
        catalogs,
        layouts: decoded_layouts,
        indexes: decoded_indexes,
        graph_header,
        graph_names,
    })
}

fn seed_metadata(destination: &mut Destination, metadata: &Metadata) -> Result<()> {
    let header = header_bytes(metadata.header)?;
    put_replicas(destination, |copy| vec![0, 0, copy], &header)?;
    for (catalog, next) in &metadata.catalogs {
        let bytes = catalog_bytes(catalog)?;
        put_replicas(
            destination,
            |copy| replica_key(1, catalog.id.0, copy),
            &bytes,
        )?;
        let bytes = packet(COUNTER_MAGIC, &next.to_be_bytes())?;
        put_replicas(
            destination,
            |copy| replica_key(2, catalog.id.0, copy),
            &bytes,
        )?;
        let name = name_key(&catalog.name);
        if destination.store.get(&name)?.is_some() {
            return Err(corrupt(
                "duplicate collection name in authoritative catalog",
            ));
        }
        destination.put(&name, &catalog.id.0.to_be_bytes())?;
    }
    for layout in &metadata.layouts {
        let id = u32::try_from(layout.id).map_err(corrupt)?;
        let bytes = layout.descriptor().map_err(corrupt)?;
        put_replicas(destination, |copy| layout_key(id, copy), &bytes)?;
    }
    for index in &metadata.indexes {
        let mut building = index.clone();
        building.state = IndexState::Building { after: 0 };
        // A per-index tree's root is a page NUMBER, and page numbers are local
        // to a database. The destination is fresh, so the rebuilt index starts
        // with an empty tree (root 0) and the build packs a new one there; the
        // tree id is kept so the two databases name the same index the same
        // way. Carrying the source root over would have pointed the
        // destination's index at whatever page happened to occupy that number
        // -- caught as "page belongs to another tree", which is the check
        // doing its job, not a near miss.
        if let Some(t) = building.tree {
            building.tree = Some(catalog::IndexTree { id: t.id, root: 0 });
        }
        let bytes = catalog::encode(&building)?;
        put_replicas(destination, |copy| catalog::dkey(index.id, copy), &bytes)?;
        destination.put(
            &catalog::ikey(catalog::REGISTRY, index.id),
            &index.collection.0.to_be_bytes(),
        )?;
        destination.put(&catalog::ckey(index.collection, index.id), &[])?;
        let name = catalog::nkey(index.collection, &index.name);
        if destination.store.get(&name)?.is_some() {
            return Err(corrupt("duplicate index name in authoritative catalog"));
        }
        destination.put(&name, &ordered(index.id.0))?;
        if index.family == IndexFamily::Text {
            destination.put(&crate::index::text::corpus_key(index.id), &[0; 16])?;
        }
    }
    if let Some((_, bytes)) = &metadata.graph_header {
        put_replicas(destination, crate::index::graph::graph_header_key, bytes)?;
        for (kind, name, bytes) in &metadata.graph_names {
            put_replicas(
                destination,
                |copy| crate::index::graph::name_descriptor_key(*kind, copy, name.id),
                bytes,
            )?;
            let lookup = crate::index::graph::name_lookup_key(*kind, &name.name);
            if destination.store.get(&lookup)?.is_some() {
                return Err(corrupt("duplicate graph name in authoritative dictionary"));
            }
            destination.put(&lookup, &ordered(name.id))?;
        }
    }
    Ok(())
}

fn layout_for<'a>(metadata: &'a Metadata, id: u32) -> Result<&'a Layout> {
    metadata
        .layouts
        .iter()
        .find(|layout| layout.id == u64::from(id))
        .ok_or_else(|| corrupt("row references an unknown historical layout"))
}

fn row_field(layout: &Layout, row: &[u8], name: &str) -> Result<crate::dense_v3::FieldValue> {
    crate::dense_v3::read_field(layout, row, name).map_err(corrupt)
}

fn validate_namespaces(source: &SourceView, metadata: &Metadata) -> Result<()> {
    source.visit(&[], None, |key, _| {
        let Some(tag) = key.first().copied() else {
            return Err(corrupt("empty typed key"));
        };
        if !matches!(
            tag,
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
                | 0x7c
        ) {
            return Err(Error::Unsupported(format!(
                "index rebuild does not understand key tag {tag:#x}"
            )));
        }
        match tag {
            0 => {
                let header = key.len() == 3 && key[1] == 0 && key[2] <= 2;
                let layout = key.len() == 10 && key[1] == 240;
                if !header && !layout {
                    return Err(Error::Unsupported(
                        "unknown typed metadata key in tag 0".into(),
                    ));
                }
                if layout {
                    let slot = u64::from_be_bytes(key[2..].try_into().unwrap());
                    let id = slot / 3;
                    if id == 0 || id >= u64::from(metadata.header.next_layout) {
                        return Err(corrupt("layout descriptor identity/allocator"));
                    }
                }
            }
            1 | 2 => {
                if key.len() < 3 || key[1] > 2 {
                    return Err(corrupt("collection metadata replica key"));
                }
                let mut at = 2;
                let id = u32::try_from(read_ordered(key, &mut at)?).map_err(corrupt)?;
                if at != key.len() || id == 0 || id >= metadata.header.next_collection {
                    return Err(corrupt("collection metadata identity/allocator"));
                }
            }
            3 => {
                if key.len() < 3 || key[1] > 2 {
                    return Err(corrupt("index descriptor replica key"));
                }
                let mut at = 2;
                let id = IndexId(read_ordered(key, &mut at)?);
                if at != key.len() || !metadata.indexes.iter().any(|index| index.id == id) {
                    return Err(corrupt("index descriptor outside the preserved catalog"));
                }
            }
            6 | 7 | 0x12 | 0x71 | 0x72 if metadata.graph_header.is_none() => {
                return Err(corrupt("graph namespace exists without graph feature"));
            }
            6 => {
                if key.len() != 2 || key[1] > 2 {
                    return Err(corrupt("graph header replica key"));
                }
            }
            7 => {
                if key.len() < 4 || key[1] > 1 || key[2] > 2 {
                    return Err(corrupt("graph name descriptor key"));
                }
                let mut at = 3;
                let id = read_ordered(key, &mut at)?;
                let header = metadata.graph_header.as_ref().unwrap().0;
                let next = if key[1] == 0 {
                    header.next_type
                } else {
                    header.next_context
                };
                if at != key.len() || id == 0 || id >= next {
                    return Err(corrupt("graph name descriptor identity/allocator"));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(())
}

fn copy_primary_rows(
    source: &SourceView,
    destination: &mut Destination,
    metadata: &Metadata,
) -> Result<u64> {
    let mut rows = 0u64;
    source.visit(&[0x40], Some(&[0x41]), |key, row| {
        let id = row_id(key)?;
        let (_, next) = metadata
            .catalogs
            .iter()
            .find(|(catalog, _)| catalog.id == id.collection)
            .ok_or_else(|| corrupt("entity belongs to an unknown collection"))?;
        if id.sequence >= *next {
            return Err(corrupt("entity identity exceeds the preserved allocator"));
        }
        let layout = layout_for(metadata, layout_id(row)?)?;
        for (ordinal, (name, kind)) in layout.fields.iter().enumerate() {
            let value = row_field(layout, row, name)?;
            match kind {
                Kind::Vector(dimension) => {
                    if let crate::dense_v3::FieldValue::Vector {
                        ordinal: found,
                        dimension: found_dimension,
                    } = value
                    {
                        if found != ordinal || found_dimension != *dimension {
                            return Err(corrupt("vector ordinal/dimension in primary row"));
                        }
                        let key = vector_key(id, ordinal);
                        let bytes = source
                            .get(&key)?
                            .ok_or_else(|| corrupt("authoritative vector sidecar missing"))?;
                        crate::index::vector::exact::validate_vector(&bytes, *dimension)?;
                    }
                }
                Kind::Point => {
                    if let crate::dense_v3::FieldValue::Inline(value) = value {
                        let mut document = serde_json::Map::new();
                        document.insert(name.clone(), value);
                        crate::index::spatial::point::selected_point(&Value::Object(document), name)?;
                    }
                }
                _ => {}
            }
        }
        let external = match row_field(layout, row, KEY_FIELD)? {
            crate::dense_v3::FieldValue::Inline(Value::String(external)) => external,
            _ => return Err(corrupt("entity hidden external key")),
        };
        let mapping = mapping_key(id.collection, &external);
        if destination.store.get(&mapping)?.is_some() {
            return Err(corrupt("duplicate external key in authoritative rows"));
        }
        destination.put(key, row)?;
        destination.put(&mapping, &ordered(id.sequence))?;
        rows = rows
            .checked_add(1)
            .ok_or_else(|| corrupt("primary row count overflow"))?;
        Ok(())
    })?;
    Ok(rows)
}

fn copy_vector_sidecars(
    source: &SourceView,
    destination: &mut Destination,
    metadata: &Metadata,
) -> Result<u64> {
    let mut sidecars = 0u64;
    source.visit(&[0x60], Some(&[0x61]), |key, value| {
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
        let row = source
            .get(&row_key(id))?
            .ok_or_else(|| corrupt("authoritative vector sidecar has no primary row"))?;
        let layout = layout_for(metadata, layout_id(&row)?)?;
        let Some((name, Kind::Vector(dimension))) = layout.fields.get(ordinal) else {
            return Err(corrupt("authoritative vector sidecar ordinal/kind"));
        };
        if !matches!(row_field(layout,&row,name)?,crate::dense_v3::FieldValue::Vector{ordinal:found,dimension:found_dimension} if found==ordinal && found_dimension==*dimension)
        {
            return Err(corrupt("orphan authoritative vector sidecar"));
        }
        crate::index::vector::exact::validate_vector(value, *dimension)?;
        destination.put(key, value)?;
        sidecars = sidecars
            .checked_add(1)
            .ok_or_else(|| corrupt("vector sidecar count overflow"))?;
        Ok(())
    })?;
    Ok(sidecars)
}

fn copy_graph(
    source: &SourceView,
    destination: &mut Destination,
    metadata: &Metadata,
) -> Result<u64> {
    let Some((header, _)) = metadata.graph_header.as_ref() else {
        return Ok(0);
    };
    let mut edges = 0u64;
    source.visit(
        &[crate::index::graph::PRIMARY_EDGE],
        Some(&tag_end(crate::index::graph::PRIMARY_EDGE)),
        |key, value| {
            let edge = crate::index::graph::parse_edge_key(key, crate::index::graph::PRIMARY_EDGE)?;
            if edge.edge_type.0 == 0
                || edge.edge_type.0 >= header.next_type
                || edge.context.0 >= header.next_context
            {
                return Err(corrupt("graph edge name identity exceeds allocator"));
            }
            crate::index::graph::decode_properties(value)?;
            for entity in [edge.source, edge.destination] {
                if source.get(&row_key(entity))?.is_none() {
                    return Err(corrupt("graph edge endpoint is missing"));
                }
            }
            destination.put(key, value)?;
            destination.put(
                &crate::index::graph::edge_key(crate::index::graph::REVERSE_EDGE, edge),
                &[],
            )?;
            edges = edges
                .checked_add(1)
                .ok_or_else(|| corrupt("graph edge count overflow"))?;
            Ok(())
        },
    )?;
    source.visit(
        &[crate::index::graph::REVERSE_EDGE],
        Some(&tag_end(crate::index::graph::REVERSE_EDGE)),
        |key, _| {
            let edge = match crate::index::graph::parse_edge_key(key, crate::index::graph::REVERSE_EDGE)
            {
                Ok(edge) => edge,
                Err(_) => return Ok(()), // malformed derived garbage is discarded
            };
            if source
                .get(&crate::index::graph::edge_key(
                    crate::index::graph::PRIMARY_EDGE,
                    edge,
                ))?
                .is_none()
            {
                return Err(corrupt(
                    "graph reverse proves a missing authoritative primary edge",
                ));
            }
            Ok(())
        },
    )?;
    Ok(edges)
}

fn visit_reader(
    reader: &CurrentSourceReader,
    start: &[u8],
    end: Option<&[u8]>,
    mut callback: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let mut detail = None;
    let raw = reader.visit_range(start, end, |key, value| match callback(key, value) {
        Ok(()) => Ok(()),
        Err(error) => {
            detail = Some(error);
            Err(kernel::Error::Corrupt {
                page_no: 0,
                why: "rebuild comparison callback refused",
            })
        }
    });
    if let Some(error) = detail {
        return Err(error);
    }
    raw.map(|_| ()).map_err(Error::from)
}

fn compare_authoritative(source: &SourceView, destination: &CurrentSourceReader) -> Result<()> {
    for tag in [0x40, 0x60, crate::index::graph::PRIMARY_EDGE] {
        source.visit(&[tag], Some(&tag_end(tag)), |key, value| {
            if destination.get(key).map_err(Error::from)?.as_deref() != Some(value) {
                return Err(corrupt(
                    "destination changed or omitted an authoritative key/value",
                ));
            }
            Ok(())
        })?;
        visit_reader(destination, &[tag], Some(&tag_end(tag)), |key, value| {
            if source.get(key)?.as_deref() != Some(value) {
                return Err(corrupt("destination introduced an authoritative key/value"));
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn build_indexes(path: &Path, indexes: &[IndexInfo], limits: RebuildLimits) -> Result<()> {
    let mut database = Database::open(path, config(limits.cache_bytes))?;
    for index in indexes {
        // The same entry point a late `CREATE INDEX` uses, so a rebuilt text
        // index is packed into segments exactly as the original build packed
        // it -- byte for byte -- instead of being re-materialized as head rows.
        database.build_index_to_ready(index.id, limits.batch)?;
    }
    drop(database);
    Ok(())
}

fn validate_limits(limits: RebuildLimits) -> Result<()> {
    if limits.cache_bytes < 2 * kernel::page::PAGE_SIZE
        || !(1..=256).contains(&limits.batch)
        || limits.max_metadata == 0
        || limits.max_records == 0
        || limits.max_point_reads == 0
        || limits.max_destination_logical_bytes < 4 * kernel::page::PAGE_SIZE as u64
    {
        return Err(invalid("invalid index rebuild limits"));
    }
    Ok(())
}

/// Rebuild every derived collection/index/graph namespace into a fresh
/// destination while holding the source's existing writer lock exclusively.
///
/// This first implementation deliberately refuses BUILDING or DROPPING index
/// descriptors. Their lifecycle is authoritative, and publishing a partial
/// family as READY would be a false recovery. A later resumable rebuild may
/// preserve those states explicitly; this function never skips them.
pub fn rebuild_derived_indexes(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    limits: RebuildLimits,
) -> Result<RebuildReport> {
    validate_limits(limits)?;
    let (source_path, destination_path) = destination_path(source.as_ref(), destination.as_ref())?;
    let source_reader =
        Arc::new(CurrentSourceReader::open(&source_path, limits.source).map_err(Error::from)?);
    let source = SourceView {
        reader: source_reader,
        limits,
        records: Cell::new(0),
        points: Cell::new(0),
    };
    let metadata = collect_metadata(&source)?;
    validate_namespaces(&source, &metadata)?;

    let (control_reserve, maximum_completion_len) =
        maximum_control_bytes(&source_path, &destination_path)?;
    let available_managed = limits
        .max_destination_logical_bytes
        .checked_sub(control_reserve)
        .ok_or(Error::Kernel(kernel::Error::ResourceLimit(
            "rebuild logical-byte budget lacks control-file reserve",
        )))?;
    let preserved_managed_cap = metadata
        .header
        .limits
        .map(|policy| {
            policy
                .data_bytes
                .checked_add(policy.wal_bytes)
                .ok_or_else(|| corrupt("persisted resource cap overflow"))
        })
        .transpose()?;
    let managed_cap = preserved_managed_cap.unwrap_or(available_managed);
    if managed_cap > available_managed {
        return Err(Error::Kernel(kernel::Error::ResourceLimit(
            "persisted resource policy plus rebuild controls exceeds logical-byte budget",
        )));
    }
    if managed_cap < PageWalStore::creation_cap_headroom() {
        return Err(Error::Kernel(kernel::Error::ResourceLimit(
            "rebuild managed-byte budget lacks destination creation headroom",
        )));
    }

    let mut store = create_destination_store(
        &destination_path,
        limits.cache_bytes,
        source.reader.compact_cells(),
    )?;
    store.set_cap(managed_cap)?;
    marker(&destination_path, INCOMPLETE, INCOMPLETE_CONTENT)?;
    io::sync_directory(
        destination_path
            .parent()
            .ok_or_else(|| corrupt("rebuild destination parent disappeared"))?,
    )?;
    let mut destination = Destination {
        store,
        batch: limits.batch,
        pending: 0,
    };
    seed_metadata(&mut destination, &metadata)?;
    let primary_rows = copy_primary_rows(&source, &mut destination, &metadata)?;
    let vector_sidecars = copy_vector_sidecars(&source, &mut destination, &metadata)?;
    let primary_edges = copy_graph(&source, &mut destination, &metadata)?;
    destination.finish()?;
    drop(destination);

    let build_rows = primary_rows
        .checked_add(primary_rows.div_ceil(limits.batch as u64))
        .and_then(|rows| rows.checked_add(1))
        .and_then(|rows| rows.checked_mul(metadata.indexes.len() as u64))
        .ok_or_else(|| corrupt("index rebuild build-work estimate overflow"))?;
    source.charge_records(build_rows)?;
    build_indexes(&destination_path, &metadata.indexes, limits)?;

    let destination_reader =
        CurrentSourceReader::open(&destination_path, limits.source).map_err(Error::from)?;
    compare_authoritative(&source, &destination_reader)?;
    drop(destination_reader);

    let mut verification_limits = verification::VerificationLimits::default();
    verification_limits.source = limits.source;
    verification_limits.max_rows = limits.max_records;
    verification_limits.max_point_reads = limits.max_point_reads;
    verification_limits.max_metadata = limits.max_metadata;
    verification_limits.max_issues = limits.max_records;
    let verified =
        verification::verify_indexed_source(&destination_path, verification_limits, |_| {})?;
    if !verified.complete || !verified.clean {
        return Err(corrupt(
            "independent verification rejected the rebuilt destination",
        ));
    }
    source.reader.recheck_source().map_err(Error::from)?;

    let provisional = RebuildReport {
        source: source_path.clone(),
        destination: destination_path.clone(),
        collections: metadata.catalogs.len(),
        layouts: metadata.layouts.len(),
        indexes: metadata.indexes.len(),
        primary_rows,
        vector_sidecars,
        primary_edges,
        source_records_visited: source.records.get(),
        source_point_reads: source.points.get(),
        destination_logical_bytes: directory_bytes(&destination_path)?,
    };
    let complete = completion_bytes(
        &provisional.source,
        &provisional.destination,
        CompletionCounts {
            collections: provisional.collections as u64,
            layouts: provisional.layouts as u64,
            indexes: provisional.indexes as u64,
            primary_rows: provisional.primary_rows,
            vector_sidecars: provisional.vector_sidecars,
            primary_edges: provisional.primary_edges,
            destination_logical_bytes_before_completion: provisional.destination_logical_bytes,
        },
    )?;
    if complete.len() > maximum_completion_len {
        return Err(corrupt("rebuild completion exceeded its preflight reserve"));
    }
    marker(&destination_path, COMPLETE_PENDING, &complete)?;
    let peak_logical_bytes = directory_bytes(&destination_path)?;
    if peak_logical_bytes > limits.max_destination_logical_bytes {
        return Err(corrupt("rebuild preflight byte-budget invariant violated"));
    }
    let incomplete_bytes = fs::metadata(destination_path.join(INCOMPLETE))
        .map_err(kernel::Error::from)?
        .len();
    let destination_logical_bytes = peak_logical_bytes
        .checked_sub(incomplete_bytes)
        .ok_or_else(|| corrupt("rebuild completion byte accounting underflow"))?;
    fs::remove_file(destination_path.join(INCOMPLETE)).map_err(kernel::Error::from)?;
    io::sync_directory(&destination_path)?;
    io::sync_directory(
        destination_path
            .parent()
            .ok_or_else(|| corrupt("rebuild destination parent disappeared"))?,
    )?;
    fs::rename(
        destination_path.join(COMPLETE_PENDING),
        destination_path.join(COMPLETE),
    )
    .map_err(kernel::Error::from)?;
    io::sync_directory(&destination_path)?;
    // `destination_logical_bytes` was computed before publication from the
    // exact peak regular-file lengths minus the incomplete marker. Rename
    // preserves length, so no fallible byte-budget or accounting check follows
    // publication; the directory sync above is the final durability boundary.
    Ok(RebuildReport {
        destination_logical_bytes,
        ..provisional
    })
}
