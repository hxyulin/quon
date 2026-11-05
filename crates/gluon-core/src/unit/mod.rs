//! The build unit table: one [`BuildUnit`] per rustc invocation for a
//! crate.
//!
//! [`lower`] runs once per build, after `config::resolve` and
//! `build_cross_vendor_context`, and before `build_dag`. It turns the
//! two crate sources the build knows about into one table:
//!
//! - every crate in `ResolvedConfig::crates` (declared in `gluon.rs`),
//! - every build in the [`VendorBuildContext`] (`cfg.dependency()`
//!   crates compiled for a cross target, plus their host proc macros).
//!
//! The scheduler has one compile node per unit, and the command builder
//! reads a unit instead of a `CrateDef` or a `VendorCrateBuild`, so both
//! sources go through one dispatch and one command builder.
//!
//! Every unit gets a [`Metadata`] hash (see [`metadata`]) that rustc
//! receives as `-C metadata`, and that names its files through
//! `-C extra-filename`.
//!
//! ### Profiles
//!
//! Cross units use the active profile. Host units are split:
//!
//! - Proc macros, every vendored host unit, and every host unit they
//!   depend on use the fixed host profile ([`HOST_OPT_LEVEL`],
//!   [`HOST_DEBUG_INFO`]). Switching between `dev` and `release` does
//!   not rebuild them, and their output directory is
//!   profile-independent. They are compiled with plain rustc and full
//!   codegen under every driver, because a proc macro links real rlibs
//!   of its deps, so `gluon check` and `gluon build` share them.
//! - Other host units (host tools and build-step runners declared in
//!   `gluon.rs`, and their deps) use the active profile, so a slow host
//!   tool can be optimised. Their output directory carries the profile
//!   name, so each profile keeps its own copy.
//!
//! ### Layout
//!
//! Units of one platform share one `deps/` directory, as in Cargo, and
//! the hash in each file name keeps them apart. rustc finds transitive
//! crates there by name and checks their hash, so a unit needs one
//! `-L dependency=` per platform directory instead of one per dep.
//!
//! ```text
//! <root>/host/deps/                       fixed host profile units
//! <driver root>/host/profile/<p>/deps/    active-profile host units
//! <driver root>/cross/<t>/<p>/deps/       declared cross libraries
//! <driver root>/cross/<t>/<p>/final/      cross executables, unhashed names
//! <root>/cross/<t>/<p>/deps/              vendored cross units, config crate
//! <platform dir>/incremental/<unit>-<h16>/
//! ```
//!
//! `<driver root>` is `<root>` under `gluon build` and
//! `<root>/tool/{check,clippy}` otherwise. Units compiled the same way
//! under every driver ([`UnitSettings::always_rustc`]) live under
//! `<root>` so the drivers share them.
//!
//! [`HOST_OPT_LEVEL`]: crate::compile::HOST_OPT_LEVEL
//! [`HOST_DEBUG_INFO`]: crate::compile::HOST_DEBUG_INFO

pub mod metadata;

pub use metadata::Metadata;

use crate::compile::BuildLayout;
use crate::compile::compile_utils::{
    exe_suffix_for_target, normalize_crate_name, proc_macro_file_name,
};
use crate::error::{Error, Result};
use crate::vendor::cross::context::{VendorBuildContext, VendorBuildId};
use crate::vendor::cross::metadata::CrateKind;
use gluon_model::{
    BuildModel, CrateDef, CrateType, DepResolution, ExternalArtifactKey, Handle, ResolvedConfig,
    TargetDef, TargetSpec,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Index into the unit table ([`UnitGraph::get`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitId(pub u32);

impl UnitId {
    fn index(self) -> usize {
        self.0 as usize
    }
}

/// Where a unit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitOrigin {
    /// A crate declared in `gluon.rs`.
    Crate(Handle<CrateDef>),
    /// A `cfg.dependency()` crate from the cross vendor pipeline.
    Vendor(VendorBuildId),
}

/// The package a unit is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageKey {
    pub name: String,
    /// `0.0.0` for crates declared in `gluon.rs`.
    pub version: String,
    pub source: PackageSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// Crate directory relative to the project root.
    Path(PathBuf),
    /// A vendored crates.io package.
    Registry,
}

/// The platform a unit is compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Host,
    Cross(Handle<TargetDef>),
}

/// Which profile's code-generation flags a unit takes. See the module
/// docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitProfile {
    /// The fixed host profile.
    Host,
    /// The active gluon profile.
    Active,
}

/// One `--extern` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitDep {
    pub extern_name: String,
    pub target: DepTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepTarget {
    /// Another unit in the table.
    Unit(UnitId),
    /// A host artifact built by cargo in `gluon vendor`
    /// (`model.external_artifacts`). Not a node; it exists before the
    /// build starts.
    Prebuilt {
        path: PathBuf,
        /// `-L dependency=` directories for the artifact's own deps.
        search_dirs: Vec<PathBuf>,
    },
    /// An external dep with neither a vendor unit nor a prebuilt
    /// artifact. Lowering keeps going so `gluon configure` still works;
    /// compiling the consumer fails with a "run `gluon vendor`" error.
    Missing { package: String },
}

/// Flags that change an artifact's content but not which artifact it
/// is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitSettings {
    pub cfg_flags: Vec<String>,
    pub rustc_flags: Vec<String>,
    /// `--cap-lints allow`, as Cargo does for non-workspace crates.
    pub cap_lints: bool,
    /// Absolute path of the linker script, for executables.
    pub linker_script: Option<PathBuf>,
    /// Pass `-C incremental=<dir>`.
    pub incremental: bool,
    /// Compile with plain rustc and full codegen under `gluon check` and
    /// `gluon clippy` too, into the driver-independent root. Set for
    /// vendored units (the user cannot fix their lints, and dependents
    /// need real rlibs) and for fixed-host-profile units (a proc macro
    /// links real rlibs of its deps).
    pub always_rustc: bool,
}

