//! Packed text posting segments (tag `0x7A`, feature bit `0x40`).
//!
//! The head representation (tag `0x75`) is one B-tree entry per
//! `(term, document)`: blind, searchable the instant it lands, and the right
//! shape for a live write, which only ever touches the terms of the one
//! document it changed. It is the wrong shape for a late build. Ordinary prose
//! has about five distinct terms per document, so a corpus of N documents
//! writes 5N entries, each with its own key -- the term repeated in full -- its
//! own slot and its own descent. FTS5 does not do that: it accumulates a term's
//! postings in memory and writes ONE packed blob per term per segment.
//!
//! This is that second tier. A late build emits, per term, one value holding
//! every document that term occurs in, delta-encoded. The 5N entries become
//! one entry per distinct term, and the term bytes are stored once instead of
//! once per document.
//!
//! Both tiers coexist and are read as one stream (`text_indexes::TermPostings`):
//! segments hold what the build folded, head rows hold everything written
//! since, and a head row always overrides a segment for the same document --
//! including the `tf = 0` tombstone that a delete or an update writes when the
//! old posting is inside a segment it cannot cheaply rewrite.
//!
//! Law 5: every field is bounds-checked before it is believed, the postings
//! must ascend strictly, the entry count and the last sequence are both stored
//! and both re-derived on decode, and the pager has already checksummed the
//! page the value came from. Blast radius of one bad byte: the one term's one
//! segment refuses; the rest of the index reads.
use super::*;

/// Key tag. `[0x7A][ordered index id][term UTF-8][NUL][ordered last sequence]`.
pub(in crate::collections) const SEGMENT: u8 = 0x7A;
/// Collection-header feature bit. Monotone after the first segment is written;
/// an engine that does not know this bit refuses the file before it is opened.
pub(in crate::collections) const SEGMENT_FEATURE: u64 = 0x40;
/// Value format byte. Only 1 exists.
pub(in crate::collections) const FORMAT: u8 = 1;

/// Largest packed value this writer emits.
///
/// A leaf record is `4 + key + value` bytes and must fit `MAX_RECORD_LEN`
/// (`PAGE_SIZE 4096 - HEADER_LEN 40 - SLOT_LEN 4 = 4052`); anything larger
/// spills to an overflow chain. The longest possible segment key is 1 tag +
/// 9 ordered index id + 128 term bytes + 1 NUL + 9 ordered sequence = 148, so
/// `4 + 148 + 3600 = 3752 < 4052`: every segment this writer emits stays
/// inline, one page, no chain. A term with more postings than that continues
/// in the next segment key rather than spilling.
pub(in crate::collections) const MAX_SEGMENT_BYTES: usize = 3600;
/// `FORMAT` plus two 10-byte worst-case varints.
const MAX_HEADER_BYTES: usize = 1 + 10 + 10;

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn varint_len(mut value: u64) -> usize {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// Read one LEB128 varint. Refuses a truncated tail, a run longer than ten
/// bytes and any non-minimal encoding (a continuation byte that adds nothing).
fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*at)
            .ok_or_else(|| corrupt("text segment varint is truncated"))?;
        *at += 1;
        if shift >= 64 || (shift == 63 && byte > 1) {
            return Err(corrupt("text segment varint overflows 64 bits"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if byte == 0 && shift != 0 {
                return Err(corrupt("text segment varint is not minimal"));
            }
            return Ok(value);
        }
        shift += 7;
    }
}

/// Decode a packed segment value into ascending `(sequence, term frequency)`.
///
/// Every structural promise is re-derived, not trusted: the declared count is
/// the number of entries actually decoded, the declared last sequence is the
/// last one decoded, the value ends exactly where the last entry ends, deltas
/// are strictly positive so sequences ascend, and no frequency is zero.
pub(in crate::collections) fn decode(value: &[u8]) -> Result<Vec<(u64, u32)>> {
    let mut out = Vec::new();
    decode_into(value, &mut out)?;
    Ok(out)
}

