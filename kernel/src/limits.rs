//! Explicit limits for the constrained entry writer. Stored in both checked
//! metadata publications. Ordinary stores keep their existing WAL commit mode.
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    pub data_bytes: u64,
    pub wal_bytes: u64,
    /// Maximum retired pages AND recycled pages tracked in one epoch.
    pub tracked_pages: u32,
    pub readers: u32,
    pub record_bytes: u32,
    /// Space kept unavailable to normal writes for an operator's recovery work.
    /// Crash restart itself needs no new data pages in this mode.
    pub recovery_bytes: u64,
}

impl ResourceLimits {
    pub fn validate(self) -> Result<Self> {
        if self.data_bytes < 3 * 4096
            || self.data_bytes % 4096 != 0
            || self.data_bytes / 4096 > u32::MAX as u64
            || self.wal_bytes < 4096
            || self.tracked_pages == 0
            || self.readers == 0
            || self.readers > 4096
            || self.record_bytes == 0
            || self.record_bytes as u64 + 4096 > self.wal_bytes
        {
            return Err(Error::ResourceLimit("invalid resource limits"));
        }
        self.total_bytes()?;
        Ok(self)
    }
    pub fn freelist_bytes(self) -> u64 {
        28 + 24 * self.tracked_pages as u64
    }
    /// Maximum managed logical bytes, including both freelist candidates,
    /// fixed reader slots and unused recovery allowance. Filesystem metadata,
    /// allocation rounding, unrelated files and recovery exports are separate.
    pub fn total_bytes(self) -> Result<u64> {
        self.data_bytes
            .checked_add(self.wal_bytes)
            .and_then(|n| n.checked_add(2 * self.freelist_bytes()))
            .and_then(|n| n.checked_add(48 * self.readers as u64))
            .and_then(|n| n.checked_add(self.recovery_bytes))
            .ok_or(Error::ResourceLimit("total byte limit overflow"))
    }
    pub(crate) fn encode(self) -> Vec<u8> {
        let mut b = b"E4LIMIT1".to_vec();
        for n in [
            self.data_bytes,
            self.wal_bytes,
            self.tracked_pages as u64,
            self.readers as u64,
            self.record_bytes as u64,
            self.recovery_bytes,
        ] {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 56 || &b[..8] != b"E4LIMIT1" {
            return Err(Error::ResourceLimit(
                "damaged resource policy; use source-preserving recovery",
            ));
        }
        let n = |i| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let small =
            |i| u32::try_from(n(i)).map_err(|_| Error::ResourceLimit("resource policy overflow"));
        Self {
            data_bytes: n(8),
            wal_bytes: n(16),
            tracked_pages: small(24)?,
            readers: small(32)?,
            record_bytes: small(40)?,
            recovery_bytes: n(48),
        }
        .validate()
    }
}

/// Read only checked metadata, with a fixed small cache. Never changes a file.
pub(crate) fn read(dir: &std::path::Path) -> Result<Option<ResourceLimits>> {
    if !dir.join("data").exists() {
        return Ok(None);
    }
    let file = crate::io::open_file_readonly(&dir.join("data"))?;
    let budget = std::sync::Arc::new(crate::budget::MemoryBudget::new(65536));
    let pool = crate::pool::BufferPool::new(file.into(), budget, 16)?;
    crate::meta::Meta::read_limits(&pool)
}