/// One rustc invocation for a crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildUnit {
    /// Unique per build; used in labels. The `CrateDef` name for
    /// declared crates, the package name for vendored crates (suffixed
    /// with `@host` or `@<target>` only when the name is taken).
    pub name: String,
    /// `--crate-name`: `-` replaced by `_`.
    pub crate_name: String,
    pub origin: UnitOrigin,
    pub package: PackageKey,
    pub kind: CrateType,
    pub platform: Platform,
    pub profile: UnitProfile,
    /// Links Gluon's sysroot for its target.
    pub sysroot: bool,
    /// Links the generated config crate for its target.
    pub config_crate: bool,
    /// Crate root source file.
    pub src: PathBuf,
    pub edition: String,
    /// Cargo features, emitted as `--cfg feature="<name>"`.
    pub features: Vec<String>,
    /// `--extern` deps, sorted by extern name.
    pub deps: Vec<UnitDep>,
    /// Units that must finish first without being linked
    /// (`artifact_deps`).
    pub order_deps: Vec<UnitId>,
    pub settings: UnitSettings,
    /// Identity hash; see [`metadata`]. Set by [`lower`] once every
    /// dep's hash is known.
    pub metadata: Metadata,
}

/// Where a unit's rustc output lands. Pure path arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitPaths {
    /// `--out-dir`.
    pub out_dir: PathBuf,
    /// Primary artifact: rlib, dylib, binary, or `.bin` for raw
    /// binaries.
    pub output_path: PathBuf,
    pub depfile_path: PathBuf,
    /// `<name>.elf` that rustc writes for a raw binary before objcopy
    /// turns it into `<name>.bin`.
    pub debug_elf_path: Option<PathBuf>,
    /// `.rmeta` file of a library. Under `gluon check` and `gluon
    /// clippy` it is the file dependents link against.
    pub rmeta_path: Option<PathBuf>,
    /// `-C extra-filename` value, when the unit uses one. Every unit
    /// but cross executables, which keep stable names in `final/`.
    pub extra_filename: Option<String>,
    /// `-C incremental` directory, when the unit uses one.
    pub incremental_dir: Option<PathBuf>,
}

/// The unit table plus the lookups other nodes use.
#[derive(Debug, Clone, Default)]
pub struct UnitGraph {
    units: Vec<BuildUnit>,
    unit_of_crate: BTreeMap<Handle<CrateDef>, UnitId>,
    unit_of_vendor: BTreeMap<VendorBuildId, UnitId>,
    /// Config crate hash per target that has a unit linking it.
    config_metadata: BTreeMap<Handle<TargetDef>, Metadata>,
}

impl UnitGraph {
    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    pub fn get(&self, id: UnitId) -> &BuildUnit {
        &self.units[id.index()]
    }

    pub fn iter(&self) -> impl Iterator<Item = (UnitId, &BuildUnit)> {
        self.units
            .iter()
            .enumerate()
            .map(|(i, u)| (UnitId(i as u32), u))
    }

    /// The unit of a declared crate. `None` when the crate is not part
    /// of this build (gated out by the profile or config).
    pub fn unit_of(&self, h: Handle<CrateDef>) -> Option<UnitId> {
        self.unit_of_crate.get(&h).copied()
    }

    pub fn unit_of_crate(&self) -> &BTreeMap<Handle<CrateDef>, UnitId> {
        &self.unit_of_crate
    }

    /// The unit of a vendored crate build.
    pub fn unit_of_vendor(&self, id: VendorBuildId) -> Option<UnitId> {
        self.unit_of_vendor.get(&id).copied()
    }

    /// Hash of `target`'s config crate. `None` when no unit in this
    /// build links it.
    pub fn config_metadata(&self, target: Handle<TargetDef>) -> Option<Metadata> {
        self.config_metadata.get(&target).copied()
    }

    /// The directory that holds `id`'s `deps/`, `final/` and
    /// `incremental/`. See the module docs.
    fn platform_dir(
        &self,
        id: UnitId,
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
    ) -> Result<PathBuf> {
        let unit = self.get(id);
        Ok(match (unit.platform, unit.profile) {
            (Platform::Host, UnitProfile::Host) => layout.host_dir(),
            (Platform::Host, UnitProfile::Active) => layout.host_profile_dir(&resolved.profile),
            (Platform::Cross(t), _) => {
                let t = target_def(model, t, &unit.name)?;
                if unit.settings.always_rustc {
                    layout.shared_cross_dir(t, &resolved.profile)
                } else {
                    layout.cross_dir(t, &resolved.profile)
                }
            }
        })
    }

    /// `-L dependency=` directories for `id`, sorted: its own platform's
    /// `deps/`, the shared `host/deps/` (where proc macros live; rustc
    /// may load one re-exported by a dep), and for driver-rooted cross
    /// units the shared cross `deps/` that holds vendored units. Cargo
    /// passes the host directory to every cross unit for the same
    /// reason. Prebuilt artifacts' directories are added by the command
    /// builder.
    pub fn search_dirs(
        &self,
        id: UnitId,
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
    ) -> Result<Vec<PathBuf>> {
        let unit = self.get(id);
        let mut dirs = BTreeSet::new();
        dirs.insert(self.platform_dir(id, layout, model, resolved)?.join("deps"));
        dirs.insert(layout.host_dir().join("deps"));
        if let Platform::Cross(t) = unit.platform {
            let t = target_def(model, t, &unit.name)?;
            dirs.insert(layout.shared_cross_dir(t, &resolved.profile).join("deps"));
        }
        Ok(dirs.into_iter().collect())
    }

    /// Output paths of `id`. Errors only when a handle the unit refers
    /// to is missing from the model.
    pub fn paths(
        &self,
        id: UnitId,
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
    ) -> Result<UnitPaths> {
        let unit = self.get(id);
        let cn = &unit.crate_name;
        let h16 = unit.metadata.short();
        let platform_dir = self.platform_dir(id, layout, model, resolved)?;
        let incremental_dir = unit.settings.incremental.then(|| {
            platform_dir
                .join("incremental")
                .join(format!("{}-{h16}", unit.name))
        });
        match (unit.platform, unit.kind) {
            // Cross executables land in `final/` under stable names
            // because ESP entries, images, QEMU and users refer to them.
            (Platform::Cross(t), CrateType::Bin) => {
                let t = target_def(model, t, &unit.name)?;
                let out_dir = platform_dir.join("final");
                Ok(UnitPaths {
                    output_path: out_dir.join(format!("{cn}{}", exe_suffix_for_target(&t.spec))),
                    depfile_path: out_dir.join(format!("{cn}.d")),
                    out_dir,
                    debug_elf_path: None,
                    rmeta_path: None,
                    extra_filename: None,
                    incremental_dir,
                })
            }
            (Platform::Cross(_), CrateType::RawBinary) => {
                let out_dir = platform_dir.join("final");
                Ok(UnitPaths {
                    output_path: out_dir.join(format!("{cn}.bin")),
                    depfile_path: out_dir.join(format!("{cn}.d")),
                    debug_elf_path: Some(out_dir.join(format!("{cn}.elf"))),
                    out_dir,
                    rmeta_path: None,
                    extra_filename: None,
                    incremental_dir,
                })
            }
            (_, kind) => {
                let out_dir = platform_dir.join("deps");
                let stem = format!("{cn}-{h16}");
                let output_path = match kind {
                    CrateType::Bin | CrateType::RawBinary => out_dir.join(&stem),
                    CrateType::ProcMacro => out_dir.join(proc_macro_file_name(&stem)),
                    _ => out_dir.join(format!("lib{stem}.rlib")),
                };
                let rmeta_path =
                    (!kind.is_executable()).then(|| out_dir.join(format!("lib{stem}.rmeta")));
                Ok(UnitPaths {
                    depfile_path: out_dir.join(format!("{stem}.d")),
                    out_dir,
                    output_path,
                    debug_elf_path: None,
                    rmeta_path,
                    extra_filename: Some(format!("-{h16}")),
                    incremental_dir,
                })
            }
        }
    }