pub(in crate::collections) fn decode_into(value: &[u8], out: &mut Vec<(u64, u32)>) -> Result<()> {
    out.clear();
    let mut at = 0usize;
    match value.first() {
        Some(&FORMAT) => at += 1,
        Some(_) => return Err(corrupt("unknown text segment format")),
        None => return Err(corrupt("empty text segment")),
    }
    let count = read_varint(value, &mut at)?;
    let last = read_varint(value, &mut at)?;
    if count == 0 || last == 0 {
        return Err(corrupt("empty text segment header"));
    }
    let count = usize::try_from(count).map_err(|_| corrupt("text segment count"))?;
    // One posting needs at least two bytes, so a count larger than the bytes
    // that remain is refused before a single allocation is sized from it.
    if count > value.len() - at {
        return Err(corrupt("text segment count exceeds its bytes"));
    }
    out.reserve(count);
    let mut sequence = 0u64;
    for _ in 0..count {
        let delta = read_varint(value, &mut at)?;
        if delta == 0 {
            return Err(corrupt("text segment sequences do not ascend"));
        }
        sequence = sequence
            .checked_add(delta)
            .ok_or_else(|| corrupt("text segment sequence overflow"))?;
        let frequency = read_varint(value, &mut at)?;
        if frequency == 0 || frequency > u64::from(u32::MAX) {
            return Err(corrupt("text segment term frequency bounds"));
        }
        out.push((sequence, frequency as u32));
    }
    if at != value.len() {
        return Err(corrupt("text segment has trailing bytes"));
    }
    if sequence != last {
        return Err(corrupt("text segment last sequence disagrees with its postings"));
    }
    Ok(())
}

/// Pack one term's postings into size-capped values, in ascending order.
///
/// Sacrifice (Law 1): one segment's worth of encoded bytes is resident while
/// it is being filled -- at most `MAX_SEGMENT_BYTES`, never the term's whole
/// posting list and never the corpus.
pub(in crate::collections) struct Packer {
    body: Vec<u8>,
    count: u64,
    previous: u64,
    last: u64,
}

impl Default for Packer {
    fn default() -> Self {
        Self::new()
    }
}

impl Packer {
    pub(in crate::collections) fn new() -> Self {
        Self {
            body: Vec::with_capacity(MAX_SEGMENT_BYTES),
            count: 0,
            previous: 0,
            last: 0,
        }
    }

    #[allow(dead_code)]
    pub(in crate::collections) fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Encoded bytes this packer is holding for the segment it is filling.
    ///
    /// The late build keeps one packer per term alive across the whole scan,
    /// so it needs a cheap, exact answer to "how much RAM are the unfinished
    /// segments costing" -- the difference across one `push` is what that
    /// posting added, or, when the push completed a segment, what it gave
    /// back.
    pub(in crate::collections) fn held_bytes(&self) -> usize {
        self.body.len()
    }

    /// Add one posting. Returns a finished `(last sequence, value)` when this
    /// posting did not fit the segment being filled; that segment is complete
    /// and the posting has started the next one.
    pub(in crate::collections) fn push(
        &mut self,
        sequence: u64,
        frequency: u32,
    ) -> Result<Option<(u64, Vec<u8>)>> {
        if sequence == 0 || frequency == 0 {
            return Err(corrupt("text segment posting bounds"));
        }
        if sequence <= self.previous && self.count != 0 {
            return Err(corrupt("text segment postings must ascend"));
        }
        let delta = sequence - self.previous;
        let width = varint_len(delta) + varint_len(u64::from(frequency));
        let mut finished = None;
        if self.count != 0 && MAX_HEADER_BYTES + self.body.len() + width > MAX_SEGMENT_BYTES {
            finished = Some(self.take()?);
        }
        let delta = sequence - self.previous;
        put_varint(&mut self.body, delta);
        put_varint(&mut self.body, u64::from(frequency));
        self.previous = sequence;
        self.last = sequence;
        self.count += 1;
        Ok(finished)
    }

