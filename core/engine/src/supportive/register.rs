//! 2.0.3 The Register, persisted: three B-trees with fixed ids, one copy of
//! every critical entry in each and ignorable entries in copy 0 only, with
//! the damage rules of `docs/core/SUPPORTIVE.md` 2.0.3.

use super::carrier::{decode_value, encode_value, Key, Node, REGISTER_TREES};
use crate::collections::{corrupt, Error};
use crate::store::Backend;

type Result<T> = std::result::Result<T, Error>;

/// The Register of one open database: the three copies' roots. The roots are
/// saved in the Anchor by the caller, in the same commit as any write here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Register {
    roots: [u32; 3],
}

impl Register {
    /// Three empty trees, one per copy, under the fixed tree ids.
    pub(crate) fn create(store: &mut Backend) -> Result<Self> {
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

    pub(crate) fn put(&mut self, store: &mut Backend, key: &Key, version: u8, payload: &[u8]) -> Result<()> {
        let k = key.encode()?;
        let v = encode_value(&k, version, payload)?;
        for i in 0..Self::copies(key) {
            self.roots[i] = store.tree_put(REGISTER_TREES[i], self.roots[i], &k, &v)?;
        }
        Ok(())
    }

    /// One entry, by the damage rules: a damaged or missing copy loses to the
    /// intact ones; intact copies that disagree, or no intact copy where one
    /// was written, are corruption. An ignorable entry that is damaged reads
    /// as missing (it is rebuilt, never a reason to refuse the file).
    pub(crate) fn get(&self, store: &Backend, key: &Key) -> Result<Option<(u8, Vec<u8>)>> {
        let k = key.encode()?;
        let copies = Self::copies(key);
        let mut intact: Option<Vec<u8>> = None;
        let mut present = 0;
        for i in 0..copies {
            let Some(v) = store.tree_get(REGISTER_TREES[i], self.roots[i], &k)? else { continue };
            present += 1;
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
            None if present > 0 && copies == 3 => Err(corrupt("every Register copy of a critical entry is damaged")),
            None => Ok(None),
        }
    }

    pub(crate) fn delete(&mut self, store: &mut Backend, key: &Key) -> Result<bool> {
        let k = key.encode()?;
        let mut found = false;
        for i in 0..Self::copies(key) {
            let (yes, root) = store.tree_delete(REGISTER_TREES[i], self.roots[i], &k)?;
            self.roots[i] = root;
            found |= yes;
        }
        Ok(found)
    }

    /// Every entry of one node, in key order, read by the damage rules.
    /// Copy 0 holds every entry, critical and ignorable alike.
    pub(crate) fn scan_node(&self, store: &Backend, node: Node) -> Result<Vec<(Key, u8, Vec<u8>)>> {
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
