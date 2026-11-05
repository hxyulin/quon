//! Build cache: fingerprint records, depfile parsing, hashing and the
//! build-directory lock.
//!
//! See `docs/src/architecture/cache.md` for the freshness rule and
//! [`store`] for its implementation.

pub mod depinfo;
pub mod hash;
pub mod lock;
pub mod store;

pub use depinfo::{DepInfo, parse_depfile};
pub use hash::{hash_argv, hash_parts, sha256_bytes, sha256_file};
pub use lock::CacheLock;
pub use store::{
    FileStamp, Fingerprint, FingerprintStore, Freshness, FreshnessQuery, RecordInput, StaleReason,
};

use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use std::path::Path;

/// Remove the cache of Gluon versions that kept every record in one
/// `build/cache-manifest.json`, together with the artifact trees it
/// described. Returns whether anything was removed.
///
/// Those trees used other file names and directories, and their
/// records cannot be converted into fingerprint records (they lack the
/// declared/discovered split and the toolchain hash). Deleting them
/// costs one clean build per checkout. `final/` and `esp/` directories
/// stay, because QEMU and users refer to their files between builds;
/// the next build overwrites them. The sysroot stays on disk but has no
/// record, so it rebuilds too.
pub fn remove_legacy_cache(layout: &BuildLayout) -> Result<bool> {
    let root = layout.root();
    let manifest = layout.legacy_cache_manifest();
    if !manifest.exists() {
        return Ok(false);
    }
    remove_path(&root.join("host"))?;
    remove_path(&root.join("tool"))?;
    let cross = root.join("cross");
    for target in read_dir(&cross)? {
        for profile in read_dir(&target)? {
            for entry in read_dir(&profile)? {
                let keep = entry
                    .file_name()
                    .is_some_and(|n| n == "final" || n == "esp");
                if !keep {
                    remove_path(&entry)?;
                }
            }
        }
    }
    let mut lock = manifest.as_os_str().to_owned();
    lock.push(".lock");
    remove_path(Path::new(&lock))?;
    remove_path(&manifest)?;
    Ok(true)
}

/// Subdirectories of `dir`, sorted; empty when `dir` does not exist.
fn read_dir(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| io_err(dir, e))?.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Remove a file or directory tree; a missing path is not an error.
pub(crate) fn remove_path(path: &Path) -> Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path, e)),
    }
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