    fn take(&mut self) -> Result<(u64, Vec<u8>)> {
        if self.count == 0 {
            return Err(corrupt("empty text segment"));
        }
        let mut value = Vec::with_capacity(MAX_HEADER_BYTES + self.body.len());
        value.push(FORMAT);
        put_varint(&mut value, self.count);
        put_varint(&mut value, self.last);
        value.extend_from_slice(&self.body);
        let last = self.last;
        self.body.clear();
        self.count = 0;
        self.previous = 0;
        self.last = 0;
        Ok((last, value))
    }

    /// Finish the segment being filled, if any.
    pub(in crate::collections) fn finish(&mut self) -> Result<Option<(u64, Vec<u8>)>> {
        if self.count == 0 {
            return Ok(None);
        }
        self.take().map(Some)
    }
}

/// `[0x7A][ordered index][term][NUL]` -- every segment of one term.
pub(in crate::collections) fn segment_prefix(id: IndexId, term: &str) -> Vec<u8> {
    let mut key = index_prefix(SEGMENT, id);
    key.extend(term.as_bytes());
    key.push(0);
    key
}

/// The segment key is suffixed with the LAST sequence it holds, not an
/// ordinal. Segments of one term are disjoint ascending ranges, so both orders
/// are the same; keying by the last sequence additionally makes "which segment
/// could hold document S" one seek -- `range(prefix ++ ordered(S))` lands on
/// the first segment whose last sequence is at least S. The filtered query
/// path probes exactly one document at a time and would otherwise have to walk
/// a term's segments from the start.
pub(in crate::collections) fn segment_key(id: IndexId, term: &str, last: u64) -> Vec<u8> {
    let mut key = segment_prefix(id, term);
    key.extend(ordered(last));
    key
}

// ---------------------------------------------------------------------------
// Packed document norms (tag `0x7B`, same feature bit `0x40`).
// ---------------------------------------------------------------------------
//
// The norm row `0x76` is one B-tree entry per document holding one `u32`: the
// token count BM25 divides by. At the head tier that is right -- a live write
// changes exactly one document's length. For a late build it is the largest
// surviving per-row write: 50,000 documents meant 50,000 entries, each with
// its own key, its own slot and its own descent, to carry 50,000 small
// integers that arrive in ascending order and never change again.
//
// So the late build packs them the same way it packs postings: one entry per
// 256 consecutive sequences. Key `[0x7B][ordered index id][ordered seq >> 8]`,
// value a 256-bit presence bitmap followed by one varint per present document.
// Sequences are not dense -- a deleted row leaves a hole and the build skips
// documents whose field is missing or null -- so the bitmap is what makes a
// hole cost one bit instead of a byte, and it keeps `length = 0` (a present
// document with no tokens, which the design admits) distinct from "no
// document here".
//
// Measured on the 50K benchmark shape, where every length is under 128 and so
// one varint byte: a full block is 1 + 32 + 256 = 289 bytes against
// 1 + 32 + 4*256 = 1057 for a fixed-width u32 table, 3.7x smaller. The varint
// table is what this writes; `norm_block_fixed_width_bytes` exists so the test
// can state the comparison rather than assert a remembered number.
//
// A head `0x76` row always overrides the block for its document, exactly as a
// head posting overrides a segment. Under this bit `0x76` gains one new value
// shape: the EMPTY value, meaning "this document is not in the index". It is
// what a delete of a folded document writes, because the block cannot be
// rewritten cheaply and `length = 0` already means something else.

/// Key tag. `[0x7B][ordered index id][ordered (sequence >> 8)]`.
pub(in crate::collections) const NORM_BLOCK: u8 = 0x7B;
/// Documents per block. One byte of slot, so `sequence >> 8` names the block.
pub(in crate::collections) const NORM_BLOCK_SPAN: u64 = 256;
/// Presence bitmap width in bytes: one bit per slot.
const NORM_BITMAP_BYTES: usize = (NORM_BLOCK_SPAN as usize) / 8;

pub(in crate::collections) fn norm_block_of(sequence: u64) -> u64 {
    sequence / NORM_BLOCK_SPAN
}

pub(in crate::collections) fn norm_slot_of(sequence: u64) -> usize {
    (sequence % NORM_BLOCK_SPAN) as usize
}

