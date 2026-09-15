//! Prototype v2 recovery envelope. Record and B-tree cell encodings are unchanged.
use super::*;

pub(super) const HEADER_BYTES: usize = 56;
const COMPACT_CELLS: u64 = 1;
pub(super) const WRITE_FEATURES: u64 = if cfg!(feature = "compact-cells") { COMPACT_CELLS } else { 0 };

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
        if features & !WRITE_FEATURES != 0 {
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
pub(super) fn disk_header(data: &dyn FileIo) -> Result<Option<Header>> {
    if data.len()? < 2 * PAGE as u64 { return Err(bad("missing checkpoint metadata pages")); }
    let mut selected: Option<Header> = None;
    for no in 0..2 {
        let mut bytes = [0; PAGE];
        data.read_at(&mut bytes, no as u64 * PAGE as u64)?;
        if kernel::page::checksum(&bytes) != u32at(&bytes, 36) { continue; }
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