    /// Cache key of `id`. Part of the on-disk cache contract; the
    /// driver suffix keeps `gluon build`, `check` and `clippy` apart.
    pub fn cache_key(
        &self,
        id: UnitId,
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
    ) -> Result<String> {
        let unit = self.get(id);
        // Units built the same way under every driver share one record.
        let sfx = if unit.settings.always_rustc {
            ""
        } else {
            layout.cache_key_suffix()
        };
        let profile = &resolved.profile.name;
        let (prefix, name) = match unit.origin {
            UnitOrigin::Crate(_) => ("crate", unit.name.clone()),
            UnitOrigin::Vendor(_) => (
                "vendor",
                format!("{}:{}", unit.package.name, unit.package.version),
            ),
        };
        // Units with a hashed file name get the hash in their key, so
        // each artifact has its own record and alternating between two
        // (say, config presets) rebuilds neither. Cross executables have
        // one output path, so one record.
        let hash = if unit.platform != Platform::Host && unit.kind.is_executable() {
            String::new()
        } else {
            format!(":{}", unit.metadata.short())
        };
        Ok(match (unit.platform, unit.profile) {
            (Platform::Host, UnitProfile::Host) => format!("{prefix}:host:{name}{hash}{sfx}"),
            (Platform::Host, UnitProfile::Active) => {
                format!("{prefix}:host:{name}:{profile}{hash}{sfx}")
            }
            (Platform::Cross(t), _) => {
                let t = target_def(model, t, &unit.name)?;
                format!("{prefix}:cross:{name}:{profile}:{}{hash}{sfx}", t.name)
            }
        })
    }

    /// Name of `id` in failure messages, such as
    /// `crate kernel (x86_64-unknown-none, dev)`.
    pub fn label(&self, id: UnitId, model: &BuildModel, resolved: &ResolvedConfig) -> String {
        let unit = self.get(id);
        let platform = match (unit.platform, unit.profile) {
            (Platform::Host, UnitProfile::Host) => "host".to_string(),
            (Platform::Host, UnitProfile::Active) => format!("host, {}", resolved.profile.name),
            (Platform::Cross(t), _) => {
                let name = model
                    .targets
                    .get(t)
                    .map(|t| t.name.as_str())
                    .unwrap_or("<unknown target>");
                format!("{name}, {}", resolved.profile.name)
            }
        };
        match unit.origin {
            UnitOrigin::Crate(_) => format!("crate {} ({platform})", unit.name),
            UnitOrigin::Vendor(_) => format!(
                "vendored crate {} {} ({platform})",
                unit.package.name, unit.package.version
            ),
        }
    }
}

fn crate_def(model: &BuildModel, h: Handle<CrateDef>) -> Result<&CrateDef> {
    model.crates.get(h).ok_or_else(|| {
        Error::Compile(format!(
            "unit lowering: crate handle {h:?} is in resolved.crates but not in the build \
             model (resolver bug)"
        ))
    })
}

fn target_def<'m>(
    model: &'m BuildModel,
    t: Handle<TargetDef>,
    unit: &str,
) -> Result<&'m TargetDef> {
    model.targets.get(t).ok_or_else(|| {
        Error::Compile(format!(
            "unit '{unit}': target handle {t:?} not found in build model"
        ))
    })
}

fn platform_of(spec: TargetSpec) -> Platform {
    match spec {
        TargetSpec::Host => Platform::Host,
        TargetSpec::Cross(t) => Platform::Cross(t),
    }
}

