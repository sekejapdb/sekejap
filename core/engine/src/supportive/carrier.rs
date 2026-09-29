//! 2.0 The carrier (`docs/core/SUPPORTIVE.md` section 2.0): the byte formats of
//! the Anchor and the Register. Frozen: a shipped encoding never changes.

use crate::collections::{corrupt, invalid, read_ordered, ordered_into, Error};

type Result<T> = std::result::Result<T, Error>;

/// The Register's three trees, one per copy (2.0.3). Fixed, so repair finds
/// the Register without the Anchor.
pub(crate) const REGISTER_TREES: [u16; 3] = [0xFFFD, 0xFFFE, 0xFFFF];
/// The Anchor's payload room: the 2081-byte header packet less its framing.
pub(crate) const ANCHOR_PAYLOAD: usize = 2067;
/// One Register value, framing included.
pub(crate) const MAX_VALUE: usize = 8128;
/// One Register payload: `MAX_VALUE` less version, length and checksum.
pub(crate) const MAX_PAYLOAD: usize = MAX_VALUE - 1 - 4 - 4;
/// Census lines one Anchor holds.
pub(crate) const MAX_CENSUS: usize = 220;
/// A name, in bytes.
pub(crate) const MAX_NAME: usize = 255;

/// A four-letter entry kind. The first letter's case is the class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Kind([u8; 4]);

impl Kind {
    /// Four ASCII letters; letters 2-4 uppercase, so no two kinds differ by
    /// case alone and the first letter alone carries the class.
    pub(crate) fn new(code: &[u8; 4]) -> Result<Self> {
        if code[0].is_ascii_alphabetic() && code[1..].iter().all(u8::is_ascii_uppercase) {
            Ok(Self(*code))
        } else {
            Err(invalid(format!("entry kind {:?}: four letters, the last three uppercase", String::from_utf8_lossy(code))))
        }
    }
    /// Upper first letter: critical, three copies, an older reader refuses
    /// the file. Lower: ignorable, one copy, an older reader skips it.
    pub(crate) fn critical(&self) -> bool {
        self.0[0].is_ascii_uppercase()
    }
    pub(crate) fn code(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII at construction")
    }
}

/// A node of the tree, 2.a-2.g, as the key's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Node {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
}

/// What an entry describes (2.0.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OwnerClass {
    Database = 0,
    Schema = 1,
    Table = 2,
    Index = 3,
    EdgeType = 4,
    Graph = 5,
    Job = 6,
}

/// The last part of a key: its grammar is the kind's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Id(u64),
    /// `LAYT`: a layout's numbered part.
    Part { id: u64, part: u8 },
    /// `nIDX`: a name under its parent.
    Name { class: u8, name: Vec<u8> },
    /// A kind this build does not know: kept as bytes, never interpreted.
    Opaque(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) node: Node,
    pub(crate) owner_class: OwnerClass,
    pub(crate) owner_id: u64,
    pub(crate) kind: Kind,
    pub(crate) item: Item,
}

impl Node {
    fn byte(self) -> u8 {
        b'a' + self as u8
    }
    fn from_byte(b: u8) -> Option<Self> {
        [Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G].get(b.wrapping_sub(b'a') as usize).copied()
    }
}

impl OwnerClass {
    fn from_byte(b: u8) -> Option<Self> {
        [Self::Database, Self::Schema, Self::Table, Self::Index, Self::EdgeType, Self::Graph, Self::Job]
            .get(b as usize)
            .copied()
    }
}

/// The kinds whose item is not a plain id.
const LAYOUT_PART: &[u8; 4] = b"LAYT";
const NAME_INDEX: &[u8; 4] = b"nIDX";
/// Every kind this build knows, for the item grammar; any other kind's item
/// is kept opaque.
const KNOWN: [&[u8; 4]; 16] = [
    b"TREE", b"LIMT", b"cAPS", b"NEXT", b"NAME", b"nIDX", b"TABL", b"COLM", b"LAYT", b"KEYS", b"INDX", b"GRPH",
    b"BIND", b"MEMB", b"JOBS", b"rCNT",
];
const KNOWN_STATS: [&[u8; 4]; 2] = [b"tCRP", b"vENT"];