pub(in crate::collections) fn norm_block_key(id: IndexId, block: u64) -> Vec<u8> {
    let mut key = index_prefix(NORM_BLOCK, id);
    key.extend(ordered(block));
    key
}

/// What the same block would cost with a fixed-width `u32` table instead of
/// varints, for the same present documents. Used by the format test to state
/// the choice in bytes rather than in prose.
#[cfg(test)]
pub(in crate::collections) fn norm_block_fixed_width_bytes(present: usize) -> usize {
    1 + NORM_BITMAP_BYTES + 4 * present
}

/// Decode one packed norm block into `(slot, length)` pairs, ascending.
///
/// Law 5: the bitmap must be wholly present, its population count must equal
/// the number of varints that actually decode, the value must end exactly
/// where the last varint ends, at least one slot must be set (an empty block
/// is never written, so one is corruption), and every length must fit `u32`.
pub(in crate::collections) fn decode_norm_block(
    value: &[u8],
    out: &mut Vec<(usize, u32)>,
) -> Result<()> {
    out.clear();
    match value.first() {
        Some(&FORMAT) => {}
        Some(_) => return Err(corrupt("unknown text norm block format")),
        None => return Err(corrupt("empty text norm block")),
    }
    let bitmap = value
        .get(1..1 + NORM_BITMAP_BYTES)
        .ok_or_else(|| corrupt("text norm block bitmap is truncated"))?;
    let present: usize = bitmap.iter().map(|byte| byte.count_ones() as usize).sum();
    if present == 0 {
        return Err(corrupt("text norm block presents no document"));
    }
    // One length needs at least one byte, so a bitmap claiming more documents
    // than the bytes that remain is refused before anything is sized from it.
    let mut at = 1 + NORM_BITMAP_BYTES;
    if present > value.len() - at {
        return Err(corrupt("text norm block presence exceeds its bytes"));
    }
    out.reserve(present);
    for slot in 0..NORM_BLOCK_SPAN as usize {
        if bitmap[slot / 8] & (1 << (slot % 8)) == 0 {
            continue;
        }
        let length = read_varint(value, &mut at)?;
        if length > u64::from(u32::MAX) {
            return Err(corrupt("text norm block length bounds"));
        }
        out.push((slot, length as u32));
    }
    if at != value.len() {
        return Err(corrupt("text norm block has trailing bytes"));
    }
    Ok(())
}

/// The one document's length this block holds, if it holds it.
///
/// The value must be the block `norm_block_of(sequence)` names: only the slot
/// is matched, and slot 1 of block 0 and slot 1 of block 1 are the same slot.
/// Both callers derive the key from `norm_block_of` immediately before the
/// read, so the pairing is never in doubt.
pub(in crate::collections) fn norm_in_block(value: &[u8], sequence: u64) -> Result<Option<u32>> {
    let mut decoded = Vec::new();
    decode_norm_block(value, &mut decoded)?;
    let slot = norm_slot_of(sequence);
    Ok(decoded
        .into_iter()
        .find(|(candidate, _)| *candidate == slot)
        .map(|(_, length)| length))
}

/// Fill blocks from strictly ascending `(sequence, length)` pairs.
///
/// Sacrifice (Law 1): one block -- at most 256 lengths and a 32-byte bitmap --
/// is resident while it fills. Not the corpus.
pub(in crate::collections) struct NormPacker {
    block: u64,
    bitmap: [u8; NORM_BITMAP_BYTES],
    body: Vec<u8>,
    previous: u64,
    count: usize,
}

impl Default for NormPacker {
    fn default() -> Self {
        Self::new()
    }
}

impl NormPacker {
    pub(in crate::collections) fn new() -> Self {
        Self {
            block: 0,
            bitmap: [0; NORM_BITMAP_BYTES],
            body: Vec::with_capacity(NORM_BLOCK_SPAN as usize),
            previous: 0,
            count: 0,
        }
    }

