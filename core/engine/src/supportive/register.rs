//! 2.0.3 The Register, persisted: three B-trees with fixed ids, one copy of
//! every critical entry in each and ignorable entries in copy 0 only, with
//! the damage rules of `docs/core/SUPPORTIVE.md` 2.0.3.

use super::carrier::{decode_value, encode_value, Key, Node, REGISTER_TREES};
use crate::collections::{corrupt, Error};
use crate::pagewal::PageWalStore;
use crate::store::Backend;

type Result<T> = std::result::Result<T, Error>;

/// What the Register needs to read: the header keys and point reads in a
/// named tree. The live store, the offline source reader and a rebuild's
/// destination all offer it.
pub(crate) trait Trees {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>>;
    fn tree_get(&self, tree: u16, root: u32, k: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Records of a tree from `from` in order, until `each` answers false.
    fn tree_scan(&self, tree: u16, root: u32, from: &[u8], each: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()>;
}
/// What the Register needs to write.
pub(crate) trait TreesMut {
    fn put(&mut self, k: &[u8], v: &[u8]) -> Result<()>;
    fn tree_create(&mut self, tree: u16) -> Result<u32>;
    fn tree_put(&mut self, tree: u16, root: u32, k: &[u8], v: &[u8]) -> Result<u32>;
    fn tree_delete(&mut self, tree: u16, root: u32, k: &[u8]) -> Result<(bool, u32)>;
}
impl Trees for PageWalStore {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        PageWalStore::get(self, k).map_err(Error::from)
    }
    fn tree_get(&self, tree: u16, root: u32, k: &[u8]) -> Result<Option<Vec<u8>>> {
        PageWalStore::tree_get(self, tree, root, k).map_err(Error::from)
    }
    fn tree_scan(&self, tree: u16, root: u32, from: &[u8], each: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()> {
        if let Some(iter) = PageWalStore::tree_range(self, tree, root, from)? {
            for record in iter {
                let (k, v) = record?;
                if !each(&k, &v)? {
                    break;
                }
            }
        }
        Ok(())
    }
}
impl Trees for crate::pagewal::CurrentSourceReader {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Self::get(self, k).map_err(Error::from)
    }
    fn tree_get(&self, tree: u16, root: u32, k: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_in(tree, root, k).map_err(Error::from)
    }
    fn tree_scan(&self, tree: u16, root: u32, from: &[u8], each: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()> {
        // The streamed range has no early stop but an error; a private
        // marker error ends it and is not reported.
        let mut inner = None;
        let mut stopped = false;
        let r = self.visit_tree_range(tree, root, from, None, |k, v| match each(k, v) {
            Ok(true) => Ok(()),
            Ok(false) => {
                stopped = true;
                Err(kernel::Error::Corrupt { page_no: 0, why: "register scan stop" })
            }
            Err(e) => {
                inner = Some(e);
                Err(kernel::Error::Corrupt { page_no: 0, why: "register scan" })
            }
        });
        match (r, inner) {
            (_, Some(e)) => Err(e),
            (Err(_), None) if stopped => Ok(()),
            (Err(e), None) => Err(e.into()),
            (Ok(_), None) => Ok(()),
        }
    }
}
impl Trees for Backend {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Backend::get(self, k).map_err(Error::from)
    }
    fn tree_get(&self, tree: u16, root: u32, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Backend::tree_get(self, tree, root, k).map_err(Error::from)
    }
    fn tree_scan(&self, tree: u16, root: u32, from: &[u8], each: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()> {
        Trees::tree_scan(self.store(), tree, root, from, each)
    }
}
macro_rules! trees_mut {
    ($t:ty) => {
        impl TreesMut for $t {
            fn put(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
                <$t>::put(self, k, v).map_err(Error::from)
            }
            fn tree_create(&mut self, tree: u16) -> Result<u32> {
                <$t>::tree_create(self, tree).map_err(Error::from)
            }
            fn tree_put(&mut self, tree: u16, root: u32, k: &[u8], v: &[u8]) -> Result<u32> {
                <$t>::tree_put(self, tree, root, k, v).map_err(Error::from)
            }
            fn tree_delete(&mut self, tree: u16, root: u32, k: &[u8]) -> Result<(bool, u32)> {
                <$t>::tree_delete(self, tree, root, k).map_err(Error::from)
            }
        }
    };
}
trees_mut!(Backend);
trees_mut!(PageWalStore);

/// The Register of one open database: the three copies' roots. The roots are
/// saved in the Anchor by the caller, in the same commit as any write here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Register {
    roots: [u32; 3],
}

impl Register {
    /// Three empty trees, one per copy, under the fixed tree ids.
    pub(crate) fn create(store: &mut impl TreesMut) -> Result<Self> {
        let mut roots = [0u32; 3];
        for (i, tree) in REGISTER_TREES.iter().enumerate() {
            roots[i] = store.tree_create(*tree)?;
        }
        Ok(Self { roots })
    }
    pub(crate) fn open(roots: [u32; 3]) -> Self {
        Self { roots }
    }
    pub(crate) fn roots(&self) -> [u32; 3] {
        self.roots
    }

