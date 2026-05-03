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
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// Input for [`crate::cache::Cache::is_composite_fresh`]. Grouped into
/// a struct because the four fields must stay in sync — passing them
/// positionally invited bugs in the rustc-entry equivalent.
#[derive(Debug)]
pub struct CompositeQuery<'a> {
    /// Caller-chosen manifest key.
    pub key: &'a str,
    /// File inputs whose fingerprints participate in freshness.
    /// Iteration order is irrelevant (compared as a set), but the
    /// slice's order is used when refreshing fingerprints on the
    /// slow path.
    pub inputs: &'a [PathBuf],
    /// Optional opaque digest of non-file inputs (def bytes, tool
    /// versions). `None` means "no aux state to compare"; in that
    /// case the entry's stored `aux_hash` must also be `None`.
    pub aux_hash: Option<[u8; 32]>,
    /// Artefact the caller expects to find; absence ⇒ stale.
    pub output_path: &'a Path,
}

/// Input for [`crate::cache::Cache::mark_composite_built`]. Same
/// rationale as [`CompositeQuery`].
#[derive(Debug)]
pub struct CompositeRecord {
    pub key: String,
    pub inputs: Vec<PathBuf>,
    pub aux_hash: Option<[u8; 32]>,
    pub output_path: PathBuf,
}

/// Domain-separated SHA-256 over (tag, len-prefixed segments...).
///
/// Callers use this to roll non-file inputs (model def bytes, tool
/// version strings) into a single 32-byte `aux_hash`. The domain tag
/// is `gluon.cache.composite.v1\0`. Each segment is length-prefixed
/// with `u64::to_le_bytes` so concatenation ambiguity (`"foo"+"bar"`
/// vs `"foob"+"ar"`) cannot collide.
///
/// The algorithm is frozen. Bumping the domain tag would invalidate
/// every existing composite cache entry's `aux_hash` and force a
/// full rebuild for that entry kind.
pub fn composite_aux_hash(segments: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gluon.cache.composite.v1\0");
    hasher.update((segments.len() as u64).to_le_bytes());
    for seg in segments {
        hasher.update((seg.len() as u64).to_le_bytes());
        hasher.update(seg);
    }
    hasher.finalize().into()
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

    #[test]
    fn composite_query_construction() {
        let inputs = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let out = PathBuf::from("/out");
        let q = CompositeQuery {
            key: "esp:default:EFI/BOOT/X.EFI",
            inputs: &inputs,
            aux_hash: Some([0; 32]),
            output_path: &out,
        };
        assert_eq!(q.key, "esp:default:EFI/BOOT/X.EFI");
        assert_eq!(q.inputs.len(), 2);
    }

    #[test]
    fn composite_record_construction() {
        let r = CompositeRecord {
            key: "image:boot".into(),
            inputs: vec![PathBuf::from("/a")],
            aux_hash: None,
            output_path: PathBuf::from("/out.img"),
        };
        assert_eq!(r.key, "image:boot");
        assert!(r.aux_hash.is_none());
    }

    #[test]
    fn composite_aux_hash_stable_for_identical_inputs() {
        let a = composite_aux_hash(&[b"image:boot", b"hadris-fat=0.5.0"]);
        let b = composite_aux_hash(&[b"image:boot", b"hadris-fat=0.5.0"]);
        assert_eq!(a, b);
    }

    #[test]
    fn composite_aux_hash_length_prefix_collision_resistance() {
        // ["foo", "bar"] must not collide with ["foob", "ar"].
        let a = composite_aux_hash(&[b"foo", b"bar"]);
        let b = composite_aux_hash(&[b"foob", b"ar"]);
        assert_ne!(a, b);
    }

    #[test]
    fn composite_aux_hash_segment_count_matters() {
        // Empty trailing segment is not the same as no trailing segment.
        let a = composite_aux_hash(&[b"foo"]);
        let b = composite_aux_hash(&[b"foo", b""]);
        assert_ne!(a, b);
    }
}
