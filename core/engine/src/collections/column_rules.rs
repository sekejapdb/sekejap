//! Per-field COLUMN RULES: a DEFAULT generator and a NOT NULL flag, recorded
//! in the collection descriptor and applied when a row is assembled.
//!
//! The shape follows the DECLARED-TYPE tail exactly (`mod.rs`): the rules
//! live in the catalog record's own tail behind a bit of the frozen flags
//! byte, and the FILE carries [`COLUMN_RULES_FEATURE`] so a binary that
//! predates the tail refuses the file at admission rather than reading the
//! tail as part of the name. Two lines, the same two the DROPPING mark and
//! the declared pairs have.
//!
//! What the rules do NOT touch: the row codec, the index keys, and the
//! layout. A default is filled into the document before `encode_dense_v3`
//! sees it, so a defaulted field is encoded exactly as a written one is, and
//! an index over it is maintained by the ordinary hook. A NOT NULL check is a
//! refusal before any write, never a repair.
//!
//! `docs/lang/QL_CONTRACT.md` §2 (`DEFAULT now()`, `DEFAULT uuid4()`,
//! `DEFAULT uuid5(ns, name)`, `NOT NULL`).

use super::{invalid, Catalog, Database, Error, Result};
use crate::Kind;
use serde_json::Value;

/// The generator a `DEFAULT` names. The set is CLOSED: each member is O(1)
/// per row -- one clock read, sixteen random bytes, one hash over a fixed
/// namespace and name -- which is what makes the slot a write-path atomic
/// rather than an expression evaluator. An arbitrary expression is Tier 3
/// (QL_CONTRACT §2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    /// `now()`: UTC microseconds since 1970-01-01, the encoding every
    /// declared TIMESTAMPTZ/DATE column uses (QL_CONTRACT §5 deviation 8).
    /// Read ONCE per row, so two `now()` columns of one row agree.
    Now,
    /// `uuid4()`: sixteen bytes from the operating system, version 4.
    Uuid4,
    /// `uuid5(namespace, name)`: RFC 4122 §4.3, SHA-1 over the namespace
    /// bytes followed by the name bytes. Deterministic, so the same row
    /// written twice carries the same value.
    Uuid5 {
        /// The namespace UUID, as its sixteen bytes in RFC order.
        namespace: [u8; 16],
        name: String,
    },
}

/// The rule slot of one field: a default, a NOT NULL flag, or both. A slot
/// with neither is not a rule and is refused, so a descriptor never carries
/// an entry that decides nothing.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ColumnRule {
    pub default: Option<DefaultValue>,
    pub not_null: bool,
}

/// Bit 3 of the catalog packet's frozen flags byte: the record carries a
/// COLUMN RULES tail.
///
/// Placed there for the reason the DROPPING bit and the DECLARED bit were: a
/// binary that predates it tests `b[8] & !(1 | 2 | 4)` and refuses the record
/// rather than reading the tail as part of the name. It is the SECOND line,
/// behind [`COLUMN_RULES_FEATURE`].
pub(crate) const CATALOG_RULES: u8 = 8;

/// The collection-header bit that says this database's catalog carries at
/// least one COLUMN RULE.
///
/// Additive and monotone (Law 8): set in the same transaction that first
/// records a rule, never by opening and never by an ordinary write, and never
/// cleared -- a file that ever carried the tail is still a file an older
/// binary must refuse. It exists for the refusal CLASS: without it a rollback
/// binary passes header admission, reaches `parse_catalog`, fails the flags
/// byte and reports an intact, valid, newer file as `Corrupt`. Law 8
/// separates an unknown format from damage, so the bit makes the refusal
/// `Unsupported` at admission, before a record is read at all.
pub const COLUMN_RULES_FEATURE: u64 = 0x1000;

// ── the descriptor tail ───────────────────────────────────────────────────

const DEFAULT_NOW: u8 = 1;
const DEFAULT_UUID4: u8 = 2;
const DEFAULT_UUID5: u8 = 3;
const RULE_NOT_NULL: u8 = 1;
const RULE_HAS_DEFAULT: u8 = 2;

