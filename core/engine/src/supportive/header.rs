//! Build step 3a: the database header on a Register file
//! (`docs/core/SUPPORTIVE.md` 2.i).
//!
//! A 0.18 file keeps three 2,081-byte header copies at `[0,0,copy]`: two
//! counters, the optional resource policy and the 24-bit feature word. A
//! Register file keeps the Anchor at those keys instead, and the same facts as
//! entries:
//!
//! * the counters are `NEXT` entries of the database (id classes 1, 2, 3);
//! * the resource policy is the `LIMT` entry;
//! * every feature bit is one census line (the table [`LEGACY`]); the feature
//!   word the rest of the engine reads is derived from the census.
//!
//! The index count the 0.18 header stored is derived: it is the number of
//! registry entries, counted by the caller.

use super::anchor::{read_anchor, write_anchor, ANCHOR_MAGIC};
use super::carrier::{admit, Anchor, CensusLine, Item, Key, Kind, Node, OwnerClass};
use super::register::{Register, Trees, TreesMut};
use crate::collections::{corrupt, Error};

type Result<T> = std::result::Result<T, Error>;

/// `NEXT` id classes (2.i). Only the three database counters are written by
/// step 3a; the rest are numbered here so the table is complete in one place.
pub(crate) const NEXT_TABLE: u64 = 1;
pub(crate) const NEXT_LAYOUT: u64 = 2;
pub(crate) const NEXT_INDEX: u64 = 3;

/// 2.h/2.i: each legacy feature bit and the census line that stands for it.
/// Bit 0x1 ("typed indexes exist") is the `NEXT` index-class line itself.
pub(crate) const LEGACY: [(u64, &[u8; 4], u8, u32); 23] = [
    (0x2, b"GRPH", 1, 0),
    (0x4, b"INDX", 1, 4),
    (0x8, b"INDX", 1, 7),
    (0x10, b"INDX", 1, 1),
    (0x20, b"INDX", 1, 5),
    (0x40, b"INDX", 1, 2),
    (0x80, b"TREE", 1, 0),
    (0x100, b"INDX", 1, 8),
    (0x200, b"JOBS", 1, 3),
    (0x400, b"INDX", 2, 0),
    (0x800, b"COLM", 1, 1),
    (0x1000, b"COLM", 1, 2),
    (0x2000, b"rCNT", 1, 0),
    (0x4000, b"INDX", 1, 9),
    (0x8000, b"INDX", 1, 6),
    (0x10000, b"INDX", 3, 0),
    (0x20000, b"NAME", 1, 1),
    (0x40000, b"NEXT", 1, 5),
    (0x80000, b"BIND", 1, 0),
    (0x100000, b"KEYS", 1, 0),
    (0x200000, b"COLM", 1, 3),
    (0x400000, b"MEMB", 1, 0),
    (0x800000, b"INDX", 1, 3),
];

fn line(code: &[u8; 4], version: u8, variant: u32) -> CensusLine {
    CensusLine { kind: Kind::new(code).expect("a registry kind is well formed"), version, variant }
}

/// The lines every Register file this build writes may carry besides
/// [`LEGACY`]: the three database counters and the resource policy.
fn base_lines() -> [CensusLine; 4] {
    [
        line(b"NEXT", 1, NEXT_TABLE as u32),
        line(b"NEXT", 1, NEXT_LAYOUT as u32),
        line(b"NEXT", 1, NEXT_INDEX as u32),
        line(b"LIMT", 1, 0),
    ]
}

/// Whether this build implements a census line.
pub(crate) fn supported(l: &CensusLine) -> bool {
    base_lines().contains(l)
        || super::schema::lines().contains(l)
        || LEGACY.iter().any(|(_, c, v, n)| line(c, *v, *n) == *l)
}

/// The census lines a feature word needs.
pub(crate) fn census_of(features: u64) -> Vec<CensusLine> {
    LEGACY
        .iter()
        .filter(|(bit, ..)| features & bit != 0)
        .map(|(_, c, v, n)| line(c, *v, *n))
        .collect()
}

/// The feature word a census declares (without bit 0x1).
pub(crate) fn features_of(census: &[CensusLine]) -> u64 {
    LEGACY
        .iter()
        .filter(|(_, c, v, n)| census.contains(&line(c, *v, *n)))
        .fold(0, |acc, (bit, ..)| acc | bit)
}