/// Build the unit table for one build.
///
/// Declared crates come first, in `resolved.crates` order, then vendored
/// builds in `vendor_ctx` order, so unit ids are stable across runs.
///
/// Errors on model inconsistencies (a dangling handle, a vendor dep edge
/// that does not resolve). A missing vendored artifact is not an error
/// here; see [`DepTarget::Missing`].
pub fn lower(
    model: &BuildModel,
    resolved: &ResolvedConfig,
    vendor_ctx: &VendorBuildContext,
) -> Result<UnitGraph> {
    let crate_count = resolved.crates.len() as u32;
    let unit_of_crate: BTreeMap<Handle<CrateDef>, UnitId> = resolved
        .crates
        .iter()
        .enumerate()
        .map(|(i, r)| (r.handle, UnitId(i as u32)))
        .collect();
    let unit_of_vendor: BTreeMap<VendorBuildId, UnitId> = vendor_ctx
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id, UnitId(crate_count + i as u32)))
        .collect();

    let mut units = Vec::with_capacity(resolved.crates.len() + vendor_ctx.len());

    for cref in &resolved.crates {
        let def = crate_def(model, cref.handle)?;
        let platform = if cref.host {
            Platform::Host
        } else {
            Platform::Cross(cref.target)
        };
        let no_sysroot = model
            .groups
            .get(def.group)
            .map(|g| g.no_sysroot)
            .unwrap_or(false);
        let links_sysroot = platform != Platform::Host && !no_sysroot;
        let crate_dir = resolved.project_root.join(&def.path);
        let src = match &def.root {
            Some(root) => crate_dir.join(root),
            None if def.crate_type.is_executable() => crate_dir.join("src/main.rs"),
            None => crate_dir.join("src/lib.rs"),
        };

        let mut deps = Vec::with_capacity(def.deps.len());
        for (extern_name, dep) in &def.deps {
            let target = match dep.resolution {
                // A dep outside this build (gated out) gets no edge and
                // no --extern, as before units existed.
                DepResolution::Project(h) => match unit_of_crate.get(&h) {
                    Some(&u) => DepTarget::Unit(u),
                    None => continue,
                },
                DepResolution::External(e) => {
                    let ext = model.external_deps.get(e).ok_or_else(|| {
                        Error::Compile(format!(
                            "crate '{}': external dep handle for '{extern_name}' is dangling \
                             (intern bug)",
                            def.name
                        ))
                    })?;
                    external_dep_target(model, vendor_ctx, &unit_of_vendor, ext, extern_name)
                }
            };
            deps.push(UnitDep {
                extern_name: extern_name.clone(),
                target,
            });
        }

        let order_deps = def
            .artifact_deps
            .iter()
            .filter_map(|name| model.crates.lookup(name))
            .filter_map(|h| unit_of_crate.get(&h).copied())
            .collect();

        units.push(BuildUnit {
            name: def.name.clone(),
            crate_name: normalize_crate_name(&def.name).into_owned(),
            origin: UnitOrigin::Crate(cref.handle),
            package: PackageKey {
                name: def.name.clone(),
                version: "0.0.0".into(),
                source: PackageSource::Path(PathBuf::from(&def.path)),
            },
            kind: def.crate_type,
            platform,
            profile: UnitProfile::Active,
            sysroot: links_sysroot,
            config_crate: links_sysroot,
            src,
            edition: def.edition.clone(),
            features: Vec::new(),
            deps,
            order_deps,
            settings: UnitSettings {
                cfg_flags: def.cfg_flags.clone(),
                rustc_flags: def.rustc_flags.clone(),
                cap_lints: false,
                linker_script: def
                    .linker_script
                    .as_ref()
                    .filter(|_| def.crate_type.is_executable())
                    .map(|ls| resolved.project_root.join(ls)),
                incremental: true,
                always_rustc: false,
            },
            metadata: Metadata::default(),
        });
    }

    for (id, build) in vendor_ctx.iter() {
        let mut deps = Vec::with_capacity(build.deps.len());
        for edge in &build.deps {
            let dep = vendor_ctx
                .lookup(edge.target, &edge.extern_name)
                .and_then(|d| unit_of_vendor.get(&d).copied())
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "unit lowering: vendor build {id} ({}) references unresolved dep \
                         ({:?}, {}); vendor graph lowering produced an inconsistent edge",
                        build.crate_id, edge.target, edge.extern_name
                    ))
                })?;
            deps.push(UnitDep {
                extern_name: edge.extern_name.clone(),
                target: DepTarget::Unit(dep),
            });
        }
        let platform = platform_of(build.target);
        units.push(BuildUnit {
            name: build.crate_id.name.clone(),
            crate_name: build.cargo_target_name.clone(),
            origin: UnitOrigin::Vendor(id),
            package: PackageKey {
                name: build.crate_id.name.clone(),
                version: build.crate_id.version.clone(),
                source: PackageSource::Registry,
            },
            kind: match build.kind {
                CrateKind::Lib => CrateType::Lib,
                CrateKind::ProcMacro => CrateType::ProcMacro,
            },
            platform,
            profile: UnitProfile::Active,
            // Vendored crates link the sysroot but not the config
            // crate: crates.io code does not know the project's
            // `CONFIG_*` constants.
            sysroot: platform != Platform::Host,
            config_crate: false,
            src: build.lib_path.clone(),
            edition: build.edition.clone(),
            features: build.features.clone(),
            deps,
            order_deps: Vec::new(),
            settings: UnitSettings {
                cap_lints: true,
                always_rustc: true,
                ..UnitSettings::default()
            },
            metadata: Metadata::default(),
        });
    }

    uniquify_vendor_names(&mut units, model);
    assign_host_profile(&mut units);
    let config_metadata = config_crate_hashes(&units, model, resolved)?;
    assign_metadata(&mut units, model, resolved, &config_metadata)?;

    Ok(UnitGraph {
        units,
        unit_of_crate,
        unit_of_vendor,
        config_metadata,
    })
}

/// Config crate hash for every target that has a unit linking it. The
/// source is rendered only when some unit needs it, so a build without
/// a config crate never fails on a config-option name collision.
fn config_crate_hashes(
    units: &[BuildUnit],
    model: &BuildModel,
    resolved: &ResolvedConfig,
) -> Result<BTreeMap<Handle<TargetDef>, Metadata>> {
    let targets: BTreeSet<Handle<TargetDef>> = units
        .iter()
        .filter(|u| u.config_crate)
        .filter_map(|u| match u.platform {
            Platform::Cross(t) => Some(t),
            Platform::Host => None,
        })
        .collect();
    if targets.is_empty() {
        return Ok(BTreeMap::new());
    }
    let source = crate::generated::config_crate::render_source(resolved)?;
    let name = crate::generated::config_crate::config_crate_name(resolved);
    targets
        .into_iter()
        .map(|t| {
            let platform = metadata::platform_identity(Platform::Cross(t), model, resolved)?;
            Ok((
                t,
                metadata::config_crate_metadata(&name, &platform, &source),
            ))
        })
        .collect()
}

/// Compute every unit's metadata hash, deps first. A cycle is a
/// lowering bug and fails with the unit names on it.
fn assign_metadata(
    units: &mut [BuildUnit],
    model: &BuildModel,
    resolved: &ResolvedConfig,
    config: &BTreeMap<Handle<TargetDef>, Metadata>,
) -> Result<()> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        New,
        Open,
        Done,
    }
    let mut state = vec![State::New; units.len()];
    for root in 0..units.len() {
        if state[root] != State::New {
            continue;
        }
        // Iterative DFS: (unit, index of the next dep to visit).
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        state[root] = State::Open;
        while let Some(&(u, _)) = stack.last() {
            let dep = {
                let next = &mut stack.last_mut().expect("non-empty").1;
                let mut found = None;
                while *next < units[u].deps.len() {
                    let target = &units[u].deps[*next].target;
                    *next += 1;
                    if let DepTarget::Unit(d) = target {
                        found = Some(d.index());
                        break;
                    }
                }
                found
            };
            if let Some(d) = dep {
                match state[d] {
                    State::Done => {}
                    State::New => {
                        state[d] = State::Open;
                        stack.push((d, 0));
                    }
                    State::Open => {
                        let start = stack.iter().position(|&(x, _)| x == d).unwrap_or(0);
                        let mut cycle: Vec<&str> = stack[start..]
                            .iter()
                            .map(|&(x, _)| units[x].name.as_str())
                            .collect();
                        cycle.push(units[d].name.as_str());
                        return Err(Error::Compile(format!(
                            "unit lowering: dependency cycle {} (lowering bug)",
                            cycle.join(" -> ")
                        )));
                    }
                }
                continue;
            }
            stack.pop();
            let unit = &units[u];
            let platform = metadata::platform_identity(unit.platform, model, resolved)?;
            let cfg = match (unit.config_crate, unit.platform) {
                (true, Platform::Cross(t)) => config.get(&t).copied(),
                _ => None,
            };
            let m = metadata::unit_metadata(
                unit,
                &platform,
                &resolved.project_root,
                |d| units[d.index()].metadata,
                cfg,
            );
            units[u].metadata = m;
            state[u] = State::Done;
        }
    }
    Ok(())
}