    /// Add one document. Returns a finished `(block, value)` when this
    /// document belongs to a later block than the one being filled.
    pub(in crate::collections) fn push(
        &mut self,
        sequence: u64,
        length: u32,
    ) -> Result<Option<(u64, Vec<u8>)>> {
        if sequence == 0 {
            return Err(corrupt("text norm block sequence bounds"));
        }
        if self.count != 0 && sequence <= self.previous {
            return Err(corrupt("text norm block sequences must ascend"));
        }
        let block = norm_block_of(sequence);
        let mut finished = None;
        if self.count != 0 && block != self.block {
            finished = Some(self.take()?);
        }
        self.block = block;
        let slot = norm_slot_of(sequence);
        self.bitmap[slot / 8] |= 1 << (slot % 8);
        put_varint(&mut self.body, u64::from(length));
        self.previous = sequence;
        self.count += 1;
        Ok(finished)
    }

    fn take(&mut self) -> Result<(u64, Vec<u8>)> {
        if self.count == 0 {
            return Err(corrupt("empty text norm block"));
        }
        let mut value = Vec::with_capacity(1 + NORM_BITMAP_BYTES + self.body.len());
        value.push(FORMAT);
        value.extend_from_slice(&self.bitmap);
        value.extend_from_slice(&self.body);
        let block = self.block;
        self.bitmap = [0; NORM_BITMAP_BYTES];
        self.body.clear();
        self.count = 0;
        Ok((block, value))
    }

