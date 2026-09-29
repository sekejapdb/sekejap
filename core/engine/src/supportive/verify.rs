//! The Register verifier (`docs/core/SUPPORTIVE.md` 2.0.3, 3.d): reads every
//! copy of every entry and reports what it found, before anything trusts it.

use super::carrier::{decode_value, Anchor, Key, REGISTER_TREES};
use super::register::Register;
use crate::collections::{corrupt, Error};
use crate::store::Backend;

type Result<T> = std::result::Result<T, Error>;

/// What a clean verification saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) critical: u64,
    pub(crate) ignorable: u64,
}

/// Every entry: each copy's checksum; critical copies all present and equal;
/// ignorable entries in copy 0 only; every (kind, version) in the census.
/// The first problem found is returned as corruption, naming it.
pub(crate) fn verify(store: &Backend, register: &Register, anchor: &Anchor) -> Result<Verified> {
    let roots = register.roots();
    let mut seen = Verified::default();
    // Copy 0 holds every entry: walk it in order, and check each critical
    // entry's other two copies by key -- streaming, never the whole Register.
    if let Some(iter) = store.tree_range(REGISTER_TREES[0], roots[0], &[])? {
        for record in iter {
            let (k, v) = record?;
            let key = Key::decode(&k)?;
            let name = || format!("{} {:?}", key.kind.code(), key.item);
            let (version, _) = decode_value(&k, &v).map_err(|_| corrupt(format!("Register entry {}: copy 0 damaged", name())))?;
            if !anchor.census.iter().any(|l| l.kind == key.kind && l.version == version) {
                return Err(corrupt(format!("Register entry {} version {version} is not in the census", name())));
            }
            if key.kind.critical() {
                for copy in 1..3 {
                    match store.tree_get(REGISTER_TREES[copy], roots[copy], &k)? {
                        None => return Err(corrupt(format!("Register entry {}: copy {copy} missing", name()))),
                        Some(other) if other != v => {
                            return Err(corrupt(if decode_value(&k, &other).is_ok() {
                                format!("Register entry {}: copies 0 and {copy} disagree", name())
                            } else {
                                format!("Register entry {}: copy {copy} damaged", name())
                            }))
                        }
                        Some(_) => {}
                    }
                }
                seen.critical += 1;
            } else {
                seen.ignorable += 1;
            }
        }
    }
    // Copies 1 and 2 hold critical entries only, each also in copy 0.
    for copy in 1..3 {
        if let Some(iter) = store.tree_range(REGISTER_TREES[copy], roots[copy], &[])? {
            for record in iter {
                let (k, _) = record?;
                let key = Key::decode(&k)?;
                if !key.kind.critical() {
                    return Err(corrupt(format!("ignorable entry {} found in copy {copy}", key.kind.code())));
                }
                if store.tree_get(REGISTER_TREES[0], roots[0], &k)?.is_none() {
                    return Err(corrupt(format!("copy {copy} holds entry {} that copy 0 lacks", key.kind.code())));
                }
            }
        }
    }
    Ok(seen)
}