/// Whether this process works in 0.18 compatibility: databases it creates
/// are 0.18-format files, and it opens 0.18-format files. Only
/// `SEKEJAP_CREATE_REGISTER=0` turns it on -- the tests that read or edit
/// 0.18 bytes set it through `internal::LegacyFormat` -- and a unit test can
/// pin its own thread either way. Everywhere else a new database is a
/// Register file and a 0.18 file is refused until `sekejap-upgrade` moves it.
pub(crate) fn legacy_mode() -> bool {
    #[cfg(test)]
    if let Some(register) = FORCE.with(|f| f.get()) {
        return !register;
    }
    if let Some(legacy) = PIN.with(|p| p.get()) {
        return legacy;
    }
    std::env::var_os("SEKEJAP_CREATE_REGISTER").is_some_and(|v| v == "0")
}

thread_local! {
    /// `internal::LegacyFormat`'s pin: THIS thread only, so a test that pins
    /// 0.18 compatibility never changes what a test running beside it on
    /// another thread creates or opens.
    static PIN: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Set this thread's pin, returning the one it replaces.
pub(crate) fn pin_legacy(on: Option<bool>) -> Option<bool> {
    PIN.with(|p| p.replace(on))
}

/// Whether new databases are created as Register files.
pub(crate) fn create_switch() -> bool {
    !legacy_mode()
}
#[cfg(test)]
thread_local! {
    /// `Some` pins this thread's new databases to one format, whatever the
    /// environment says: a test of the carrier itself builds on a 0.18 file.
    pub(crate) static FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the header keys hold an Anchor rather than a 0.18 header. One
/// intact-looking magic is enough; the Anchor read then applies the replica
/// rules to all three copies.
pub(crate) fn anchored(s: &(impl Trees + ?Sized)) -> Result<bool> {
    for copy in 0..3u8 {
        match s.get(&[0, 0, copy]) {
            Ok(Some(b)) if b.starts_with(ANCHOR_MAGIC) => return Ok(true),
            Ok(_) | Err(Error::Kernel(kernel::Error::Corrupt { .. })) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

fn next_key(class: u64) -> Key {
    Key {
        node: Node::B,
        owner_class: OwnerClass::Database,
        owner_id: 0,
        kind: Kind::new(b"NEXT").unwrap(),
        item: Item::Id(class),
    }
}
fn limit_key() -> Key {
    Key {
        node: Node::A,
        owner_class: OwnerClass::Database,
        owner_id: 0,
        kind: Kind::new(b"LIMT").unwrap(),
        item: Item::Id(0),
    }
}

/// The header facts of a Register file, as the 0.18 header held them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Fields {
    pub(crate) next_collection: u32,
    pub(crate) next_layout: u32,
    /// The kernel's 56-byte policy record, undecoded.
    pub(crate) limits: Option<Vec<u8>>,
    /// Feature word (with bit 0x1) and next index id, when indexes exist.
    pub(crate) index: Option<(u64, u64)>,
}

/// An open database's Register and the Anchor last written for it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Supportive {
    pub(crate) register: Register,
    pub(crate) anchor: Anchor,
    pub(crate) fields: Fields,
}

impl Supportive {
    /// A new Register file: the trees, the counters, the policy, the Anchor.
    pub(crate) fn create(store: &mut impl TreesMut, limits: Option<Vec<u8>>) -> Result<Self> {
        let register = Register::create(store)?;
        let mut s = Self {
            register,
            anchor: Anchor { roots: register.roots(), census: Vec::new() },
            fields: Fields { next_collection: 0, next_layout: 0, limits: None, index: None },
        };
        if let Some(l) = &limits {
            s.register.put(store, &limit_key(), 1, l)?;
            s.fields.limits = limits.clone();
        }
        s.write(store, 1, 1, None)?;
        Ok(s)
    }

    /// Read and admit the header of a Register file.
    pub(crate) fn read(s: &(impl Trees + ?Sized)) -> Result<Self> {
        let anchor = read_anchor(s)?;
        admit(&anchor.census, &supported)?;
        let register = Register::open(anchor.roots);
        let has = |l: CensusLine| anchor.census.contains(&l);
        let counter = |class: u64| -> Result<Option<u64>> {
            if !has(line(b"NEXT", 1, class as u32)) {
                return Ok(None);
            }
            match register.get(s, &next_key(class))? {
                Some((1, p)) if p.len() == 8 => Ok(Some(u64::from_be_bytes(p.try_into().unwrap()))),
                Some(_) => Err(corrupt("NEXT entry")),
                None => Err(corrupt("census names a missing NEXT entry")),
            }
        };
        let small = |v: Option<u64>| -> Result<u32> {
            match v.map(u32::try_from) {
                Some(Ok(n)) if n != 0 => Ok(n),
                _ => Err(corrupt("header counters")),
            }
        };
        let next_collection = small(counter(NEXT_TABLE)?)?;
        let next_layout = small(counter(NEXT_LAYOUT)?)?;
        let index = match counter(NEXT_INDEX)? {
            Some(0) => return Err(corrupt("index allocator")),
            Some(next) => Some((1 | features_of(&anchor.census), next)),
            None if features_of(&anchor.census) != 0 => {
                return Err(corrupt("feature census without an index allocator"))
            }
            None => None,
        };
        let limits = match (has(line(b"LIMT", 1, 0)), register.get(s, &limit_key())?) {
            (true, Some((1, p))) => Some(p),
            (false, None) => None,
            _ => return Err(corrupt("LIMT entry")),
        };
        Ok(Self {
            register,
            anchor,
            fields: Fields { next_collection, next_layout, limits, index },
        })
    }

    /// Persist the header facts, writing only the entries that changed and
    /// the Anchor when the census grew or a root moved. The census only
    /// grows: a feature bit cleared in memory keeps its line.
    pub(crate) fn write(
        &mut self,
        store: &mut impl TreesMut,
        next_collection: u32,
        next_layout: u32,
        index: Option<(u64, u64)>,
    ) -> Result<()> {
        let mut census = self.anchor.census.clone();
        let mut need = |l: CensusLine| {
            if !census.contains(&l) {
                census.push(l);
            }
        };
        need(line(b"NEXT", 1, NEXT_TABLE as u32));
        need(line(b"NEXT", 1, NEXT_LAYOUT as u32));
        if self.fields.limits.is_some() {
            need(line(b"LIMT", 1, 0));
        }
        if let Some((features, _)) = index {
            need(line(b"NEXT", 1, NEXT_INDEX as u32));
            census_of(features).into_iter().for_each(&mut need);
        }
        census.sort();
        let old = self.fields.clone();
        for (class, new, was) in [
            (NEXT_TABLE, Some(u64::from(next_collection)), Some(u64::from(old.next_collection))),
            (NEXT_LAYOUT, Some(u64::from(next_layout)), Some(u64::from(old.next_layout))),
            (NEXT_INDEX, index.map(|i| i.1), old.index.map(|i| i.1)),
        ] {
            if let Some(n) = new {
                if new != was {
                    self.register.put(store, &next_key(class), 1, &n.to_be_bytes())?;
                }
            }
        }
        self.fields.next_collection = next_collection;
        self.fields.next_layout = next_layout;
        self.fields.index = index;
        self.sync(store, census)
    }

    /// Rewrite the Anchor when `census` differs from the one on disk or a
    /// Register root moved. Called after every Register write, in the same
    /// transaction.
    pub(crate) fn sync(&mut self, store: &mut impl TreesMut, mut census: Vec<CensusLine>) -> Result<()> {
        census.sort();
        census.dedup();
        if census != self.anchor.census || self.register.roots() != self.anchor.roots {
            let anchor = Anchor { roots: self.register.roots(), census };
            write_anchor(store, &anchor)?;
            self.anchor = anchor;
        }
        Ok(())
    }

    /// Put an entry and make sure the census names its line.
    pub(crate) fn put(
        &mut self,
        store: &mut impl TreesMut,
        key: &Key,
        line: CensusLine,
        payload: &[u8],
    ) -> Result<()> {
        self.register.put(store, key, line.version, payload)?;
        if !self.anchor.census.contains(&line) {
            let mut census = self.anchor.census.clone();
            census.push(line);
            return self.sync(store, census);
        }
        if self.register.roots() != self.anchor.roots {
            return self.sync(store, self.anchor.census.clone());
        }
        Ok(())
    }

    pub(crate) fn delete(&mut self, store: &mut impl TreesMut, key: &Key) -> Result<bool> {
        let found = self.register.delete(store, key)?;
        if self.register.roots() != self.anchor.roots {
            self.sync(store, self.anchor.census.clone())?;
        }
        Ok(found)
    }
}
