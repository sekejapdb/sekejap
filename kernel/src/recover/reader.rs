//! Read-only forensic primitives. These establish byte integrity, never current
//! membership. The owner above the kernel decides keyspaces and row encodings.
use crate::{
    io::{open_recovery_source, FileIo},
    page::{PageKind, PageRef, PAGE_SIZE},
    Error, Result,
};
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct LeafCandidate<'a> {
    pub page_no: u32,
    pub generation: u64,
    pub slot: usize,
    pub key: &'a [u8],
    pub stored_value: &'a [u8],
    pub overflow: bool,
}
pub enum LeafEvent<'a> {
    Record(LeafCandidate<'a>),
    DamagedPage { page_no: u32 },
    MalformedCell { page_no: u32, slot: usize },
}
#[derive(Debug, Default)]
pub struct LeafScanReport {
    pub pages: u64,
    pub damaged_pages: u64,
    pub malformed_cells: u64,
    pub truncated_tail_bytes: u64,
}
pub struct CandidateReader {
    file: Box<dyn FileIo>,
}
impl CandidateReader {
    /// A read-only logical file supplied by a format owner (for committed WAL overlays).
    pub fn from_file(file: Box<dyn FileIo>) -> Self { Self { file } }
    pub fn open(source: &Path) -> Result<Self> {
        Ok(Self {
            file: open_recovery_source(&source.join("data"))?,
        })
    }
    /// Stream independently verified leaf cells. A single page buffer is reused;
    /// callback ownership prevents retaining borrowed data across the next read.
    pub fn scan<E: From<Error>>(
        &self,
        tree_id: u16,
        mut visitor: impl FnMut(LeafEvent<'_>) -> std::result::Result<(), E>,
    ) -> std::result::Result<LeafScanReport, E> {
        let len = self.file.len()?;
        let pages = len / PAGE_SIZE as u64;
        if pages > u32::MAX as u64 {
            return Err(Error::TooLarge.into());
        }
        let mut result = LeafScanReport {
            truncated_tail_bytes: len % PAGE_SIZE as u64,
            ..Default::default()
        };
        let mut bytes = [0; PAGE_SIZE];
        for no in 0..pages as u32 {
            self.file
                .read_at(&mut bytes, no as u64 * PAGE_SIZE as u64)?;
            result.pages += 1;
            let page = match PageRef::open(&bytes, no) {
                Ok(p) => p,
                Err(_) => {
                    result.damaged_pages += 1;
                    visitor(LeafEvent::DamagedPage { page_no: no })?;
                    continue;
                }
            };
            if page.kind() != PageKind::Leaf || page.tree_id() != tree_id {
                continue;
            }
            for slot in 0..page.nentries() {
                match super::decode_leaf_record(page.slot(slot), no) {
                    Ok((key, stored_value, overflow)) => {
                        visitor(LeafEvent::Record(LeafCandidate {
                            page_no: no,
                            generation: page.lsn(),
                            slot,
                            key,
                            stored_value,
                            overflow,
                        }))?
                    }
                    Err(_) => {
                        result.malformed_cells += 1;
                        visitor(LeafEvent::MalformedCell { page_no: no, slot })?;
                    }
                }
            }
        }
        Ok(result)
    }
    /// Materialize at most the caller's explicit encoded-value allowance.
    /// Large values are checked before allocating. No partial overflow is returned.
    pub fn read_value(&self, record: LeafCandidate<'_>, max_bytes: usize) -> Result<Vec<u8>> {
        if !record.overflow {
            if record.stored_value.len() > max_bytes {
                return Err(Error::TooLarge);
            }
            return Ok(record.stored_value.to_vec());
        }
        if record.stored_value.len() != 12 {
            return Err(bad(record.page_no, "overflow marker size"));
        }
        let total = u32::from_le_bytes(record.stored_value[..4].try_into().unwrap()) as usize;
        if total > max_bytes {
            return Err(Error::TooLarge);
        }
        let mut value = Vec::with_capacity(total);
        visit_overflow(&*self.file, record.stored_value, |chunk| {
            value.extend_from_slice(chunk)
        })?;
        Ok(value)
    }
}
fn bad(page_no: u32, why: &'static str) -> Error {
    Error::Corrupt { page_no, why }
}

pub(super) fn visit_overflow(
    file: &dyn FileIo,
    marker: &[u8],
    mut visit: impl FnMut(&[u8]),
) -> Result<()> {
    use crate::btree::{OV_CAP, OV_DATA, OV_NEXT, OV_USED};
    if marker.len() != 12 {
        return Err(bad(0, "overflow marker size"));
    }
    let total = u32::from_le_bytes(marker[..4].try_into().unwrap()) as usize;
    let mut no = u32::from_le_bytes(marker[4..8].try_into().unwrap());
    let want = u32::from_le_bytes(marker[8..].try_into().unwrap());
    let bound = total.div_ceil(OV_CAP).max(1);
    let pages = file.len()? / PAGE_SIZE as u64;
    let (mut seen, mut bytes, mut crc) = (0usize, 0usize, 0u32);
    let mut buf = [0u8; PAGE_SIZE];
    while no != 0 {
        seen += 1;
        if seen > bound || no < 2 || no as u64 >= pages {
            return Err(bad(no, "overflow chain bounds"));
        }
        file.read_at(&mut buf, no as u64 * PAGE_SIZE as u64)?;
        let p = PageRef::open(&buf, no)?;
        if p.kind() != PageKind::Overflow || p.tree_id() != 0 {
            return Err(bad(no, "overflow page kind"));
        }
        let used = u16::from_le_bytes(buf[OV_USED..OV_USED + 2].try_into().unwrap()) as usize;
        if used > OV_CAP || bytes.checked_add(used).is_none_or(|n| n > total) {
            return Err(bad(no, "overflow length bounds"));
        }
        bytes += used;
        let chunk = &buf[OV_DATA..OV_DATA + used];
        crc = crc32c::crc32c_append(crc, chunk);
        visit(chunk);
        no = u32::from_le_bytes(buf[OV_NEXT..OV_NEXT + 4].try_into().unwrap());
    }
    if seen != bound || bytes != total || crc != want {
        return Err(bad(0, "overflow whole-value checksum or length"));
    }
    Ok(())
}