/// One rule entry, appended to `out`.
pub(super) fn encode_rule(out: &mut Vec<u8>, field: &str, rule: &ColumnRule) -> Result<()> {
    if field.is_empty() || field.len() > 255 {
        return Err(invalid("a column rule names a field of 1..255 bytes"));
    }
    if rule.default.is_none() && !rule.not_null {
        return Err(invalid(format!(
            "the column rule for `{field}` sets neither a DEFAULT nor NOT NULL"
        )));
    }
    out.push(field.len() as u8);
    out.extend_from_slice(field.as_bytes());
    out.push(
        if rule.not_null { RULE_NOT_NULL } else { 0 }
            | if rule.default.is_some() {
                RULE_HAS_DEFAULT
            } else {
                0
            },
    );
    match &rule.default {
        None => {}
        Some(DefaultValue::Now) => out.push(DEFAULT_NOW),
        Some(DefaultValue::Uuid4) => out.push(DEFAULT_UUID4),
        Some(DefaultValue::Uuid5 { namespace, name }) => {
            if name.len() > 255 {
                return Err(invalid("a uuid5 name is at most 255 bytes"));
            }
            out.push(DEFAULT_UUID5);
            out.extend_from_slice(namespace);
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
    }
    Ok(())
}

/// One rule entry, read back. `read` is the cursor `parse_catalog` holds.
pub(super) fn decode_rule(
    mut read: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<(String, ColumnRule)> {
    let n = read(1)?[0] as usize;
    let field = String::from_utf8(read(n)?).map_err(super::corrupt)?;
    let flags = read(1)?[0];
    if field.is_empty() || flags & !(RULE_NOT_NULL | RULE_HAS_DEFAULT) != 0 {
        return Err(super::corrupt("catalog column rule entry"));
    }
    let default = if flags & RULE_HAS_DEFAULT == 0 {
        None
    } else {
        Some(match read(1)?[0] {
            DEFAULT_NOW => DefaultValue::Now,
            DEFAULT_UUID4 => DefaultValue::Uuid4,
            DEFAULT_UUID5 => {
                let mut namespace = [0u8; 16];
                namespace.copy_from_slice(&read(16)?);
                let n = read(1)?[0] as usize;
                DefaultValue::Uuid5 {
                    namespace,
                    name: String::from_utf8(read(n)?).map_err(super::corrupt)?,
                }
            }
            _ => return Err(super::corrupt("catalog column rule generator")),
        })
    };
    if default.is_none() && flags & RULE_NOT_NULL == 0 {
        return Err(super::corrupt("catalog column rule decides nothing"));
    }
    Ok((field, ColumnRule { default, not_null: flags & RULE_NOT_NULL != 0 }))
}

/// Every rule names a field of the new layout, and names a `Kind` its
/// generator can produce. Called before anything is written, so a refused
/// rule leaves the descriptor as it was.
pub(super) fn check_rules(
    fields: &[(String, Kind)],
    rules: &[(String, ColumnRule)],
) -> Result<()> {
    for (field, rule) in rules {
        let Some((_, kind)) = fields.iter().find(|(n, _)| n == field) else {
            return Err(invalid(format!(
                "the column rule for `{field}` names a field the collection has not got"
            )));
        };
        if rule.default.is_none() && !rule.not_null {
            return Err(invalid(format!(
                "the column rule for `{field}` sets neither a DEFAULT nor NOT NULL"
            )));
        }
        match (&rule.default, kind) {
            (None, _) => {}
            (Some(DefaultValue::Now), Kind::Int) => {}
            (Some(DefaultValue::Now), other) => {
                return Err(invalid(format!(
                    "DEFAULT now() on `{field}`: a stored instant is Kind::Int microseconds, and `{field}` is {other:?}"
                )))
            }
            (Some(DefaultValue::Uuid4 | DefaultValue::Uuid5 { .. }), Kind::Text) => {}
            (Some(DefaultValue::Uuid4 | DefaultValue::Uuid5 { .. }), other) => {
                return Err(invalid(format!(
                    "DEFAULT uuid4()/uuid5() on `{field}`: a UUID is written as text, and `{field}` is {other:?}"
                )))
            }
        }
        if rules.iter().filter(|(n, _)| n == field).count() != 1 {
            return Err(invalid(format!("`{field}` carries two column rules")));
        }
    }
    Ok(())
}

// ── the write path ────────────────────────────────────────────────────────

impl Database {
    /// Fill the defaults of a row being assembled, then refuse it if a NOT
    /// NULL column is still MISSING or NULL.
    ///
    /// The order is the contract's: a default fills a MISSING field only, so
    /// an INSERT that writes NULL on purpose keeps its NULL -- and is then
    /// refused if the column is NOT NULL. `now()` is read once for the row,
    /// so two `now()` columns of one row cannot disagree.
    pub(super) fn apply_column_rules(&self, c: &Catalog, doc: &mut Value) -> Result<()> {
        let mut now: Option<i64> = None;
        for (field, rule) in &c.rules {
            let Some(generator) = &rule.default else {
                continue;
            };
            if doc.get(field).is_some() {
                continue;
            }
            let value = match generator {
                DefaultValue::Now => {
                    Value::from(*now.get_or_insert_with(|| self.clock.unix_micros()))
                }
                DefaultValue::Uuid4 => Value::from(uuid4()?),
                DefaultValue::Uuid5 { namespace, name } => {
                    Value::from(uuid5(namespace, name.as_bytes()))
                }
            };
            doc[field.as_str()] = value;
        }
        for (field, rule) in &c.rules {
            if !rule.not_null {
                continue;
            }
            match doc.get(field) {
                None => {
                    return Err(invalid(format!(
                    "`{field}` is NOT NULL and this row does not carry it; a NOT NULL column refuses a MISSING field and a NULL one alike"
                )))
                }
                Some(Value::Null) => {
                    return Err(invalid(format!(
                    "`{field}` is NOT NULL and this row writes NULL; a NOT NULL column refuses a MISSING field and a NULL one alike"
                )))
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

// ── UUIDs (RFC 4122) ──────────────────────────────────────────────────────

/// The sixteen bytes of a UUID, written the way RFC 4122 §3 writes one.
fn format_uuid(b: &[u8; 16]) -> String {
    let hex = |range: std::ops::Range<usize>| {
        b[range].iter().map(|x| format!("{x:02x}")).collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )
}

/// RFC 4122 §4.4: sixteen random bytes with the version and variant fields
/// overwritten. The randomness is the operating system's, through the same
/// `getrandom` the page-WAL identity uses.
fn uuid4() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b)
        .map_err(|e| Error::Kernel(kernel::Error::Io(std::io::Error::other(e.to_string()))))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(format_uuid(&b))
}

/// RFC 4122 §4.3: SHA-1 over the namespace bytes followed by the name bytes,
/// truncated to sixteen bytes, version 5, variant 2.
fn uuid5(namespace: &[u8; 16], name: &[u8]) -> String {
    let mut input = namespace.to_vec();
    input.extend_from_slice(name);
    let digest = sha1(&input);
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    format_uuid(&b)
}

/// SHA-1 (FIPS 180-4 §6.1), in the tree because RFC 4122 §4.3 names it and
/// version-5 UUIDs are the only caller. No dependency is added for it.
///
/// It is a digest for a NAME, never for a secret: SHA-1 is broken for
/// collision resistance and RFC 4122 uses it anyway, because a v5 UUID
/// promises determinism, not unforgeability.
pub fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&(message.len() as u64 * 8).to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().expect("four bytes"));
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (chunk, word) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// A namespace UUID read from its written form (`6ba7b810-...`). The rule
/// slot stores the sixteen bytes, so the spelling is parsed once, when the
/// rule is recorded, not once per row.
pub fn parse_uuid(text: &str) -> Result<[u8; 16]> {
    let hex: Vec<u8> = text
        .strip_prefix("urn:uuid:")
        .unwrap_or(text)
        .bytes()
        .filter(|b| *b != b'-')
        .collect();
    if hex.len() != 32 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid(format!(
            "`{text}` is not a UUID: a namespace is 32 hexadecimal digits, hyphenated as 8-4-4-4-12"
        )));
    }
    let mut out = [0u8; 16];
    for (at, pair) in hex.chunks_exact(2).enumerate() {
        out[at] = u8::from_str_radix(std::str::from_utf8(pair).expect("ascii hex"), 16)
            .map_err(invalid)?;
    }
    Ok(out)
}

// ── what the layers above reach for ───────────────────────────────────────

/// One header's feature word against the mask a binary implements, for a test
/// that asks what an OLDER binary answers for a file this build writes. It is
/// the production decision `parse_header` makes, not a copy of it.
pub fn admit_logical_features(features: u64, supported: u64) -> Result<()> {
    super::admit_features(features, supported)
}

/// The logical feature word this open database has written.
pub fn logical_features(db: &Database) -> u64 {
    db.index_header.map_or(0, |h| h.features)
}

/// The layout id the next `alter_collection` on this database will write.
/// `EXPLAIN ALTER TABLE` prints it without running the statement.
pub fn next_layout_id(db: &Database) -> Result<u64> {
    Ok(u64::from(db.header()?.1))
}