/// Resolve a declared crate's external dep: the vendor unit when the
/// cross pipeline built one, else the cargo-built host artifact, else
/// [`DepTarget::Missing`].
fn external_dep_target(
    model: &BuildModel,
    vendor_ctx: &VendorBuildContext,
    unit_of_vendor: &BTreeMap<VendorBuildId, UnitId>,
    ext: &gluon_model::ExternalDepDef,
    extern_name: &str,
) -> DepTarget {
    if let Some(u) = vendor_ctx
        .lookup(ext.target, extern_name)
        .and_then(|id| unit_of_vendor.get(&id))
    {
        return DepTarget::Unit(*u);
    }
    // Key shape matches `vendor::auto_register_vendored_deps`: the dep's
    // own target and the underscored crate name.
    let key = ExternalArtifactKey {
        target: ext.target,
        cargo_target_name: ext.name.replace('-', "_"),
    };
    let Some(path) = model.external_artifacts.get(&key) else {
        return DepTarget::Missing {
            package: ext.name.clone(),
        };
    };
    // Stable cargo keeps every unit in one `deps/` directory, so the
    // artifact's parent covers its own deps. Nightly cargo (1.101+)
    // gives each unit its own directory; `gluon vendor` records all of
    // them in `gluon.lock` (`external_search_dirs`).
    let mut search_dirs: Vec<PathBuf> = path.parent().map(PathBuf::from).into_iter().collect();
    if ext.target == TargetSpec::Host {
        search_dirs.extend(model.external_search_dirs.iter().cloned());
    }
    DepTarget::Prebuilt {
        path: path.clone(),
        search_dirs,
    }
}

/// Vendored units are named after their package. When that name is
/// taken (the same package on host and target, or a declared crate of
/// that name), suffix it with the platform.
fn uniquify_vendor_names(units: &mut [BuildUnit], model: &BuildModel) {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for u in units.iter() {
        *count.entry(u.name.clone()).or_default() += 1;
    }
    for u in units.iter_mut() {
        if matches!(u.origin, UnitOrigin::Vendor(_)) && count[&u.name] > 1 {
            let suffix = match u.platform {
                Platform::Host => "host".to_string(),
                Platform::Cross(t) => model
                    .targets
                    .get(t)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| format!("{t:?}")),
            };
            u.name = format!("{}@{suffix}", u.name);
        }
    }
}