    pub(in crate::collections) fn finish(&mut self) -> Result<Option<(u64, Vec<u8>)>> {
        if self.count == 0 {
            return Ok(None);
        }
        self.take().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_all(postings: &[(u64, u32)]) -> Vec<(u64, Vec<u8>)> {
        let mut packer = Packer::new();
        let mut out = Vec::new();
        for (sequence, frequency) in postings {
            if let Some(done) = packer.push(*sequence, *frequency).unwrap() {
                out.push(done);
            }
        }
        if let Some(done) = packer.finish().unwrap() {
            out.push(done);
        }
        out
    }

    #[test]
    fn a_packed_segment_round_trips_every_posting_it_was_given() {
        for postings in [
            vec![(1u64, 1u32)],
            vec![(1, 7), (2, 1), (900, 4)],
            vec![(1, 1), (u64::from(u32::MAX), u32::MAX)],
            (1..200u64).map(|n| (n * 37, (n % 9) as u32 + 1)).collect(),
        ] {
            let segments = pack_all(&postings);
            assert_eq!(segments.len(), 1, "{postings:?} should fit one segment");
            let (last, value) = &segments[0];
            assert_eq!(*last, postings.last().unwrap().0);
            assert!(value.len() <= MAX_SEGMENT_BYTES);
            assert_eq!(decode(value).unwrap(), postings);
        }
    }

    #[test]
    fn a_long_posting_list_splits_at_the_size_cap_and_loses_nothing() {
        // Wide, irregular gaps so the varints are not all one byte.
        let postings: Vec<(u64, u32)> = (1..4000u64).map(|n| (n * 1_000_003, 1 + (n % 5) as u32)).collect();
        let segments = pack_all(&postings);
        assert!(segments.len() > 1, "expected a split, got {}", segments.len());
        let mut seen = Vec::new();
        let mut previous = 0u64;
        for (last, value) in &segments {
            assert!(value.len() <= MAX_SEGMENT_BYTES, "{} bytes", value.len());
            let decoded = decode(value).unwrap();
            assert_eq!(decoded.last().unwrap().0, *last);
            assert!(decoded.first().unwrap().0 > previous, "segments must ascend");
            previous = *last;
            seen.extend(decoded);
        }
        assert_eq!(seen, postings);
    }

    #[test]
    fn a_malformed_segment_is_refused_rather_than_half_believed() {
        let (_, good) = pack_all(&[(1, 2), (5, 1), (9, 3)]).pop().unwrap();
        assert!(decode(&good).is_ok());

        let mut wrong_format = good.clone();
        wrong_format[0] = 2;
        for bad in [
            vec![],                       // empty
            wrong_format,                 // unknown format byte
            good[..good.len() - 1].to_vec(), // truncated tail
            {
                let mut v = good.clone();
                v.push(0);
                v
            }, // trailing bytes
        ] {
            assert!(matches!(decode(&bad), Err(Error::Corrupt(_))), "{bad:?}");
        }

        // A count that does not match the entries, and a last sequence that
        // does not match the entries, are both caught.
        let mut miscounted = Vec::new();
        miscounted.push(FORMAT);
        put_varint(&mut miscounted, 4);
        put_varint(&mut miscounted, 9);
        for (delta, tf) in [(1u64, 2u64), (4, 1), (4, 3)] {
            put_varint(&mut miscounted, delta);
            put_varint(&mut miscounted, tf);
        }
        assert!(matches!(decode(&miscounted), Err(Error::Corrupt(_))));

        let mut wrong_last = Vec::new();
        wrong_last.push(FORMAT);
        put_varint(&mut wrong_last, 3);
        put_varint(&mut wrong_last, 10);
        for (delta, tf) in [(1u64, 2u64), (4, 1), (4, 3)] {
            put_varint(&mut wrong_last, delta);
            put_varint(&mut wrong_last, tf);
        }
        assert!(matches!(decode(&wrong_last), Err(Error::Corrupt(_))));

        // A zero delta would repeat a document; a zero frequency is not a
        // posting at all.
        for (delta, tf) in [(0u64, 1u64), (1, 0)] {
            let mut bad = Vec::new();
            bad.push(FORMAT);
            put_varint(&mut bad, 1);
            put_varint(&mut bad, 1);
            put_varint(&mut bad, delta);
            put_varint(&mut bad, tf);
            assert!(matches!(decode(&bad), Err(Error::Corrupt(_))), "{delta} {tf}");
        }

        // A count far larger than the value's bytes must not size an
        // allocation from it.
        let mut huge = Vec::new();
        huge.push(FORMAT);
        put_varint(&mut huge, u64::MAX / 2);
        put_varint(&mut huge, 5);
        put_varint(&mut huge, 5);
        put_varint(&mut huge, 1);
        assert!(matches!(decode(&huge), Err(Error::Corrupt(_))));
    }

    fn pack_norms(norms: &[(u64, u32)]) -> Vec<(u64, Vec<u8>)> {
        let mut packer = NormPacker::new();
        let mut out = Vec::new();
        for (sequence, length) in norms {
            if let Some(done) = packer.push(*sequence, *length).unwrap() {
                out.push(done);
            }
        }
        if let Some(done) = packer.finish().unwrap() {
            out.push(done);
        }
        out
    }

    fn norms_of(block: u64, value: &[u8]) -> Vec<(u64, u32)> {
        let mut decoded = Vec::new();
        decode_norm_block(value, &mut decoded).unwrap();
        decoded
            .into_iter()
            .map(|(slot, length)| (block * NORM_BLOCK_SPAN + slot as u64, length))
            .collect()
    }

    #[test]
    fn a_norm_block_round_trips_every_document_and_every_hole() {
        for norms in [
            vec![(1u64, 0u32)],
            vec![(1, 7), (2, 0), (255, 4)],
            // Straddles three blocks with holes at both edges.
            vec![(255, 1), (256, 2), (511, 3), (512, 4), (700, u32::MAX)],
            (1..500u64).filter(|n| n % 3 != 0).map(|n| (n, (n % 97) as u32)).collect(),
        ] {
            let blocks = pack_norms(&norms);
            let mut seen = Vec::new();
            for (block, value) in &blocks {
                assert!(value.len() <= 1 + NORM_BITMAP_BYTES + 5 * NORM_BLOCK_SPAN as usize);
                seen.extend(norms_of(*block, value));
                // Only sequences of THIS block: `norm_in_block` matches the
                // slot, and the caller always pairs it with its own block.
                for (sequence, length) in norms.iter().filter(|(s, _)| norm_block_of(*s) == *block)
                {
                    assert_eq!(norm_in_block(value, *sequence).unwrap(), Some(*length));
                }
                let base = block * NORM_BLOCK_SPAN;
                for slot in 0..NORM_BLOCK_SPAN {
                    if !norms.iter().any(|(s, _)| *s == base + slot) {
                        assert_eq!(
                            norm_in_block(value, base + slot).unwrap(),
                            None,
                            "block {block} slot {slot} is a hole"
                        );
                    }
                }
            }
            assert_eq!(seen, norms, "{norms:?}");
        }
    }

    /// The choice the format makes, stated in bytes on the shape it was
    /// chosen for: 50K documents of short prose, every length under 128.
    #[test]
    fn varint_norm_blocks_are_smaller_than_a_fixed_width_table() {
        let norms: Vec<(u64, u32)> = (1..=512u64).map(|n| (n, 5 + (n % 20) as u32)).collect();
        let blocks = pack_norms(&norms);
        let packed: usize = blocks.iter().map(|(_, value)| value.len()).sum();
        let fixed: usize = blocks
            .iter()
            .map(|(block, value)| {
                let _ = block;
                let mut decoded = Vec::new();
                decode_norm_block(value, &mut decoded).unwrap();
                norm_block_fixed_width_bytes(decoded.len())
            })
            .sum();
        // 512 documents over three blocks -- 1..255, 256..511, 512 -- so three
        // bitmaps and 512 one-byte lengths.
        assert_eq!(blocks.len(), 3);
        assert_eq!(packed, 3 * (1 + NORM_BITMAP_BYTES) + 512);
        assert!(
            packed * 3 < fixed,
            "varint {packed} bytes vs fixed width {fixed}"
        );
    }

    #[test]
    fn a_malformed_norm_block_is_refused_rather_than_half_believed() {
        let (_, good) = pack_norms(&[(1, 3), (9, 0), (40, 260)]).pop().unwrap();
        let mut decoded = Vec::new();
        assert!(decode_norm_block(&good, &mut decoded).is_ok());

        let mut wrong_format = good.clone();
        wrong_format[0] = 2;
        // Clearing a presence bit leaves one varint nothing claims.
        let mut fewer = good.clone();
        fewer[1] &= !0x02;
        // Setting one leaves a varint missing.
        let mut more = good.clone();
        more[2] |= 0x01;
        for bad in [
            vec![],
            wrong_format,
            vec![FORMAT],                               // no bitmap
            vec![FORMAT; NORM_BITMAP_BYTES],            // bitmap truncated
            {
                let mut v = vec![FORMAT];
                v.extend_from_slice(&[0; NORM_BITMAP_BYTES]);
                v
            }, // presents nothing
            good[..good.len() - 1].to_vec(),            // truncated tail
            {
                let mut v = good.clone();
                v.push(0);
                v
            }, // trailing bytes
            fewer,
            more,
        ] {
            assert!(
                matches!(decode_norm_block(&bad, &mut decoded), Err(Error::Corrupt(_))),
                "{bad:?}"
            );
        }

        // A bitmap that claims every slot but carries a handful of bytes must
        // not size an allocation from the claim.
        let mut huge = vec![FORMAT];
        huge.extend_from_slice(&[0xff; NORM_BITMAP_BYTES]);
        huge.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(
            decode_norm_block(&huge, &mut decoded),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn the_norm_packer_refuses_documents_that_do_not_ascend() {
        let mut packer = NormPacker::new();
        packer.push(5, 1).unwrap();
        assert!(matches!(packer.push(5, 1), Err(Error::Corrupt(_))));
        assert!(matches!(packer.push(4, 1), Err(Error::Corrupt(_))));
        assert!(matches!(NormPacker::new().push(0, 1), Err(Error::Corrupt(_))));
    }

    #[test]
    fn the_packer_refuses_postings_that_do_not_ascend() {
        let mut packer = Packer::new();
        packer.push(5, 1).unwrap();
        assert!(matches!(packer.push(5, 1), Err(Error::Corrupt(_))));
        assert!(matches!(packer.push(4, 1), Err(Error::Corrupt(_))));
        assert!(matches!(Packer::new().push(0, 1), Err(Error::Corrupt(_))));
        assert!(matches!(Packer::new().push(1, 0), Err(Error::Corrupt(_))));
    }
}
