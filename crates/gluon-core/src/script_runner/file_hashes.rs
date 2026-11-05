//! Persistent SHA-256 memo for the script runner's cache keys.
//!
//! Every gluon invocation recomputes the rlib cache key (all
//! gluon-config and gluon-model sources plus `Cargo.lock`) and the
//! script cache key (the script plus the gluon-config rlib and rmeta,
//! about 1.2 MB). Hashing them all dominated a no-op build. This memo
//! applies the build cache's two-tier rule: a file whose mtime and size
//! match the recorded ones returns the recorded hash; any mismatch
//! rehashes the file and records the new fingerprint.
//!
//! Freshness rule: same as [`crate::cache::Cache::is_fresh`]. An edit
//! that keeps both the nanosecond mtime and the size is not detected.
//! A missing, unreadable or corrupt memo file is treated as empty.

use crate::cache::{FileFingerprint, sha256_file};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(crate) struct FileHashes {
    path: PathBuf,
    entries: BTreeMap<PathBuf, FileFingerprint>,
    dirty: bool,
}

impl FileHashes {
    /// Load the memo stored at `path`, or start empty.
    pub(crate) fn load(path: PathBuf) -> Self {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            entries,
            dirty: false,
        }
    }

    /// SHA-256 of `file`, from the memo when its mtime and size match.
    pub(crate) fn hash(&mut self, file: &Path) -> Result<[u8; 32]> {
        let meta = std::fs::metadata(file).map_err(|e| Error::Io {
            path: file.to_path_buf(),
            source: e,
        })?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());
        if let (Some(mtime_ns), Some(fp)) = (mtime_ns, self.entries.get(file))
            && fp.mtime_ns == mtime_ns
            && fp.size == meta.len()
            && let Some(sha) = fp.sha256
        {
            return Ok(sha);
        }

        let sha = sha256_file(file).map_err(|e| match e {
            Error::Io { source, .. } => Error::Io {
                path: file.to_path_buf(),
                source,
            },
            other => other,
        })?;
        // Without a usable mtime the fast path can never match, so there
        // is nothing worth recording.
        if let Some(mtime_ns) = mtime_ns {
            self.entries.insert(
                file.to_path_buf(),
                FileFingerprint {
                    mtime_ns,
                    size: meta.len(),
                    sha256: Some(sha),
                },
            );
            self.dirty = true;
        }
        Ok(sha)
    }

    /// Write the memo if any entry changed. Written to a temp file and
    /// renamed so a concurrent reader never sees a partial file.
    pub(crate) fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let bytes = serde_json::to_vec(&self.entries)
            .map_err(|e| Error::Config(format!("serialize file hash memo: {e}")))?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|e| Error::Io {
            path: tmp.clone(),
            source: e,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn unchanged_mtime_and_size_skip_rehash() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.rs");
        fs::write(&f, b"first").unwrap();
        let mut memo = FileHashes::load(tmp.path().join("memo.json"));
        let h1 = memo.hash(&f).unwrap();
        assert_eq!(h1, sha256_file(&f).unwrap());

        // Same size, same mtime, different bytes: the memo must answer
        // from its record without reading the file.
        let mtime = fs::metadata(&f).unwrap().modified().unwrap();
        fs::write(&f, b"other").unwrap();
        set_mtime(&f, mtime);
        assert_eq!(memo.hash(&f).unwrap(), h1);
    }

    #[test]
    fn changed_mtime_rehashes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.rs");
        fs::write(&f, b"first").unwrap();
        let mut memo = FileHashes::load(tmp.path().join("memo.json"));
        let h1 = memo.hash(&f).unwrap();

        fs::write(&f, b"other").unwrap();
        set_mtime(
            &f,
            std::time::SystemTime::now() + std::time::Duration::from_secs(5),
        );
        let h2 = memo.hash(&f).unwrap();
        assert_ne!(h1, h2);
        assert_eq!(h2, sha256_file(&f).unwrap());
    }

    #[test]
    fn memo_persists_across_load() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.rs");
        let memo_path = tmp.path().join("memo.json");
        fs::write(&f, b"first").unwrap();
        let mut memo = FileHashes::load(memo_path.clone());
        let h1 = memo.hash(&f).unwrap();
        memo.save().unwrap();

        let mtime = fs::metadata(&f).unwrap().modified().unwrap();
        fs::write(&f, b"other").unwrap();
        set_mtime(&f, mtime);
        let mut reloaded = FileHashes::load(memo_path);
        assert_eq!(reloaded.hash(&f).unwrap(), h1);
    }
}