/// Put proc macros, vendored host units, and every host unit they
/// depend on, on the fixed host profile. See the module docs.
fn assign_host_profile(units: &mut [BuildUnit]) {
    let mut stack: Vec<UnitId> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            u.platform == Platform::Host
                && (u.kind == CrateType::ProcMacro || matches!(u.origin, UnitOrigin::Vendor(_)))
        })
        .map(|(i, _)| UnitId(i as u32))
        .collect();
    let mut fixed: BTreeSet<UnitId> = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !fixed.insert(id) {
            continue;
        }
        for dep in &units[id.index()].deps {
            if let DepTarget::Unit(d) = dep.target
                && units[d.index()].platform == Platform::Host
            {
                stack.push(d);
            }
        }
    }
    for id in fixed {
        let unit = &mut units[id.index()];
        unit.profile = UnitProfile::Host;
        unit.settings.always_rustc = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{DagNode, build_dag};
    use crate::vendor::cross::graph::{VendorCrateBuild, VendorDepEdge};
    use crate::vendor::cross::metadata::CrateId;
    use gluon_model::{
        DepDef, DepSource, ExternalDepDef, GroupDef, ProjectDef, ResolvedCrateRef, ResolvedProfile,
    };

    struct Fixture {
        model: BuildModel,
        resolved: ResolvedConfig,
        target: Handle<TargetDef>,
    }

    impl Fixture {
        fn new() -> Self {
            let mut model = BuildModel::default();
            let (target, _) = model.targets.insert(
                "x86_64-unknown-none".into(),
                TargetDef {
                    name: "x86_64-unknown-none".into(),
                    spec: "x86_64-unknown-none".into(),
                    builtin: true,
                    panic_strategy: None,
                    span: None,
                },
            );
            let resolved = ResolvedConfig {
                project: ProjectDef {
                    name: "demo".into(),
                    version: "0.1.0".into(),
                    config_crate_name: None,
                    cfg_prefix: None,
                    config_override_file: None,
                    default_profile: None,
                },
                profile: ResolvedProfile {
                    name: "dev".into(),
                    target,
                    opt_level: 0,
                    debug_info: true,
                    lto: None,
                    boot_binary: None,
                    qemu_memory: None,
                    qemu_cores: None,
                    qemu_extra_args: Vec::new(),
                    qemu_boot_mode: None,
                    test_timeout: None,
                },
                options: Default::default(),
                crates: Vec::new(),
                build_dir: PathBuf::from("/proj/build"),
                project_root: PathBuf::from("/proj"),
            };
            Self {
                model,
                resolved,
                target,
            }
        }

        /// Declare a crate and put it in the build.
        fn add_crate(
            &mut self,
            name: &str,
            kind: CrateType,
            host: bool,
            deps: &[(&str, DepResolution)],
        ) -> Handle<CrateDef> {
            let spec = if host {
                TargetSpec::Host
            } else {
                TargetSpec::Cross(self.target)
            };
            let group_name = if host { "host" } else { "kernel" };
            let group = match self.model.groups.lookup(group_name) {
                Some(g) => g,
                None => {
                    self.model
                        .groups
                        .insert(
                            group_name.into(),
                            GroupDef {
                                name: group_name.into(),
                                target: spec,
                                ..Default::default()
                            },
                        )
                        .0
                }
            };
            let (h, _) = self.model.crates.insert(
                name.into(),
                CrateDef {
                    name: name.into(),
                    path: format!("crates/{name}"),
                    edition: "2021".into(),
                    crate_type: kind,
                    target: spec,
                    deps: deps
                        .iter()
                        .map(|(n, r)| {
                            (
                                n.to_string(),
                                DepDef {
                                    version: None,
                                    features: Vec::new(),
                                    default_features: true,
                                    optional: false,
                                    resolution: *r,
                                    span: None,
                                },
                            )
                        })
                        .collect(),
                    dev_deps: BTreeMap::new(),
                    features: Vec::new(),
                    root: None,
                    linker_script: None,
                    group,
                    is_project_crate: true,
                    cfg_flags: Vec::new(),
                    rustc_flags: Vec::new(),
                    requires_config: Vec::new(),
                    artifact_deps: Vec::new(),
                    artifact_env: BTreeMap::new(),
                    span: None,
                },
            );
            self.resolved.crates.push(ResolvedCrateRef {
                handle: h,
                target: self.target,
                host,
            });
            h
        }

        fn lower(&self, vendor_ctx: &VendorBuildContext) -> UnitGraph {
            lower(&self.model, &self.resolved, vendor_ctx).expect("lower")
        }
    }

    fn vendor_lib(name: &str, target: TargetSpec, deps: Vec<VendorDepEdge>) -> VendorCrateBuild {
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: "1.2.3".into(),
            },
            cargo_target_name: name.replace('-', "_"),
            target,
            kind: CrateKind::Lib,
            manifest_dir: PathBuf::from(format!("/vendor/{name}")),
            lib_path: PathBuf::from(format!("/vendor/{name}/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps,
        }
    }

    #[test]
    fn package_on_host_and_target_gives_two_units_nodes_and_paths() {
        let fx = Fixture::new();
        let mut vctx = VendorBuildContext::new();
        let host = vctx
            .add(vendor_lib("bitflags", TargetSpec::Host, vec![]))
            .unwrap();
        let cross = vctx
            .add(vendor_lib("bitflags", TargetSpec::Cross(fx.target), vec![]))
            .unwrap();
        let units = fx.lower(&vctx);
        let (h, c) = (
            units.unit_of_vendor(host).unwrap(),
            units.unit_of_vendor(cross).unwrap(),
        );
        assert_ne!(h, c);
        assert_eq!(units.get(h).name, "bitflags@host");
        assert_eq!(units.get(c).name, "bitflags@x86_64-unknown-none");

        let dag = build_dag(&fx.resolved, &fx.model, &units).unwrap();
        assert!(dag.node_index.contains_key(&DagNode::Compile(h)));
        assert!(dag.node_index.contains_key(&DagNode::Compile(c)));

        let layout = BuildLayout::new("/proj/build", "demo");
        let path = |u| {
            units
                .paths(u, &layout, &fx.model, &fx.resolved)
                .unwrap()
                .output_path
        };
        let (hh, ch) = (units.get(h).metadata.short(), units.get(c).metadata.short());
        assert_ne!(hh, ch, "host and target units must not share a hash");
        assert_eq!(
            path(h),
            PathBuf::from(format!("/proj/build/host/deps/libbitflags-{hh}.rlib"))
        );
        assert_eq!(
            path(c),
            PathBuf::from(format!(
                "/proj/build/cross/x86_64-unknown-none/dev/deps/libbitflags-{ch}.rlib"
            ))
        );
        let key = |u| {
            units
                .cache_key(u, &layout, &fx.model, &fx.resolved)
                .unwrap()
        };
        assert_eq!(key(h), format!("vendor:host:bitflags:1.2.3:{hh}"));
        assert_eq!(
            key(c),
            format!("vendor:cross:bitflags:1.2.3:dev:x86_64-unknown-none:{ch}")
        );
    }

    #[test]
    fn declared_crate_external_dep_resolves_to_vendor_unit() {
        let mut fx = Fixture::new();
        let (ext, _) = fx.model.external_deps.insert(
            "bitflags".into(),
            ExternalDepDef {
                name: "bitflags".into(),
                source: DepSource::CratesIo {
                    version: "1".into(),
                },
                features: Vec::new(),
                default_features: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                target: TargetSpec::Cross(fx.target),
                span: None,
            },
        );
        let kernel = fx.add_crate(
            "kernel",
            CrateType::Bin,
            false,
            &[("bitflags", DepResolution::External(ext))],
        );
        let mut vctx = VendorBuildContext::new();
        let vid = vctx
            .add(vendor_lib("bitflags", TargetSpec::Cross(fx.target), vec![]))
            .unwrap();
        let units = fx.lower(&vctx);
        let k = units.get(units.unit_of(kernel).unwrap());
        assert_eq!(
            k.deps,
            vec![UnitDep {
                extern_name: "bitflags".into(),
                target: DepTarget::Unit(units.unit_of_vendor(vid).unwrap()),
            }]
        );

        // Without a vendor unit or a prebuilt artifact the dep stays in
        // the table as Missing, so lowering still succeeds.
        let units = fx.lower(&VendorBuildContext::new());
        let k = units.get(units.unit_of(kernel).unwrap());
        assert_eq!(
            k.deps[0].target,
            DepTarget::Missing {
                package: "bitflags".into()
            }
        );
    }

    #[test]
    fn unresolved_vendor_edge_is_a_lowering_error() {
        let fx = Fixture::new();
        let mut vctx = VendorBuildContext::new();
        vctx.add(vendor_lib(
            "consumer",
            TargetSpec::Host,
            vec![VendorDepEdge {
                extern_name: "ghost".into(),
                crate_id: CrateId {
                    name: "ghost".into(),
                    version: "1.0.0".into(),
                },
                target: TargetSpec::Host,
            }],
        ))
        .unwrap();
        let err = lower(&fx.model, &fx.resolved, &vctx).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn proc_macros_and_their_host_deps_use_the_fixed_host_profile() {
        let mut fx = Fixture::new();
        let shared = fx.add_crate("shared", CrateType::Lib, true, &[]);
        let derive = fx.add_crate(
            "derive",
            CrateType::ProcMacro,
            true,
            &[("shared", DepResolution::Project(shared))],
        );
        let tool_lib = fx.add_crate("tool_lib", CrateType::Lib, true, &[]);
        let tool = fx.add_crate(
            "tool",
            CrateType::Bin,
            true,
            &[
                ("shared", DepResolution::Project(shared)),
                ("tool_lib", DepResolution::Project(tool_lib)),
            ],
        );
        let kernel = fx.add_crate(
            "kernel",
            CrateType::Bin,
            false,
            &[("derive", DepResolution::Project(derive))],
        );
        let units = fx.lower(&VendorBuildContext::new());
        let profile = |h| units.get(units.unit_of(h).unwrap()).profile;
        assert_eq!(profile(derive), UnitProfile::Host);
        assert_eq!(profile(shared), UnitProfile::Host);
        assert_eq!(profile(tool), UnitProfile::Active);
        assert_eq!(profile(tool_lib), UnitProfile::Active);
        assert_eq!(profile(kernel), UnitProfile::Active);

        // Host-profile units share the profile-independent `host/deps/`
        // under every driver; active-profile host units get the profile
        // in their path and key, and the driver root.
        let check = BuildLayout::with_driver(
            "/proj/build",
            "demo",
            crate::compile::driver::DriverKind::Check,
        );
        let paths = |h| {
            units
                .paths(units.unit_of(h).unwrap(), &check, &fx.model, &fx.resolved)
                .unwrap()
        };
        let short = |h| units.get(units.unit_of(h).unwrap()).metadata.short();
        assert_eq!(
            paths(derive).out_dir,
            PathBuf::from("/proj/build/host/deps")
        );
        assert_eq!(
            paths(tool).output_path,
            PathBuf::from(format!(
                "/proj/build/tool/check/host/profile/dev/deps/tool-{}",
                short(tool)
            ))
        );
        let key = |h| {
            units
                .cache_key(units.unit_of(h).unwrap(), &check, &fx.model, &fx.resolved)
                .unwrap()
        };
        assert_eq!(key(derive), format!("crate:host:derive:{}", short(derive)));
        assert_eq!(
            key(tool),
            format!("crate:host:tool:dev:{}:check", short(tool))
        );
        assert!(
            units
                .get(units.unit_of(shared).unwrap())
                .settings
                .always_rustc
        );
        assert!(
            !units
                .get(units.unit_of(tool).unwrap())
                .settings
                .always_rustc
        );
    }

    #[test]
    fn gated_dep_has_no_unit_and_no_dep_entry() {
        let mut fx = Fixture::new();
        let kernel = fx.add_crate("kernel", CrateType::Bin, false, &[]);
        let gated = fx.add_crate("gated", CrateType::Lib, false, &[]);
        fx.resolved.crates.retain(|r| r.handle != gated);
        if let Some(def) = fx.model.crates.get_mut(kernel) {
            def.deps.insert(
                "gated".into(),
                DepDef {
                    version: None,
                    features: Vec::new(),
                    default_features: true,
                    optional: false,
                    resolution: DepResolution::Project(gated),
                    span: None,
                },
            );
        }
        let units = fx.lower(&VendorBuildContext::new());
        assert_eq!(units.unit_of(gated), None);
        assert!(units.get(units.unit_of(kernel).unwrap()).deps.is_empty());
    }

    /// A change to one declared crate's fixture.
    type Edit = dyn Fn(&mut Fixture, Handle<CrateDef>);

    fn meta(units: &UnitGraph, h: Handle<CrateDef>) -> Metadata {
        units.get(units.unit_of(h).unwrap()).metadata
    }

    /// Golden hashes. A change here changes every artifact name, so it
    /// must be deliberate (bump the domain tag in `metadata.rs`).
    #[test]
    fn metadata_golden_values() {
        let mut fx = Fixture::new();
        let kernel = fx.add_crate("kernel", CrateType::Lib, false, &[]);
        let mut vctx = VendorBuildContext::new();
        let vid = vctx
            .add(vendor_lib("bitflags", TargetSpec::Host, vec![]))
            .unwrap();
        let units = fx.lower(&vctx);
        assert_eq!(meta(&units, kernel).short(), "dcf3e58fda78f19b");
        assert_eq!(
            units
                .get(units.unit_of_vendor(vid).unwrap())
                .metadata
                .short(),
            "9c70910fd504b049"
        );
        assert_eq!(
            units.config_metadata(fx.target).unwrap().short(),
            "ba66cb8d00a72b22"
        );
    }

    #[test]
    fn metadata_changes_with_identity_fields() {
        let base = |edit: &dyn Fn(&mut Fixture, Handle<CrateDef>)| {
            let mut fx = Fixture::new();
            let h = fx.add_crate("kernel", CrateType::Lib, false, &[]);
            edit(&mut fx, h);
            meta(&fx.lower(&VendorBuildContext::new()), h)
        };
        let unchanged = base(&|_, _| {});
        let edits: [(&str, &Edit); 4] = [
            ("path", &|fx, h| {
                fx.model.crates.get_mut(h).unwrap().path = "elsewhere/kernel".into()
            }),
            ("kind", &|fx, h| {
                fx.model.crates.get_mut(h).unwrap().crate_type = CrateType::ProcMacro
            }),
            ("platform", &|fx, _| fx.resolved.crates[0].host = true),
            ("config value", &|fx, _| {
                fx.resolved
                    .options
                    .insert("LOG".into(), gluon_model::ResolvedValue::Bool(true));
            }),
        ];
        for (what, edit) in edits {
            assert_ne!(base(edit), unchanged, "{what} must change the hash");
        }

        // Vendored units: version and features.
        let vendor = |version: &str, features: &[&str]| {
            let mut b = vendor_lib("bitflags", TargetSpec::Host, vec![]);
            b.crate_id.version = version.into();
            b.features = features.iter().map(|f| f.to_string()).collect();
            let mut vctx = VendorBuildContext::new();
            let id = vctx.add(b).unwrap();
            let units = Fixture::new().lower(&vctx);
            units.get(units.unit_of_vendor(id).unwrap()).metadata
        };
        assert_ne!(vendor("1.2.3", &[]), vendor("1.2.4", &[]));
        assert_ne!(vendor("1.2.3", &[]), vendor("1.2.3", &["std"]));
        assert_eq!(
            vendor("1.2.3", &["a", "b"]),
            vendor("1.2.3", &["b", "a"]),
            "feature order is not identity"
        );
    }

    #[test]
    fn metadata_ignores_content_fields() {
        let base = |edit: &dyn Fn(&mut Fixture, Handle<CrateDef>)| {
            let mut fx = Fixture::new();
            let h = fx.add_crate("kernel", CrateType::Lib, false, &[]);
            edit(&mut fx, h);
            meta(&fx.lower(&VendorBuildContext::new()), h)
        };
        let unchanged = base(&|_, _| {});
        let edits: [(&str, &Edit); 5] = [
            ("cfg flags", &|fx, h| {
                fx.model.crates.get_mut(h).unwrap().cfg_flags = vec!["x".into()]
            }),
            ("rustc flags", &|fx, h| {
                fx.model.crates.get_mut(h).unwrap().rustc_flags =
                    vec!["-Cforce-frame-pointers".into()]
            }),
            ("edition", &|fx, h| {
                fx.model.crates.get_mut(h).unwrap().edition = "2024".into()
            }),
            ("profile", &|fx, _| {
                fx.resolved.profile.name = "release".into();
                fx.resolved.profile.opt_level = 3;
            }),
            ("project location", &|fx, _| {
                fx.resolved.project_root = PathBuf::from("/moved");
                fx.resolved.build_dir = PathBuf::from("/moved/build");
            }),
        ];
        for (what, edit) in edits {
            assert_eq!(base(edit), unchanged, "{what} must not change the hash");
        }
    }

    #[test]
    fn dep_identity_change_propagates_to_dependents() {
        let hashes = |features: &[&str]| {
            let mut fx = Fixture::new();
            let (ext, _) = fx.model.external_deps.insert(
                "bitflags".into(),
                ExternalDepDef {
                    name: "bitflags".into(),
                    source: DepSource::CratesIo {
                        version: "1".into(),
                    },
                    features: Vec::new(),
                    default_features: true,
                    cfg_flags: Vec::new(),
                    rustc_flags: Vec::new(),
                    target: TargetSpec::Cross(fx.target),
                    span: None,
                },
            );
            let lib = fx.add_crate(
                "klib",
                CrateType::Lib,
                false,
                &[("bitflags", DepResolution::External(ext))],
            );
            let kernel = fx.add_crate(
                "kernel",
                CrateType::Bin,
                false,
                &[("klib", DepResolution::Project(lib))],
            );
            let mut b = vendor_lib("bitflags", TargetSpec::Cross(fx.target), vec![]);
            b.features = features.iter().map(|f| f.to_string()).collect();
            let mut vctx = VendorBuildContext::new();
            vctx.add(b).unwrap();
            let units = fx.lower(&vctx);
            (meta(&units, lib), meta(&units, kernel))
        };
        let (lib_a, kernel_a) = hashes(&[]);
        let (lib_b, kernel_b) = hashes(&["serde"]);
        assert_ne!(lib_a, lib_b);
        assert_ne!(kernel_a, kernel_b);
    }

    #[test]
    fn config_values_change_project_units_but_not_vendored_ones() {
        let hashes = |log: bool| {
            let mut fx = Fixture::new();
            fx.resolved
                .options
                .insert("LOG".into(), gluon_model::ResolvedValue::Bool(log));
            let kernel = fx.add_crate("kernel", CrateType::Lib, false, &[]);
            let mut vctx = VendorBuildContext::new();
            let vid = vctx
                .add(vendor_lib("bitflags", TargetSpec::Cross(fx.target), vec![]))
                .unwrap();
            let units = fx.lower(&vctx);
            (
                meta(&units, kernel),
                units.get(units.unit_of_vendor(vid).unwrap()).metadata,
                units.config_metadata(fx.target).unwrap(),
            )
        };
        let (k1, v1, c1) = hashes(false);
        let (k2, v2, c2) = hashes(true);
        assert_ne!(c1, c2);
        assert_ne!(k1, k2);
        assert_eq!(v1, v2);
    }

    #[test]
    fn dependency_cycle_is_a_lowering_error() {
        let mut fx = Fixture::new();
        let a = fx.add_crate("a", CrateType::Lib, false, &[]);
        let b = fx.add_crate(
            "b",
            CrateType::Lib,
            false,
            &[("a", DepResolution::Project(a))],
        );
        fx.model.crates.get_mut(a).unwrap().deps.insert(
            "b".into(),
            DepDef {
                version: None,
                features: Vec::new(),
                default_features: true,
                optional: false,
                resolution: DepResolution::Project(b),
                span: None,
            },
        );
        let err = lower(&fx.model, &fx.resolved, &VendorBuildContext::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cycle") && (msg.contains("a -> b -> a") || msg.contains("b -> a -> b")),
            "{msg}"
        );
    }

    #[test]
    fn cross_units_search_their_platform_and_the_host_deps() {
        let mut fx = Fixture::new();
        let kernel = fx.add_crate("kernel", CrateType::Bin, false, &[]);
        let tool = fx.add_crate("tool", CrateType::Bin, true, &[]);
        let units = fx.lower(&VendorBuildContext::new());
        let check =
            BuildLayout::with_driver("/b", "demo", crate::compile::driver::DriverKind::Check);
        let dirs = |h| {
            units
                .search_dirs(units.unit_of(h).unwrap(), &check, &fx.model, &fx.resolved)
                .unwrap()
        };
        assert_eq!(
            dirs(kernel),
            vec![
                PathBuf::from("/b/cross/x86_64-unknown-none/dev/deps"),
                PathBuf::from("/b/host/deps"),
                PathBuf::from("/b/tool/check/cross/x86_64-unknown-none/dev/deps"),
            ]
        );
        assert_eq!(
            dirs(tool),
            vec![
                PathBuf::from("/b/host/deps"),
                PathBuf::from("/b/tool/check/host/profile/dev/deps"),
            ]
        );
        // Cross executables keep stable names in `final/`.
        let p = units
            .paths(
                units.unit_of(kernel).unwrap(),
                &check,
                &fx.model,
                &fx.resolved,
            )
            .unwrap();
        assert_eq!(
            p.output_path,
            PathBuf::from("/b/tool/check/cross/x86_64-unknown-none/dev/final/kernel")
        );
        assert_eq!(p.extra_filename, None);
        assert_eq!(
            p.incremental_dir,
            Some(PathBuf::from(format!(
                "/b/tool/check/cross/x86_64-unknown-none/dev/incremental/kernel-{}",
                meta(&units, kernel).short()
            )))
        );
    }
}
