//! The 0.18 -> 0.19 format move (`docs/core/SUPPORTIVE.md` section 3), as a
//! library function: embedded users have no command line, and
//! `sekejap-upgrade --apply` wraps it.
//!
//! The move builds BESIDE the source. `<db>.v019-upgrading` is a rebuild of
//! `<db>` into a Register file ([`super::rebuild::upgrade_to_register`]):
//! rows, vectors and edges copied byte for byte, every supportive fact
//! translated, every index rebuilt, the result verified independently and
//! compared with the source. Only then two renames publish it:
//!
//! 1. `<db>` -> `<db>.v018-backup` (the original, never written: the backup
//!    costs no copy);
//! 2. `<db>.v019-upgrading` -> `<db>`.
//!
//! | State after a crash | Recognised by | This function then |
//! |---|---|---|
//! | building | `<db>` is 0.18, `<db>.v019-upgrading` has the rebuild's incomplete marker | deletes the partial build and starts again |
//! | between the renames | no `<db>`, a backup, a complete `<db>.v019-upgrading` | finishes the second rename |
//! | done | `<db>` is a Register file | nothing |

use super::rebuild::{upgrade_to_register, RebuildLimits, RebuildReport};
use super::*;
use crate::pagewal::{CurrentReaderLimits, CurrentSourceReader};
use std::fs;

/// What an upgrade did.
#[derive(Debug)]
pub struct FormatUpgrade {
    /// The original 0.18 directory, untouched.
    pub backup: PathBuf,
    /// The rebuild's report; `None` when this call only finished the rename
    /// an earlier, interrupted call had not.
    pub report: Option<RebuildReport>,
}

fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("an upgrade needs a database directory, not a root"))?;
    let mut name = name.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("a database directory has a parent"))?;
    kernel::io::sync_directory(parent).map_err(Error::from)
}

/// Whether the database at `path` is in the 0.18 format: its header keys
/// hold a 0.18 header rather than the Anchor. Read-only.
pub fn is_legacy_format(path: impl AsRef<Path>) -> Result<bool> {
    let reader = CurrentSourceReader::open(path.as_ref(), CurrentReaderLimits::default())
        .map_err(Error::from)?;
    Ok(!crate::supportive::header::anchored(&reader)?)
}

/// Move the database at `path` to the 0.19 format. `Ok(None)` when it is
/// already a Register file, or when `path` holds no database yet.
pub fn upgrade_format(path: impl AsRef<Path>, limits: RebuildLimits) -> Result<Option<FormatUpgrade>> {
    let path = path.as_ref();
    let staging = sibling(path, ".v019-upgrading")?;
    let backup = sibling(path, ".v018-backup")?;
    if !path.exists() && backup.exists() && staging.join("COMPLETE").exists() {
        fs::rename(&staging, path).map_err(kernel::Error::from)?;
        sync_parent(path)?;
        return Ok(Some(FormatUpgrade { backup, report: None }));
    }
    // No database here yet: a missing folder, or one the application made
    // empty before its first open. Nothing to move.
    if !path.join("data").exists() && !path.join("wal").exists() {
        return Ok(None);
    }
    if !is_legacy_format(path)? {
        return Ok(None);
    }
    if backup.exists() {
        return Err(invalid(format!(
            "{} exists and is not this upgrade's backup; move it away first",
            backup.display()
        )));
    }
    if staging.exists() {
        // Only a build this function started is deleted: it carries the
        // rebuild's own marker.
        if !staging.join("REBUILD_INCOMPLETE").exists() && !staging.join("COMPLETE").exists() {
            return Err(invalid(format!(
                "{} exists and is not an upgrade in progress; move it away first",
                staging.display()
            )));
        }
        fs::remove_dir_all(&staging).map_err(kernel::Error::from)?;
        sync_parent(&staging)?;
    }
    let report = upgrade_to_register(path, &staging, limits)?;
    fs::rename(path, &backup).map_err(kernel::Error::from)?;
    sync_parent(path)?;
    fs::rename(&staging, path).map_err(kernel::Error::from)?;
    sync_parent(path)?;
    Ok(Some(FormatUpgrade { backup, report: Some(report) }))
}

#[cfg(test)]
#[path = "upgrade_tests.rs"]
mod upgrade_tests;
