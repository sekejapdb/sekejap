//! 2.0.2 The Anchor on disk: today's header keys `[0,0,copy]` and framing,
//! magic `E4COLL3`. A released 0.18 binary meets that intact header and
//! refuses the file as newer before it writes anything.

use super::carrier::Anchor;
use crate::collections::{packet, replicas, unpack, Error};
use super::register::{Trees, TreesMut};

type Result<T> = std::result::Result<T, Error>;

pub(crate) const ANCHOR_MAGIC: &[u8; 8] = b"E4COLL3\0";

/// Write the Anchor to all three header copies, in the caller's transaction.
pub(crate) fn write_anchor(store: &mut impl TreesMut, anchor: &Anchor) -> Result<()> {
    let framed = packet(ANCHOR_MAGIC, &anchor.encode()?)?;
    for copy in 0..3 {
        store.put(&[0, 0, copy], &framed)?;
    }
    Ok(())
}

/// Read the Anchor by the header's replica rule: a damaged copy loses to
/// intact ones, intact copies must agree, an intact copy this build cannot
/// read is refused by name rather than hidden.
pub(crate) fn read_anchor(store: &(impl Trees + ?Sized)) -> Result<Anchor> {
    replicas(
        |k| store.get(k),
        |copy| vec![0, 0, copy],
        |b| Anchor::decode(unpack(b, ANCHOR_MAGIC)?),
    )
}
