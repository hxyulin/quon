//! `StampSource` and friends — was in `image/stamp.rs`, lifted out
//! so the rest of the stamp module can be deleted as part of the
//! Stage 5 cache unification (the stamp file goes away; this enum
//! survives because it's also used by `image/fat.rs` and the
//! `ResolvedEntry` struct in `image/mod.rs` to tag entry sources).

use serde::{Deserialize, Serialize};

/// Tag identifying where an image entry's source came from. After
/// Stage 5 the stamp file is gone, but this tag is folded into the
/// record's command hash so a crate→file rename in
/// `gluon.rs` is still visible even if the resolved on-disk path
/// happens to match.
///
/// Uses struct variants (rather than newtype-around-String) because
/// serde_json's internally-tagged representation can't fold a tag
/// into a primitive payload — the `name`/`path` field gives the
/// JSON a place to land alongside `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StampSource {
    Crate { name: String },
    EspDir { name: String },
    File { path: String },
    Download { name: String },
}

impl StampSource {
    /// Encode the variant as a stable byte string for inclusion in a
    /// command hash. The encoding is `<tag>\0<payload>` —
    /// length-prefixing happens at the `hash_parts` level.
    /// Used by `image/mod.rs` when building the image's command hash
    /// from the per-entry source kinds.
    pub fn aux_bytes(&self) -> Vec<u8> {
        match self {
            StampSource::Crate { name } => {
                let mut v = b"crate\0".to_vec();
                v.extend_from_slice(name.as_bytes());
                v
            }
            StampSource::EspDir { name } => {
                let mut v = b"esp_dir\0".to_vec();
                v.extend_from_slice(name.as_bytes());
                v
            }
            StampSource::File { path } => {
                let mut v = b"file\0".to_vec();
                v.extend_from_slice(path.as_bytes());
                v
            }
            StampSource::Download { name } => {
                let mut v = b"download\0".to_vec();
                v.extend_from_slice(name.as_bytes());
                v
            }
        }
    }
}
