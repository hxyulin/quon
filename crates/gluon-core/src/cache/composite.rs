//! Composite cache entries — for non-rustc artifacts (sysroot, ESP, image).
//!
//! A `CompositeEntry` records a list of input file paths with their
//! fingerprints, an opaque `aux_hash` capturing non-file inputs (model
//! definition bytes, tool version strings, etc.), and the produced
//! artifact's `output_path`. The freshness algorithm mirrors the
//! rustc-entry algorithm in `Cache::is_fresh` minus the `argv_hash`
//! check, plus an `aux_hash` equality check — see the module docs for
//! the full algorithm.

use crate::cache::manifest::FileFingerprint;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One composite cache entry. Keyed in [`CacheManifest::composite`] by a
/// caller-chosen string (e.g. `"sysroot:x86_64-unknown-none"`,
/// `"esp:default:EFI/BOOT/BOOTX64.EFI"`, `"image:bootimg"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEntry {
    /// Per-input file fingerprint. `BTreeMap` so iteration order is
    /// deterministic for both the freshness check and serialisation.
    pub inputs: BTreeMap<PathBuf, FileFingerprint>,
    /// Opaque digest of any non-file inputs (model def bytes, tool
    /// version strings, etc.). The cache treats this as a single blob
    /// and only checks for equality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux_hash: Option<[u8; 32]>,
    /// Artefact this entry asserts is built. `is_composite_fresh`
    /// requires this file to exist for the entry to count as fresh.
    pub output_path: PathBuf,
    /// Wall-clock build time, nanoseconds since `UNIX_EPOCH`.
    /// Informational; not used in freshness checks.
    pub built_at: u128,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> CompositeEntry {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            PathBuf::from("/some/source.efi"),
            FileFingerprint {
                mtime_ns: 12_345,
                size: 4096,
                sha256: Some([0xCD; 32]),
            },
        );
        CompositeEntry {
            inputs,
            aux_hash: Some([0xAB; 32]),
            output_path: PathBuf::from("/build/out.img"),
            built_at: 999_999,
        }
    }

    #[test]
    fn composite_entry_round_trips_through_json() {
        let e = sample_entry();
        let json = serde_json::to_string(&e).unwrap();
        let de: CompositeEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(de, e);
    }

    #[test]
    fn composite_entry_serialises_keys_in_sorted_order() {
        let mut inputs: BTreeMap<PathBuf, FileFingerprint> = BTreeMap::new();
        for k in ["/z.bin", "/a.bin", "/m.bin"] {
            inputs.insert(
                PathBuf::from(k),
                FileFingerprint { mtime_ns: 0, size: 0, sha256: None },
            );
        }
        let e = CompositeEntry {
            inputs,
            aux_hash: None,
            output_path: PathBuf::from("/o"),
            built_at: 0,
        };
        let json = serde_json::to_string(&e).unwrap();
        let a_pos = json.find("/a.bin").unwrap();
        let m_pos = json.find("/m.bin").unwrap();
        let z_pos = json.find("/z.bin").unwrap();
        assert!(a_pos < m_pos && m_pos < z_pos, "keys not sorted: {json}");
    }
}