    /// The copies an entry of this kind lives in: all three for a critical
    /// kind, copy 0 for an ignorable one.
    fn copies(key: &Key) -> usize {
        if key.kind.critical() { 3 } else { 1 }
    }

    pub(crate) fn put(&mut self, store: &mut impl TreesMut, key: &Key, version: u8, payload: &[u8]) -> Result<()> {
        let k = key.encode()?;
        let v = encode_value(&k, version, payload)?;
        for i in 0..Self::copies(key) {
            self.roots[i] = store.tree_put(REGISTER_TREES[i], self.roots[i], &k, &v)?;
        }
        Ok(())
    }

    /// One entry, by the damage rules. Copy 0 answers whenever it can -- a
    /// value whose checksum holds, in a tree whose pages read -- exactly as a
    /// page answers once its own checksum holds. Only when copy 0 is damaged
    /// do copies 1 and 2 answer, and they must then agree. Whether the three
    /// copies agree when all are intact is the verifier's question
    /// (`supportive::verify`), not every read's. An ignorable entry that is
    /// damaged reads as missing: it is rebuilt, never a reason to refuse.
    pub(crate) fn get(&self, store: &(impl Trees + ?Sized), key: &Key) -> Result<Option<(u8, Vec<u8>)>> {
        let k = key.encode()?;
        match store.tree_get(REGISTER_TREES[0], self.roots[0], &k) {
            Ok(Some(v)) => {
                if let Ok((version, payload)) = decode_value(&k, &v) {
                    return Ok(Some((version, payload.to_vec())));
                }
            }
            Ok(None) => return Ok(None),
            Err(Error::Kernel(kernel::Error::Corrupt { .. })) => {}
            Err(e) => return Err(e),
        }
        if !key.kind.critical() {
            return Ok(None);
        }
        let mut intact: Option<Vec<u8>> = None;
        let mut present = false;
        for i in 1..3 {
            let Some(v) = store.tree_get(REGISTER_TREES[i], self.roots[i], &k)? else { continue };
            present = true;
            if decode_value(&k, &v).is_err() {
                continue;
            }
            match &intact {
                Some(seen) if *seen != v => return Err(corrupt("Register copies disagree")),
                Some(_) => {}
                None => intact = Some(v),
            }
        }
        match intact {
            Some(v) => {
                let (version, payload) = decode_value(&k, &v)?;
                Ok(Some((version, payload.to_vec())))
            }
            None if present => Err(corrupt("every Register copy of a critical entry is damaged")),
            None => Ok(None),
        }
    }

    pub(crate) fn delete(&mut self, store: &mut impl TreesMut, key: &Key) -> Result<bool> {
        let k = key.encode()?;
        let mut found = false;
        for i in 0..Self::copies(key) {
            let (yes, root) = store.tree_delete(REGISTER_TREES[i], self.roots[i], &k)?;
            self.roots[i] = root;
            found |= yes;
        }
        Ok(found)
    }

    /// Every entry whose key starts with `prefix` (see [`Key::prefix`]), in
    /// key order, read by the damage rules.
    pub(crate) fn scan_prefix(&self, store: &(impl Trees + ?Sized), prefix: &[u8]) -> Result<Vec<(Key, u8, Vec<u8>)>> {
        // One walk of copy 0; an entry whose value there is damaged is read
        // again by the damage rules.
        let mut out = Vec::new();
        let mut again = Vec::new();
        store.tree_scan(REGISTER_TREES[0], self.roots[0], prefix, &mut |k, v| {
            if !k.starts_with(prefix) {
                return Ok(false);
            }
            let key = Key::decode(k)?;
            match decode_value(k, v) {
                Ok((version, payload)) => out.push((key, version, payload.to_vec())),
                Err(_) => again.push(key),
            }
            Ok(true)
        })?;
        if !again.is_empty() {
            for key in again {
                if let Some((version, payload)) = self.get(store, &key)? {
                    out.push((key, version, payload));
                }
            }
            out.sort_by_cached_key(|e| e.0.encode().unwrap_or_default());
        }
        Ok(out)
    }

    /// Every entry of one node, in key order, read by the damage rules.
    /// Copy 0 holds every entry, critical and ignorable alike.
    pub(crate) fn scan_node(&self, store: &PageWalStore, node: Node) -> Result<Vec<(Key, u8, Vec<u8>)>> {
        let prefix = [node_byte(node)];
        let mut keys = Vec::new();
        if let Some(iter) = store.tree_range(REGISTER_TREES[0], self.roots[0], &prefix)? {
            for record in iter {
                let (k, _) = record?;
                if k.first() != Some(&prefix[0]) {
                    break;
                }
                keys.push(Key::decode(&k)?);
            }
        }
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some((version, payload)) = self.get(store, &key)? {
                out.push((key, version, payload));
            }
        }
        Ok(out)
    }
}

fn node_byte(node: Node) -> u8 {
    b'a' + node as u8
}
