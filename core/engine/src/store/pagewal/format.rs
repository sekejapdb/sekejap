//! Prototype v2 recovery envelope. Record and B-tree cell encodings are unchanged.
use super::*;

pub(super) const HEADER_BYTES: usize = 56;
pub(super) const COMPACT_CELLS: u64 = 1;

/// Every required-feature bit this release implements. A database declaring
/// only these opens, reads and writes in EVERY build of this release, whatever
/// cargo features that build was compiled with (Law 8). A bit outside this set
/// is a database this binary does not understand, and is refused before any
/// byte of it is changed.
pub(super) const SUPPORTED_FEATURES: u64 = COMPACT_CELLS;

/// What a build stamps into a database it CREATES. This is the only thing the
/// `compact-cells` cargo feature still decides; it does not decide what this
/// build can open. `set_create_features` lets a caller (and the compatibility
/// tests) create a database of the other supported family without a second
/// build of the binary.
const DEFAULT_CREATE_FEATURES: u64 = if cfg!(feature = "compact-cells") { COMPACT_CELLS } else { 0 };
static CREATE_FEATURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(DEFAULT_CREATE_FEATURES);

pub(super) fn create_features() -> u64 {
    CREATE_FEATURES.load(std::sync::atomic::Ordering::Relaxed)
}
pub(super) fn set_create_features(features: u64) -> Result<u64> {
    if features & !SUPPORTED_FEATURES != 0 {
        return Err(bad("cannot create a database with an unimplemented feature"));
    }
    Ok(CREATE_FEATURES.swap(features, std::sync::atomic::Ordering::Relaxed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Header {
    pub root: u32,
    pub free: u32,
    pub cap: u64,
    pub identity: [u8; 16],
    pub tx: u64,
    pub features: u64,
}
impl Header {
    pub fn bytes(self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&self.root.to_le_bytes());
        out.extend_from_slice(&self.free.to_le_bytes());
        out.extend_from_slice(&self.cap.to_le_bytes());
        out.extend_from_slice(&self.identity);
        out.extend_from_slice(&self.tx.to_le_bytes());
        out.extend_from_slice(&self.features.to_le_bytes());
        out
    }
    pub fn decode(bytes: &[u8], no: u32) -> Result<Self> {
        let page = PageRef::open(bytes, no)?;
        if page.kind() != PageKind::Meta || page.tree_id() != 0 || page.nentries() != 1 {
            return Err(bad("checkpoint metadata shape"));
        }
        Self::decode_slot(page.slot(0))
    }
    pub fn decode_slot(b: &[u8]) -> Result<Self> {
        if b.len() != HEADER_BYTES || &b[..8] != MAGIC {
            return Err(bad("unsupported page-WAL header format"));
        }
        let features = u64at(b, 48);
        // Judged against what this RELEASE implements, never against what this
        // BUILD would have chosen for a new database: a compact-cells database
        // must open in a plain build and the other way round.
        if features & !SUPPORTED_FEATURES != 0 {
            return Err(bad("unsupported required page-WAL features"));
        }
        let identity: [u8; 16] = b[24..40].try_into().unwrap();
        if identity == [0; 16] { return Err(bad("empty database identity")); }
        Ok(Self { root: u32at(b, 8), free: u32at(b, 12), cap: u64at(b, 16),
            identity, tx: u64at(b, 40), features })
    }
    pub fn page(self, no: u32) -> Result<[u8; PAGE]> {
        let mut bytes = [0; PAGE];
        let mut p = PageMut::init(&mut bytes, PageKind::Meta, 0, no);
        p.insert_slot(0, &self.bytes())?;
        p.finalise(0);
        kernel::page::seal(&mut bytes, 1);
        Ok(bytes)
    }
    pub fn validate_extent(self, pages: u32) -> Result<()> {
        if self.root < 2 || self.root >= pages { return Err(bad("pilot root extent")); }
        if self.free != 0 && (self.free < 2 || self.free >= pages) {
            return Err(bad("free head outside file"));
        }
        Ok(())
    }
}

// A damaged copy may fall back to its independently checksummed sibling. An
// intact unsupported copy must refuse, never be hidden by the other copy.
//
// This is also where the DISK-FORMAT STAMP is judged. `disk_header` is the
// first thing every open does (`Pager::inspect_bounded`), so both checkpoint
// metadata copies are checked before any other page of the file is read: a
// file that is not sekejap disk format v2 is named and refused without a
// root, a catalog or a WAL frame ever being touched. An intact copy claiming
// anything but 2 refuses here even when its sibling says 2, for the same
// reason an intact unsupported feature set does.
pub(super) fn disk_header(data: &dyn FileIo) -> Result<Option<Header>> {
    if data.len()? < 2 * PAGE as u64 { return Err(bad("missing checkpoint metadata pages")); }
    let mut selected: Option<Header> = None;
    for no in 0..2 {
        let mut bytes = [0; PAGE];
        data.read_at(&mut bytes, no as u64 * PAGE as u64)?;
        if kernel::page::checksum(&bytes) != u32at(&bytes, 36) { continue; }
        let stamp = kernel::page::format_version(&bytes);
        if stamp != kernel::FORMAT_VERSION {
            return Err(kernel::Error::UnsupportedFormat { found: stamp });
        }
        let header = Header::decode(&bytes, no)?;
        if let Some(old) = selected {
            if old.identity != header.identity || old.features != header.features {
                return Err(bad("checkpoint metadata identity/features disagree"));
            }
            if old.tx == header.tx && old != header {
                return Err(bad("checkpoint metadata histories disagree"));
            }
            if old.tx > header.tx { continue; }
        }
        selected = Some(header);
    }
    Ok(selected)
}

// Replace a damaged or older copy first. Once that replacement is durable,
// the other copy can be overwritten without ever leaving zero valid headers.
pub(super) fn metadata_write_order(data: &dyn FileIo) -> Result<[u32; 2]> {
    let mut copies = [None; 2];
    for no in 0..2 {
        let mut bytes = [0; PAGE];
        data.read_at(&mut bytes, no as u64 * PAGE as u64)?;
        if kernel::page::checksum(&bytes) == u32at(&bytes, 36) {
            copies[no] = Some(Header::decode(&bytes, no as u32)?);
        }
    }
    Ok(match (copies[0], copies[1]) {
        (None, Some(_)) => [0, 1],
        (Some(_), None) => [1, 0],
        (Some(a), Some(b)) if a.tx > b.tx => [1, 0],
        _ => [0, 1],
    })
}