impl Key {
    /// `node | owner class | owner id | kind | item`, node first so a node's
    /// entries sit together (rule 7), ids order-preserving so they sort as
    /// numbers.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut k = Vec::with_capacity(32);
        k.push(self.node.byte());
        k.push(self.owner_class as u8);
        ordered_into(&mut k, self.owner_id);
        k.extend_from_slice(&self.kind.0);
        match &self.item {
            Item::Id(id) => ordered_into(&mut k, *id),
            Item::Part { id, part } => {
                ordered_into(&mut k, *id);
                k.push(*part);
            }
            Item::Name { class, name } => {
                if name.len() > MAX_NAME {
                    return Err(invalid(format!("a name is at most {MAX_NAME} bytes; this one is {}", name.len())));
                }
                k.push(*class);
                k.extend_from_slice(name);
            }
            Item::Opaque(bytes) => k.extend_from_slice(bytes),
        }
        if k.len() > u16::MAX as usize {
            return Err(invalid("a Register key longer than 65,535 bytes"));
        }
        Ok(k)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let node = bytes.first().copied().and_then(Node::from_byte).ok_or_else(|| corrupt("Register key node"))?;
        let owner_class =
            bytes.get(1).copied().and_then(OwnerClass::from_byte).ok_or_else(|| corrupt("Register key owner class"))?;
        let mut at = 2;
        let owner_id = read_ordered(bytes, &mut at)?;
        let code: &[u8; 4] =
            bytes.get(at..at + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| corrupt("Register key kind"))?;
        let kind = Kind::new(code).map_err(|_| corrupt("Register key kind"))?;
        at += 4;
        let rest = &bytes[at..];
        let item = if code == LAYOUT_PART {
            let mut i = 0;
            let id = read_ordered(rest, &mut i)?;
            match rest.get(i..) {
                Some([part]) => Item::Part { id, part: *part },
                _ => return Err(corrupt("layout part item")),
            }
        } else if code == NAME_INDEX {
            match rest.split_first() {
                Some((class, name)) if name.len() <= MAX_NAME => Item::Name { class: *class, name: name.to_vec() },
                _ => return Err(corrupt("name index item")),
            }
        } else if KNOWN.contains(&code) || KNOWN_STATS.contains(&code) {
            let mut i = 0;
            let id = read_ordered(rest, &mut i)?;
            if i != rest.len() {
                return Err(corrupt("Register key item"));
            }
            Item::Id(id)
        } else {
            Item::Opaque(rest.to_vec())
        };
        Ok(Self { node, owner_class, owner_id, kind, item })
    }
}

fn value_crc(key: &[u8], framed: &[u8]) -> u32 {
    let mut crc = crc32c::crc32c(&(key.len() as u16).to_be_bytes());
    crc = crc32c::crc32c_append(crc, key);
    crc32c::crc32c_append(crc, framed)
}

/// `version | payload length | payload | crc32c`, the checksum over the key
/// too, so a value read under the wrong key is caught.
pub(crate) fn encode_value(key: &[u8], version: u8, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return Err(invalid(format!(
            "a Register entry holds at most {MAX_PAYLOAD} payload bytes; this one is {}",
            payload.len()
        )));
    }
    let mut v = Vec::with_capacity(payload.len() + 9);
    v.push(version);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    let crc = value_crc(key, &v);
    v.extend_from_slice(&crc.to_le_bytes());
    Ok(v)
}

/// The version and payload of a stored value, after its length and checksum
/// are checked -- the length before anything is allocated.
pub(crate) fn decode_value<'a>(key: &[u8], value: &'a [u8]) -> Result<(u8, &'a [u8])> {
    if value.len() < 9 || value.len() > MAX_VALUE {
        return Err(corrupt("Register value size"));
    }
    let len = u32::from_be_bytes(value[1..5].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD || 5 + len + 4 != value.len() {
        return Err(corrupt("Register value length"));
    }
    let want = u32::from_le_bytes(value[5 + len..].try_into().unwrap());
    if value_crc(key, &value[..5 + len]) != want {
        return Err(corrupt("Register value checksum"));
    }
    Ok((value[0], &value[5..5 + len]))
}

/// One line of the census: a (kind, version, variant) the file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CensusLine {
    pub(crate) kind: Kind,
    pub(crate) version: u8,
    pub(crate) variant: u32,
}

