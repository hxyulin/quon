//! Image-build freshness stamp.
//!
//! Mirrors `scheduler::helpers::esp::EspStamp`: a sidecar JSON file
//! listing every input fingerprint, atomic-rename-written so an
//! interrupted build doesn't leave the cache lying about freshness.

use crate::error::{Error, Result};
use gluon_model::ImageDef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use super::ResolvedEntry;
use super::source::StampSource;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct StampEntry {
    pub dest_path: String,
    pub source: StampSource,
    pub mtime_ns: i128,
    pub size: u64,
}

/// On-disk freshness record for one image.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ImageStamp {
    /// Hash of the serialised `ImageDef`. Any change to format, size,
    /// or entry list invalidates the cache.
    pub def_hash: String,
    /// Locked-in version of the FAT writer dependency.
    pub hadris_fat_version: String,
    /// Per-entry source fingerprints, keyed by dest_path so the
    /// JSON layout is human-diffable.
    pub entries: BTreeMap<String, StampEntry>,
}

impl ImageStamp {
    pub fn compute(
        image: &ImageDef,
        entries: &[ResolvedEntry],
        hadris_version: &str,
    ) -> Result<Self> {
        // Serialise the ImageDef (without span — span carries source
        // positions that are noisy but don't affect output bytes).
        let mut def_for_hash = image.clone();
        def_for_hash.span = None;
        let bytes = serde_json::to_vec(&def_for_hash).map_err(|e| {
            Error::Compile(format!(
                "image('{}'): failed to serialise ImageDef for cache key: {e}",
                image.name
            ))
        })?;
        let def_hash = sha256_hex(&bytes);

        let mut stamp_entries: BTreeMap<String, StampEntry> = BTreeMap::new();
        for e in entries {
            let (mtime_ns, size) = source_fingerprint(&e.source_path)?;
            stamp_entries.insert(
                e.dest_path.clone(),
                StampEntry {
                    dest_path: e.dest_path.clone(),
                    source: e.source_kind.clone(),
                    mtime_ns,
                    size,
                },
            );
        }

        Ok(Self {
            def_hash,
            hadris_fat_version: hadris_version.into(),
            entries: stamp_entries,
        })
    }

    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            Error::Compile(format!(
                "image stamp path '{}' has no parent directory",
                path.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Compile(format!("image stamp serialize failed: {e}")))?;
        // Atomic write: temp + rename. Same pattern as EspStamp.
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.write_all(&bytes).map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, path).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Ok(())
    }
}

/// `(mtime_ns, size)` for a regular file. For ESP-source directories
/// we hash the full subtree's `(relpath, mtime, size)` triples into a
/// single fingerprint — the `size` field becomes the entry count and
/// `mtime_ns` becomes the latest-modified timestamp inside the tree.
/// Coarse but cheap.
fn source_fingerprint(path: &Path) -> Result<(i128, u64)> {
    let md = std::fs::metadata(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if md.is_dir() {
        // Walk the tree and aggregate. We sum sizes and take the max
        // mtime. The max-mtime form is intentionally lossy: a file
        // that gets *deleted* doesn't bump it, but the file count
        // change will.
        let mut total_size: u64 = 0;
        let mut count: u64 = 0;
        let mut max_mtime: i128 = 0;
        for entry in walkdir::WalkDir::new(path).sort_by_file_name() {
            let entry = entry.map_err(|e| Error::Compile(format!("walkdir: {e}")))?;
            if entry.file_type().is_file() {
                count += 1;
                let ml = entry
                    .metadata()
                    .map_err(|e| Error::Compile(format!("metadata: {e}")))?;
                total_size = total_size.saturating_add(ml.len());
                if let Ok(t) = ml.modified() {
                    let ns = systemtime_to_ns(t);
                    if ns > max_mtime {
                        max_mtime = ns;
                    }
                }
            }
        }
        // Encode count into the upper 32 bits of size so a count change
        // is visible in the stamp diff even when total bytes match.
        Ok((max_mtime, (count << 32) | (total_size & 0xFFFF_FFFF)))
    } else {
        let mtime_ns = md.modified().ok().map(systemtime_to_ns).unwrap_or(0);
        Ok((mtime_ns, md.len()))
    }
}

fn systemtime_to_ns(t: std::time::SystemTime) -> i128 {
    use std::time::UNIX_EPOCH;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{:02x}", b));
    }
    s
}
