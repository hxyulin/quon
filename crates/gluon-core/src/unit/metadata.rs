//! The unit metadata hash: `-C metadata` and `-C extra-filename`.
//!
//! The hash names *which* artifact a unit is, so that units which must
//! coexist (two versions of a crate, a package on host and target, two
//! feature sets, two config presets) get different StableCrateIds and
//! different file names. It changes exactly when the output is a
//! different artifact that could coexist or alternate with the old one.
//! Inputs that only change the content of the same artifact (sources,
//! flags, edition, the toolchain) stay out: the cache rebuilds those in
//! place, so no garbage piles up.
//!
//! What goes in, in this order, each field length-prefixed:
//!
//! - package name, version, and source (path relative to the project
//!   root, or `registry`), so two versions or two sources coexist;
//! - unit kind and crate name, so a lib and a bin of one package do;
//! - platform: `host`, or the target name plus `builtin:<triple>` or
//!   `spec:<path relative to the project root>`;
//! - features, sorted;
//! - the dependencies' identities, sorted: their metadata hashes, or the
//!   file name of a cargo-built prebuilt artifact;
//! - the config crate hash, for units that link the config crate, so
//!   each preset or override combination keeps its own artifacts.
//!
//! What stays out: the profile (a directory level instead), the
//! toolchain (the freshness record's job), cfg and rustc flags, lints,
//! the edition, the linker script, target-spec contents, the driver
//! (`tool/check/` and `tool/clippy/` are separate roots), and every
//! absolute path, so the build directory can move.

use super::{BuildUnit, DepTarget, PackageSource, Platform};
use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateType, Handle, ResolvedConfig, TargetDef};
use sha2::{Digest, Sha256};
use std::path::Path;

/// A unit's metadata hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Metadata([u8; 32]);

impl Metadata {
    /// Full lowercase hex, passed as `-C metadata=`.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// First 16 hex digits, used in file names (`-C extra-filename=-<short>`),
    /// incremental directories and cache keys.
    pub fn short(&self) -> String {
        self.hex()[..16].to_string()
    }
}

impl std::fmt::Debug for Metadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Metadata({})", self.short())
    }
}

/// SHA-256 over a domain tag and length-prefixed fields. Length
/// prefixes keep `("ab", "c")` and `("a", "bc")` apart.
struct FieldHasher(Sha256);

impl FieldHasher {
    fn new(domain: &str) -> Self {
        let mut h = Sha256::new();
        h.update(domain.as_bytes());
        h.update([0]);
        Self(h)
    }

    fn field(&mut self, bytes: impl AsRef<[u8]>) -> &mut Self {
        let bytes = bytes.as_ref();
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    fn finish(self) -> Metadata {
        Metadata(self.0.finalize().into())
    }
}

/// `p` relative to the project root when it lies under it, else as
/// given. Keeps absolute paths out of the hash.
fn project_relative(p: &Path, project_root: &Path) -> String {
    p.strip_prefix(project_root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// Platform identity: `host`, or `<target name>` plus `builtin:<triple>`
/// or `spec:<relative path>`.
pub(crate) fn platform_identity(
    platform: Platform,
    model: &BuildModel,
    resolved: &ResolvedConfig,
) -> Result<String> {
    match platform {
        Platform::Host => Ok("host".into()),
        Platform::Cross(t) => {
            let t = target(model, t)?;
            let spec = if t.builtin {
                format!("builtin:{}", t.spec)
            } else {
                format!(
                    "spec:{}",
                    project_relative(Path::new(&t.spec), &resolved.project_root)
                )
            };
            Ok(format!("{}\0{spec}", t.name))
        }
    }
}

fn target(model: &BuildModel, t: Handle<TargetDef>) -> Result<&TargetDef> {
    model.targets.get(t).ok_or_else(|| {
        Error::Compile(format!(
            "metadata hash: target handle {t:?} not found in build model"
        ))
    })
}

/// Hash of one target's generated config crate: its crate name, the
/// target, and the rendered source (the resolved config values). Used
/// as the config crate's own `-C metadata` and as an input to every
/// unit that links it.
pub(crate) fn config_crate_metadata(crate_name: &str, platform: &str, source: &str) -> Metadata {
    let mut h = FieldHasher::new("gluon.config.v1");
    h.field(crate_name).field(platform).field(source);
    h.finish()
}

fn kind_label(kind: CrateType) -> &'static str {
    match kind {
        CrateType::Lib => "lib",
        CrateType::Bin => "bin",
        CrateType::ProcMacro => "proc-macro",
        CrateType::StaticLib => "staticlib",
        CrateType::RawBinary => "raw-binary",
    }
}

/// Metadata of `unit`. `dep_metadata` returns the already computed hash
/// of a dep unit; `config` is the config crate hash of the unit's
/// target, required when `unit.config_crate` is set.
pub(crate) fn unit_metadata(
    unit: &BuildUnit,
    platform: &str,
    project_root: &Path,
    dep_metadata: impl Fn(super::UnitId) -> Metadata,
    config: Option<Metadata>,
) -> Metadata {
    let mut h = FieldHasher::new("gluon.unit.v2");
    h.field(&unit.package.name).field(&unit.package.version);
    match &unit.package.source {
        PackageSource::Path(p) => h.field(format!("path:{}", project_relative(p, project_root))),
        PackageSource::Registry => h.field("registry"),
    };
    h.field(kind_label(unit.kind)).field(&unit.crate_name);
    h.field(platform);

    let mut features: Vec<&str> = unit.features.iter().map(String::as_str).collect();
    features.sort_unstable();
    features.dedup();
    h.field((features.len() as u64).to_le_bytes());
    for f in features {
        h.field(f);
    }

    let mut deps: Vec<String> = unit
        .deps
        .iter()
        .map(|d| match &d.target {
            DepTarget::Unit(u) => format!("unit:{}", dep_metadata(*u).hex()),
            // Cargo's file name already carries its own metadata hash.
            DepTarget::Prebuilt { path, .. } => format!(
                "prebuilt:{}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            DepTarget::Missing { package } => format!("missing:{package}"),
        })
        .collect();
    deps.sort_unstable();
    h.field((deps.len() as u64).to_le_bytes());
    for d in &deps {
        h.field(d);
    }

    match config {
        Some(c) => h.field(c.0),
        None => h.field([]),
    };
    h.finish()
}