/// The Anchor's payload (2.0.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Anchor {
    /// Root page of each Register copy, in `REGISTER_TREES` order.
    pub(crate) roots: [u32; 3],
    /// Sorted, no duplicates.
    pub(crate) census: Vec<CensusLine>,
}

/// Register format written by this build.
const REGISTER_FORMAT: u16 = 1;
/// format u16, root count u8, three (tree id u16, root u32), census count u8.
const ANCHOR_FIXED: usize = 2 + 1 + 3 * 6 + 1;
const CENSUS_LINE: usize = 4 + 1 + 4;

fn census_in_order(census: &[CensusLine]) -> bool {
    census.len() <= MAX_CENSUS && census.windows(2).all(|w| w[0] < w[1])
}

impl Anchor {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        if !census_in_order(&self.census) {
            return Err(invalid(format!(
                "the census holds at most {MAX_CENSUS} lines, sorted, none repeated; this one has {}",
                self.census.len()
            )));
        }
        let mut p = Vec::with_capacity(ANCHOR_FIXED + self.census.len() * CENSUS_LINE);
        p.extend_from_slice(&REGISTER_FORMAT.to_be_bytes());
        p.push(3);
        for (tree, root) in REGISTER_TREES.iter().zip(self.roots) {
            p.extend_from_slice(&tree.to_be_bytes());
            p.extend_from_slice(&root.to_be_bytes());
        }
        p.push(self.census.len() as u8);
        for line in &self.census {
            p.extend_from_slice(&line.kind.0);
            p.push(line.version);
            p.extend_from_slice(&line.variant.to_be_bytes());
        }
        debug_assert!(p.len() <= ANCHOR_PAYLOAD);
        Ok(p)
    }

    pub(crate) fn decode(p: &[u8]) -> Result<Self> {
        if p.len() < ANCHOR_FIXED || p.len() > ANCHOR_PAYLOAD {
            return Err(corrupt("Anchor size"));
        }
        let format = u16::from_be_bytes([p[0], p[1]]);
        if format != REGISTER_FORMAT {
            return Err(Error::Unsupported(format!("needs a newer sekejap: Register format {format}")));
        }
        if p[2] != 3 {
            return Err(corrupt("Anchor root count"));
        }
        let mut roots = [0u32; 3];
        for (i, tree) in REGISTER_TREES.iter().enumerate() {
            let at = 3 + i * 6;
            if u16::from_be_bytes([p[at], p[at + 1]]) != *tree {
                return Err(corrupt("Anchor Register tree id"));
            }
            roots[i] = u32::from_be_bytes(p[at + 2..at + 6].try_into().unwrap());
        }
        let n = p[ANCHOR_FIXED - 1] as usize;
        let end = ANCHOR_FIXED + n * CENSUS_LINE;
        if end > p.len() {
            return Err(corrupt("Anchor census length"));
        }
        let mut census = Vec::with_capacity(n);
        for line in p[ANCHOR_FIXED..end].chunks_exact(CENSUS_LINE) {
            let kind = Kind::new(line[..4].try_into().unwrap()).map_err(|_| corrupt("Anchor census kind"))?;
            census.push(CensusLine { kind, version: line[4], variant: u32::from_be_bytes(line[5..9].try_into().unwrap()) });
        }
        if !census_in_order(&census) {
            return Err(corrupt("Anchor census order"));
        }
        if p[end..].iter().any(|b| *b != 0) {
            return Err(corrupt("Anchor reserved bytes"));
        }
        Ok(Self { roots, census })
    }
}

/// Admission (2.0.3): refuse the file, naming it, when the census holds a
/// critical (kind, version, variant) this build does not support. Returns the
/// ignorable lines it does not know, which the caller skips.
pub(crate) fn admit(
    census: &[CensusLine],
    supported: &dyn Fn(&CensusLine) -> bool,
) -> Result<Vec<CensusLine>> {
    let mut skipped = Vec::new();
    for line in census {
        if supported(line) {
            continue;
        }
        if line.kind.critical() {
            return Err(Error::Unsupported(format!(
                "needs a newer sekejap: {} version {} variant {}",
                line.kind.code(),
                line.version,
                line.variant
            )));
        }
        skipped.push(*line);
    }
    Ok(skipped)
}
