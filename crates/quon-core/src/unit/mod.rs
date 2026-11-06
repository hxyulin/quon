//! The build unit table: one [`BuildUnit`] per rustc invocation for a
//! crate.
//!
//! [`lower`] runs once per build, after `config::resolve` and
//! `build_cross_vendor_context`, and before `build_dag`. It turns the
//! two crate sources the build knows about into one table:
//!
//! - every crate in `ResolvedConfig::crates` (declared in `quon.rs`),
//! - every build in the [`VendorBuildContext`] (`Dependency`
//!   crates compiled for a cross target, plus their host proc macros).
//! - the build script of every vendored crate and every manifest unit
//!   that has one: a host executable unit, plus one [`ScriptRun`] per
//!   owning unit. Owners whose script would compile identically (same
//!   package, features and build-deps, such as a crate built for host
//!   and target) share one script unit.
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
//! - Proc macros, build scripts, every vendored host unit, and every
//!   host unit they depend on use the fixed host profile ([`HOST_OPT_LEVEL`],
//!   [`HOST_DEBUG_INFO`]). Switching between `dev` and `release` does
//!   not rebuild them, and their output directory is
//!   profile-independent. They are compiled with plain rustc and full
//!   codegen under every driver, because a proc macro links real rlibs
//!   of its deps, so `quon check` and `quon build` share them.
//! - Other host units (host tools and build-step runners declared in
//!   `quon.rs`, and their deps) use the active profile, so a slow host
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
//! <root>/host/build/<pkg>-<h16>/          a compiled build script
//! <owner platform dir>/build/<pkg>-<h16>/ a run: out/ (OUT_DIR), output
//! ```
//!
//! `<driver root>` is `<root>` under `quon build` and
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
use quon_model::{
    BuildModel, CargoLint, CargoPackageSource, CargoUnitDef, CrateDef, CrateType, DepResolution,
    ExternalArtifactKey, Handle, ResolvedConfig, TargetDef, TargetSpec,
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

/// Index into the build-script run table ([`UnitGraph::run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(pub u32);

/// One run of a build script for one owning unit. The same compiled
/// script can run for several owners (a package built for host and
/// target), each with its own features, platform and `OUT_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRun {
    /// The compiled script: a host unit whose output is the binary.
    pub script: UnitId,
    /// The unit whose rustc command takes the run's output.
    pub owner: UnitId,
    /// What the script's environment says about its package.
    pub package: crate::build_script::PackageInfo,
}

/// Where a unit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitOrigin {
    /// A crate declared in `quon.rs` or lowered from a manifest. The
    /// build-script unit of a manifest package carries the handle of its
    /// first owner, and is not that crate: check
    /// [`BuildUnit::is_build_script`] before reading the crate's
    /// settings.
    Crate(Handle<CrateDef>),
    /// A `Dependency` crate from the cross vendor pipeline.
    Vendor(VendorBuildId),
}

/// The package a unit is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageKey {
    pub name: String,
    /// `0.0.0` for crates declared in `quon.rs`.
    pub version: String,
    pub source: PackageSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// Crate directory relative to the project root.
    Path(PathBuf),
    /// A crates.io package: a manifest unit from Cargo's registry cache
    /// or a vendor directory, or a `Dependency` crate.
    Registry {
        /// The `.crate` checksum from `Cargo.lock`. Registry sources are
        /// never stat'ed (see `cache::store`), so the checksum is what
        /// tells two registries' `name@version` apart. `None` for
        /// `Dependency` crates and lock entries without one.
        checksum: Option<String>,
    },
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
    /// The active quon profile.
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
    /// A host artifact built by cargo in `quon vendor`
    /// (`model.external_artifacts`). Not a node; it exists before the
    /// build starts.
    Prebuilt {
        path: PathBuf,
        /// `-L dependency=` directories for the artifact's own deps.
        search_dirs: Vec<PathBuf>,
    },
    /// An external dep with neither a vendor unit nor a prebuilt
    /// artifact. Lowering keeps going so `quon configure` still works;
    /// compiling the consumer fails with a "run `quon vendor`" error.
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
    /// Compile with plain rustc and full codegen under `quon check` and
    /// `quon clippy` too, into the driver-independent root. Set for
    /// vendored units (the user cannot fix their lints, and dependents
    /// need real rlibs) and for fixed-host-profile units (a proc macro
    /// links real rlibs of its deps).
    pub always_rustc: bool,
    /// What Cargo would also pass. Set for manifest units only, so
    /// declared crates' commands do not change.
    pub cargo: Option<CargoSettings>,
}

/// Flags and env a manifest unit gets because Cargo passes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoSettings {
    /// `--check-cfg` values: `docsrs` and `test`, the declared
    /// features, the `unexpected_cfgs` lint's `check-cfg` list, and one
    /// per `cfg_flag` Quon injects, so `unexpected_cfgs` warns only
    /// about cfgs nobody declared.
    pub check_cfg: Vec<String>,
    /// `[lints]`, sorted by priority, then name.
    pub lints: Vec<CargoLint>,
    /// `CARGO_PKG_*`, `CARGO_MANIFEST_*`, `CARGO_CRATE_NAME`, and where
    /// they apply `CARGO_PRIMARY_PACKAGE` and `CARGO_BIN_NAME`.
    pub env: BTreeMap<String, String>,
}

/// Build a manifest unit's [`CargoSettings`]. `cfg_flags` are the
/// unit's Quon `cfg_flag`s, declared to `--check-cfg` so they do not
/// trip `unexpected_cfgs`.
fn cargo_settings(c: &CargoUnitDef, cfg_flags: &[String]) -> CargoSettings {
    let quote = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
    // Cargo's own declarations: `docsrs` and `test` always, and every
    // declared feature (an empty `values()` still declares the key).
    let features: Vec<String> = c.declared_features.iter().map(|f| quote(f)).collect();
    let mut check_cfg = vec![
        "cfg(docsrs,test)".to_string(),
        format!("cfg(feature, values({}))", features.join(", ")),
    ];
    check_cfg.extend(c.check_cfg.iter().cloned());
    for flag in cfg_flags {
        // `name` or `name="value"`, as `--cfg` takes them.
        let decl = match flag.split_once('=') {
            Some((name, value)) => {
                format!("cfg({}, values({}))", name.trim(), value.trim())
            }
            None => format!("cfg({})", flag.trim()),
        };
        check_cfg.push(decl);
    }
    CargoSettings {
        check_cfg,
        lints: c.lints.clone(),
        env: c.env.clone(),
    }
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
    /// Links Quon's sysroot for its target.
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
    /// The unit compiles a package's build script (a host `Bin` named
    /// `build_script_build`). Its deps are the package's build-deps.
    pub is_build_script: bool,
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
    /// `.rmeta` file of a library. Under `quon check` and `quon
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
    /// Build-script runs, one `RunBuildScript` node each.
    runs: Vec<ScriptRun>,
    /// The run whose output each owning unit takes.
    run_of_owner: BTreeMap<UnitId, RunId>,
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

    /// Add a build-script run. Each owner has at most one; adding a
    /// second for the same owner is a lowering bug and errors.
    pub fn add_script_run(&mut self, run: ScriptRun) -> Result<RunId> {
        let id = RunId(self.runs.len() as u32);
        if self.run_of_owner.insert(run.owner, id).is_some() {
            return Err(Error::Compile(format!(
                "unit lowering: {} has two build-script runs (lowering bug)",
                self.get(run.owner).name
            )));
        }
        self.runs.push(run);
        Ok(id)
    }

    pub fn run(&self, id: RunId) -> &ScriptRun {
        &self.runs[id.0 as usize]
    }

    pub fn runs(&self) -> impl Iterator<Item = (RunId, &ScriptRun)> {
        self.runs
            .iter()
            .enumerate()
            .map(|(i, r)| (RunId(i as u32), r))
    }

    /// The build-script run whose output `unit` takes, if any.
    pub fn build_script_of(&self, unit: UnitId) -> Option<RunId> {
        self.run_of_owner.get(&unit).copied()
    }

    /// Directory of a run: `<owner platform dir>/build/<package>-<owner
    /// h16>/`, holding `out/` (`OUT_DIR`) and `output` (the script's
    /// stdout). Keyed by the owner, because each owner runs the script
    /// with its own features and platform.
    pub fn run_dir(
        &self,
        id: RunId,
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
    ) -> Result<PathBuf> {
        let run = self.run(id);
        let owner = self.get(run.owner);
        Ok(self
            .platform_dir(run.owner, layout, model, resolved)?
            .join("build")
            .join(format!("{}-{}", owner.package.name, owner.metadata.short())))
    }

    /// Fingerprint key of a run: the owner's key behind `run:`, so it
    /// carries the owner's identity and driver.
    pub fn run_key(
        &self,
        id: RunId,
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
    ) -> Result<String> {
        let owner_key = self.cache_key(self.run(id).owner, layout, model, resolved)?;
        Ok(format!("run:{owner_key}"))
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
        // A compiled build script gets its own directory, as in Cargo,
        // next to the run directories of its owners.
        if unit.is_build_script {
            let out_dir = platform_dir
                .join("build")
                .join(format!("{}-{h16}", unit.package.name));
            let stem = format!("{cn}-{h16}");
            return Ok(UnitPaths {
                output_path: out_dir.join(format!("{stem}{}", std::env::consts::EXE_SUFFIX)),
                depfile_path: out_dir.join(format!("{stem}.d")),
                out_dir,
                debug_elf_path: None,
                rmeta_path: None,
                extra_filename: Some(format!("-{h16}")),
                incremental_dir: None,
            });
        }
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
                    CrateType::Bin | CrateType::RawBinary | CrateType::Test => out_dir.join(&stem),
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
    /// driver suffix keeps `quon build`, `check` and `clippy` apart.
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
        let (prefix, mut name) = match unit.origin {
            UnitOrigin::Crate(_) => ("crate", unit.name.clone()),
            UnitOrigin::Vendor(_) => (
                "vendor",
                format!("{}:{}", unit.package.name, unit.package.version),
            ),
        };
        if unit.is_build_script {
            name.push_str(":build-script");
        }
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
        // A run is labelled `build script of <owner label>`; a script
        // unit is the compile, which several owners may share.
        let platform = if unit.is_build_script {
            "compile".to_string()
        } else {
            platform
        };
        let what = if unit.is_build_script {
            "build script of "
        } else {
            ""
        };
        match unit.origin {
            UnitOrigin::Crate(_) => format!("{what}crate {} ({platform})", unit.name),
            UnitOrigin::Vendor(_) => format!(
                "{what}vendored crate {} {} ({platform})",
                unit.package.name, unit.package.version
            ),
        }
    }

    /// Runs whose output `run`'s script reads as `DEP_<LINKS>_*`: the
    /// runs of the owner's direct deps whose package declares `links`.
    pub fn links_dep_runs(&self, run: RunId) -> Vec<RunId> {
        self.get(self.run(run).owner)
            .deps
            .iter()
            .filter_map(|d| match d.target {
                DepTarget::Unit(u) => self.build_script_of(u),
                _ => None,
            })
            .filter(|r| self.run(*r).package.links.is_some())
            .collect()
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
        let group = model.groups.get(def.group);
        let no_sysroot = group.map(|g| g.no_sysroot).unwrap_or(false);
        let links_sysroot = platform != Platform::Host && !no_sysroot;
        // Manifest units link the config crate only in a group that asks
        // for it, so workspace dependencies (in the synthetic `cargo:`
        // groups) do not. Declared crates keep linking it whenever they
        // link the sysroot.
        let links_config =
            links_sysroot && (def.cargo.is_none() || group.map(|g| g.config).unwrap_or(false));
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

        let (crate_name, package) = match &def.cargo {
            Some(c) => (
                c.rustc_name.clone(),
                PackageKey {
                    name: c.package.name.clone(),
                    version: c.package.version.clone(),
                    source: match &c.package.source {
                        CargoPackageSource::Path => {
                            PackageSource::Path(PathBuf::from(&c.package.manifest_dir))
                        }
                        CargoPackageSource::Registry { checksum } => PackageSource::Registry {
                            checksum: checksum.clone(),
                        },
                    },
                },
            ),
            None => (
                normalize_crate_name(&def.name).into_owned(),
                PackageKey {
                    name: def.name.clone(),
                    version: "0.0.0".into(),
                    source: PackageSource::Path(PathBuf::from(&def.path)),
                },
            ),
        };

        // As Cargo does for registry packages: their lints are not the
        // user's to fix.
        let cap_lints = matches!(package.source, PackageSource::Registry { .. });
        units.push(BuildUnit {
            name: def.name.clone(),
            crate_name,
            origin: UnitOrigin::Crate(cref.handle),
            package,
            kind: def.crate_type,
            platform,
            profile: UnitProfile::Active,
            sysroot: links_sysroot,
            config_crate: links_config,
            src,
            edition: def.edition.clone(),
            // Empty for declared crates; resolved features for manifest
            // units.
            features: def.features.clone(),
            deps,
            order_deps,
            settings: UnitSettings {
                cfg_flags: def.cfg_flags.clone(),
                rustc_flags: def.rustc_flags.clone(),
                cap_lints,
                linker_script: def
                    .linker_script
                    .as_ref()
                    .filter(|_| def.crate_type.is_executable())
                    .map(|ls| resolved.project_root.join(ls)),
                incremental: true,
                always_rustc: false,
                cargo: def
                    .cargo
                    .as_ref()
                    .map(|c| cargo_settings(c, &def.cfg_flags)),
            },
            metadata: Metadata::default(),
            is_build_script: false,
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
                source: PackageSource::Registry { checksum: None },
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
            is_build_script: false,
        });
    }

    let mut runs = lower_vendor_scripts(&mut units, vendor_ctx, &unit_of_vendor)?;
    runs.extend(lower_manifest_scripts(
        &mut units,
        model,
        resolved,
        &unit_of_crate,
    )?);

    uniquify_vendor_names(&mut units, model);
    assign_host_profile(&mut units);
    let config_metadata = config_crate_hashes(&units, model, resolved)?;
    assign_metadata(&mut units, model, resolved, &config_metadata)?;
    check_unique_outputs(&units)?;

    let mut graph = UnitGraph {
        units,
        unit_of_crate,
        unit_of_vendor,
        config_metadata,
        ..UnitGraph::default()
    };
    for run in runs {
        graph.add_script_run(run)?;
    }
    Ok(graph)
}

/// Add a script unit for every vendored build that has a build script,
/// and return one run per owner. Owners that would compile the same
/// script (same package, features and build-dep units) share one
/// unit, so a crate built for host and target compiles its script once
/// and runs it twice.
fn lower_vendor_scripts(
    units: &mut Vec<BuildUnit>,
    vendor_ctx: &VendorBuildContext,
    unit_of_vendor: &BTreeMap<VendorBuildId, UnitId>,
) -> Result<Vec<ScriptRun>> {
    type ScriptKey = (String, String, Vec<String>, Vec<UnitId>);
    let mut script_of: BTreeMap<ScriptKey, UnitId> = BTreeMap::new();
    let mut per_package: BTreeMap<String, usize> = BTreeMap::new();
    let mut runs = Vec::new();
    for (id, build) in vendor_ctx.iter() {
        let Some(script) = &build.build_script else {
            continue;
        };
        let mut deps = Vec::with_capacity(script.build_deps.len());
        for edge in &script.build_deps {
            let dep = vendor_ctx
                .lookup(edge.target, &edge.extern_name)
                .and_then(|d| unit_of_vendor.get(&d).copied())
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "unit lowering: build script of vendor build {id} ({}) references \
                         unresolved build-dep ({:?}, {}); vendor graph lowering produced an \
                         inconsistent edge",
                        build.crate_id, edge.target, edge.extern_name
                    ))
                })?;
            deps.push(UnitDep {
                extern_name: edge.extern_name.clone(),
                target: DepTarget::Unit(dep),
            });
        }
        let key: ScriptKey = (
            build.crate_id.name.clone(),
            build.crate_id.version.clone(),
            build.features.clone(),
            deps.iter()
                .filter_map(|d| match d.target {
                    DepTarget::Unit(u) => Some(u),
                    _ => None,
                })
                .collect(),
        );
        let script_unit = match script_of.get(&key) {
            Some(&u) => u,
            None => {
                let u = UnitId(units.len() as u32);
                // Unit names are unique; a package rarely has two
                // script units, and the second gets a counter.
                let n = per_package.entry(build.crate_id.name.clone()).or_default();
                *n += 1;
                let name = match *n {
                    1 => format!("{}@build-script", build.crate_id.name),
                    n => format!("{}@build-script-{n}", build.crate_id.name),
                };
                units.push(BuildUnit {
                    name,
                    crate_name: "build_script_build".into(),
                    origin: UnitOrigin::Vendor(id),
                    package: PackageKey {
                        name: build.crate_id.name.clone(),
                        version: build.crate_id.version.clone(),
                        source: PackageSource::Registry { checksum: None },
                    },
                    kind: CrateType::Bin,
                    platform: Platform::Host,
                    // Set to the fixed host profile by
                    // `assign_host_profile`, like every vendored host
                    // unit.
                    profile: UnitProfile::Active,
                    sysroot: false,
                    config_crate: false,
                    src: script.path.clone(),
                    edition: script.edition.clone(),
                    // Scripts test features with `cfg!(feature = ..)`.
                    features: build.features.clone(),
                    deps,
                    order_deps: Vec::new(),
                    settings: UnitSettings {
                        cap_lints: true,
                        always_rustc: true,
                        ..UnitSettings::default()
                    },
                    metadata: Metadata::default(),
                    is_build_script: true,
                });
                script_of.insert(key, u);
                u
            }
        };
        runs.push(ScriptRun {
            script: script_unit,
            owner: unit_of_vendor[&id],
            package: script.package.clone(),
        });
    }
    Ok(runs)
}

/// Add a script unit for every manifest unit whose package has a build
/// script, and return one run per owner. Owners name their script unit
/// (`CargoBuildScriptDef::unit`); the resolver gives owners that compile
/// it identically the same name, so they share one unit.
fn lower_manifest_scripts(
    units: &mut Vec<BuildUnit>,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    unit_of_crate: &BTreeMap<Handle<CrateDef>, UnitId>,
) -> Result<Vec<ScriptRun>> {
    let mut script_of: BTreeMap<String, UnitId> = BTreeMap::new();
    let mut runs = Vec::new();
    for cref in &resolved.crates {
        let def = crate_def(model, cref.handle)?;
        let Some(cargo) = &def.cargo else { continue };
        let Some(script) = &cargo.build_script else {
            continue;
        };
        let owner = unit_of_crate[&cref.handle];
        let script_unit = match script_of.get(&script.unit) {
            Some(&u) => u,
            None => {
                let mut deps = Vec::with_capacity(script.deps.len());
                for (extern_name, crate_name) in &script.deps {
                    let dep = model
                        .crates
                        .lookup(crate_name)
                        .and_then(|h| unit_of_crate.get(&h).copied())
                        .ok_or_else(|| {
                            Error::Compile(format!(
                                "unit lowering: build script of '{}' depends on '{crate_name}', \
                                 which is not in this build (manifest lowering bug)",
                                def.name
                            ))
                        })?;
                    deps.push(UnitDep {
                        extern_name: extern_name.clone(),
                        target: DepTarget::Unit(dep),
                    });
                }
                let owner_unit = &units[owner.index()];
                let mut env = cargo.env.clone();
                env.remove("CARGO_BIN_NAME");
                env.remove("CARGO_PRIMARY_PACKAGE");
                env.insert("CARGO_CRATE_NAME".into(), "build_script_build".into());
                let mut settings = cargo_settings(cargo, &[]);
                settings.env = env;
                let u = UnitId(units.len() as u32);
                units.push(BuildUnit {
                    name: script.unit.clone(),
                    crate_name: "build_script_build".into(),
                    origin: UnitOrigin::Crate(cref.handle),
                    package: owner_unit.package.clone(),
                    kind: CrateType::Bin,
                    platform: Platform::Host,
                    // Set to the fixed host profile by
                    // `assign_host_profile`.
                    profile: UnitProfile::Active,
                    sysroot: false,
                    config_crate: false,
                    src: resolved.project_root.join(&def.path).join(&script.path),
                    edition: script.edition.clone(),
                    // Scripts test features with `cfg!(feature = ..)`.
                    features: owner_unit.features.clone(),
                    deps,
                    order_deps: Vec::new(),
                    settings: UnitSettings {
                        cap_lints: owner_unit.settings.cap_lints,
                        cargo: Some(settings),
                        ..UnitSettings::default()
                    },
                    metadata: Metadata::default(),
                    is_build_script: true,
                });
                script_of.insert(script.unit.clone(), u);
                u
            }
        };
        runs.push(ScriptRun {
            script: script_unit,
            owner,
            package: package_info(&cargo.env, script.links.clone()),
        });
    }
    Ok(runs)
}

/// The package fields a script's environment needs, read back from the
/// `CARGO_PKG_*` and `CARGO_MANIFEST_*` values manifest lowering set
/// (`manifest::resolve::package_env`, which sets every variable, empty
/// when the manifest has no value).
fn package_info(
    env: &BTreeMap<String, String>,
    links: Option<String>,
) -> crate::build_script::PackageInfo {
    let get = |k: &str| env.get(k).cloned().unwrap_or_default();
    let opt = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
    crate::build_script::PackageInfo {
        name: get("CARGO_PKG_NAME"),
        version: get("CARGO_PKG_VERSION"),
        authors: get("CARGO_PKG_AUTHORS")
            .split(':')
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect(),
        description: opt("CARGO_PKG_DESCRIPTION"),
        homepage: opt("CARGO_PKG_HOMEPAGE"),
        repository: opt("CARGO_PKG_REPOSITORY"),
        license: opt("CARGO_PKG_LICENSE"),
        license_file: opt("CARGO_PKG_LICENSE_FILE"),
        rust_version: opt("CARGO_PKG_RUST_VERSION"),
        readme: opt("CARGO_PKG_README"),
        links,
        manifest_dir: PathBuf::from(get("CARGO_MANIFEST_DIR")),
        manifest_path: PathBuf::from(get("CARGO_MANIFEST_PATH")),
    }
}

/// Config crate hash for every target that has a unit linking it, and
/// for the profile's target, whose config crate the DAG builds
/// unconditionally (a manifest project whose units all sit in groups
/// without `config(true)` links none). A rendering error, such as a
/// config-option name collision, fails lowering only when some unit
/// links the config crate.
fn config_crate_hashes(
    units: &[BuildUnit],
    model: &BuildModel,
    resolved: &ResolvedConfig,
) -> Result<BTreeMap<Handle<TargetDef>, Metadata>> {
    let linked: BTreeSet<Handle<TargetDef>> = units
        .iter()
        .filter(|u| u.config_crate)
        .filter_map(|u| match u.platform {
            Platform::Cross(t) => Some(t),
            Platform::Host => None,
        })
        .collect();
    let mut targets = linked.clone();
    targets.extend(resolved.profile.cross_target());
    let source = match crate::generated::config_crate::render_source(resolved) {
        Ok(s) => s,
        Err(_) if linked.is_empty() => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
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
            let mut platform = metadata::platform_identity(unit.platform, model, resolved)?;
            // The host tests resolve their own copy of each host package,
            // and the scheduler builds both copies in one pass. When the
            // copies' features and deps match, nothing else in the hash
            // tells them apart, so both would write one file in
            // parallel. A build-script unit is the exception: one script
            // binary serves every owner, and each owner's run gets its
            // own directory from the owner's hash.
            if !unit.is_build_script && host_test_copy(model, unit) {
                platform.push_str("\0host-test");
            }
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

/// Fail when two units would write the same output file. The scheduler
/// builds units in parallel, so a shared path is a race: one unit's
/// output replaces the other's while its dependents read it, and each
/// build marks the other's dependents stale. It means two units the
/// metadata hash cannot tell apart, which is a lowering bug.
///
/// An output path is the unit's platform and profile directory, plus
/// its crate name and metadata hash, so equal tuples mean equal paths.
fn check_unique_outputs(units: &[BuildUnit]) -> Result<()> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for u in units {
        let key = format!(
            "{:?}/{:?}/{}/{}/{}",
            u.platform,
            u.profile,
            u.is_build_script,
            u.crate_name,
            u.metadata.hex()
        );
        if let Some(other) = seen.insert(key, &u.name) {
            return Err(Error::Compile(format!(
                "units '{other}' and '{}' would write the same output file \
                 (crate '{}', hash {}); this is a Quon bug",
                u.name,
                u.crate_name,
                u.metadata.short()
            )));
        }
    }
    Ok(())
}

/// Whether `unit` is lowered from the host tests' copy of a package.
fn host_test_copy(model: &BuildModel, unit: &BuildUnit) -> bool {
    match unit.origin {
        UnitOrigin::Crate(h) => model
            .crates
            .get(h)
            .and_then(|c| c.cargo.as_ref())
            .is_some_and(|c| c.host_test),
        UnitOrigin::Vendor(_) => false,
    }
}

/// Resolve a declared crate's external dep: the vendor unit when the
/// cross pipeline built one (on the dep's platform, or on the host for
/// a proc macro), else the cargo-built host artifact, else
/// [`DepTarget::Missing`].
fn external_dep_target(
    model: &BuildModel,
    vendor_ctx: &VendorBuildContext,
    unit_of_vendor: &BTreeMap<VendorBuildId, UnitId>,
    ext: &quon_model::ExternalDepDef,
    extern_name: &str,
) -> DepTarget {
    if let Some(u) = vendor_ctx
        .lookup(ext.target, extern_name)
        .and_then(|id| unit_of_vendor.get(&id))
    {
        return DepTarget::Unit(*u);
    }
    // A proc macro declared as a cross dep is placed on the host only
    // (`vendor::cross::graph`), so its unit is under `Host`.
    if let Some(u) = vendor_ctx
        .lookup(TargetSpec::Host, extern_name)
        .filter(|id| {
            vendor_ctx
                .get(*id)
                .is_some_and(|b| b.kind == CrateKind::ProcMacro)
        })
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
    // gives each unit its own directory; `quon vendor` records all of
    // them in `quon.lock` (`external_search_dirs`).
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
        if matches!(u.origin, UnitOrigin::Vendor(_)) && !u.is_build_script && count[&u.name] > 1 {
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

/// Put proc macros, build scripts, vendored host units, and every host
/// unit they depend on, on the fixed host profile. See the module docs.
fn assign_host_profile(units: &mut [BuildUnit]) {
    let mut stack: Vec<UnitId> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            u.platform == Platform::Host
                && (u.kind == CrateType::ProcMacro
                    || u.is_build_script
                    || matches!(u.origin, UnitOrigin::Vendor(_)))
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
    use quon_model::{
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
                    build_std: false,
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
                    target: TargetSpec::Cross(target),
                    opt_level: quon_model::OptLevel::Zero,
                    debug_info: true,
                    lto: None,
                    run_binary: None,
                    runner: None,
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
                    cargo: None,
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
            build_script: None,
        }
    }

    /// `vendor_lib` with a build script whose build-deps are `build_deps`
    /// (host crates) and whose package declares `links`.
    fn vendor_scripted(
        name: &str,
        target: TargetSpec,
        deps: Vec<VendorDepEdge>,
        build_deps: &[&str],
        links: Option<&str>,
    ) -> VendorCrateBuild {
        let mut b = vendor_lib(name, target, deps);
        b.build_script = Some(crate::vendor::cross::graph::VendorBuildScript {
            path: PathBuf::from(format!("/vendor/{name}/build.rs")),
            edition: "2021".into(),
            build_deps: build_deps
                .iter()
                .map(|d| VendorDepEdge {
                    extern_name: d.replace('-', "_"),
                    crate_id: CrateId {
                        name: d.to_string(),
                        version: "1.2.3".into(),
                    },
                    target: TargetSpec::Host,
                })
                .collect(),
            package: crate::build_script::PackageInfo {
                name: name.into(),
                version: "1.2.3".into(),
                links: links.map(str::to_owned),
                manifest_dir: PathBuf::from(format!("/vendor/{name}")),
                manifest_path: PathBuf::from(format!("/vendor/{name}/Cargo.toml")),
                ..Default::default()
            },
        });
        b
    }

    fn edge(name: &str, target: TargetSpec) -> VendorDepEdge {
        VendorDepEdge {
            extern_name: name.replace('-', "_"),
            crate_id: CrateId {
                name: name.into(),
                version: "1.2.3".into(),
            },
            target,
        }
    }

    #[test]
    fn vendored_build_script_is_one_host_unit_with_a_run_per_owner() {
        let fx = Fixture::new();
        let cross = TargetSpec::Cross(fx.target);
        let mut vctx = VendorBuildContext::new();
        let cc = vctx
            .add(vendor_lib("cc", TargetSpec::Host, vec![]))
            .unwrap();
        let host = vctx
            .add(vendor_scripted(
                "zc",
                TargetSpec::Host,
                vec![],
                &["cc"],
                None,
            ))
            .unwrap();
        let target = vctx
            .add(vendor_scripted("zc", cross, vec![], &["cc"], None))
            .unwrap();
        let units = fx.lower(&vctx);
        let [cc, host, target] = [cc, host, target].map(|v| units.unit_of_vendor(v).unwrap());

        let runs: Vec<&ScriptRun> = units.runs().map(|(_, r)| r).collect();
        assert_eq!(runs.len(), 2, "one run per owner");
        assert_eq!(runs[0].owner, host);
        assert_eq!(runs[1].owner, target);
        assert_eq!(
            runs[0].script, runs[1].script,
            "same package and features compile one script"
        );
        let script = units.get(runs[0].script);
        assert!(script.is_build_script);
        assert_eq!(script.crate_name, "build_script_build");
        assert_eq!(script.kind, CrateType::Bin);
        assert_eq!(script.platform, Platform::Host);
        assert_eq!(script.profile, UnitProfile::Host);
        assert!(script.settings.cap_lints && script.settings.always_rustc);
        assert_eq!(script.src, PathBuf::from("/vendor/zc/build.rs"));
        assert_eq!(
            script.deps,
            vec![UnitDep {
                extern_name: "cc".into(),
                target: DepTarget::Unit(cc),
            }]
        );

        let layout = BuildLayout::new("/proj/build", "demo");
        let h16 = script.metadata.short();
        let paths = units
            .paths(runs[0].script, &layout, &fx.model, &fx.resolved)
            .unwrap();
        assert_eq!(
            paths.output_path,
            PathBuf::from(format!(
                "/proj/build/host/build/zc-{h16}/build_script_build-{h16}{}",
                std::env::consts::EXE_SUFFIX
            ))
        );
        assert_eq!(
            units.label(runs[0].script, &fx.model, &fx.resolved),
            "build script of vendored crate zc 1.2.3 (compile)"
        );
        assert_eq!(
            units
                .cache_key(runs[0].script, &layout, &fx.model, &fx.resolved)
                .unwrap(),
            format!("vendor:host:zc:1.2.3:build-script:{h16}")
        );

        let dag = build_dag(&fx.resolved, &fx.model, &units).unwrap();
        let id = |n: DagNode| dag.node_index[&n];
        for (r, owner) in [(RunId(0), host), (RunId(1), target)] {
            let run = id(DagNode::RunBuildScript(r));
            assert!(dag.edges_out[&id(DagNode::Compile(runs[0].script))].contains(&run));
            assert!(dag.edges_out[&run].contains(&id(DagNode::Compile(owner))));
        }
        assert!(
            dag.edges_out[&id(DagNode::Compile(cc))]
                .contains(&id(DagNode::Compile(runs[0].script)))
        );
    }

    #[test]
    fn different_features_compile_two_scripts() {
        let fx = Fixture::new();
        let cross = TargetSpec::Cross(fx.target);
        let mut vctx = VendorBuildContext::new();
        vctx.add(vendor_scripted("zc", TargetSpec::Host, vec![], &[], None))
            .unwrap();
        let mut b = vendor_scripted("zc", cross, vec![], &[], None);
        b.features = vec!["derive".into()];
        vctx.add(b).unwrap();
        let units = fx.lower(&vctx);
        let scripts: Vec<UnitId> = units.runs().map(|(_, r)| r.script).collect();
        assert_ne!(scripts[0], scripts[1]);
        assert_eq!(units.get(scripts[1]).features, ["derive"]);
        assert_ne!(units.get(scripts[0]).name, units.get(scripts[1]).name);
    }

    #[test]
    fn run_of_a_links_dep_feeds_the_dependents_run() {
        // consumer (script) → probe-sys (script, links = "probe"), and
        // consumer → plain (script, no links).
        let fx = Fixture::new();
        let cross = TargetSpec::Cross(fx.target);
        let mut vctx = VendorBuildContext::new();
        let probe = vctx
            .add(vendor_scripted(
                "probe-sys",
                cross,
                vec![],
                &[],
                Some("probe"),
            ))
            .unwrap();
        let plain = vctx
            .add(vendor_scripted("plain", cross, vec![], &[], None))
            .unwrap();
        let consumer = vctx
            .add(vendor_scripted(
                "consumer",
                cross,
                vec![edge("plain", cross), edge("probe-sys", cross)],
                &[],
                None,
            ))
            .unwrap();
        let units = fx.lower(&vctx);
        let run_of = |v| {
            units
                .build_script_of(units.unit_of_vendor(v).unwrap())
                .unwrap()
        };
        let (probe, plain, consumer) = (run_of(probe), run_of(plain), run_of(consumer));
        assert_eq!(units.links_dep_runs(consumer), vec![probe]);

        let dag = build_dag(&fx.resolved, &fx.model, &units).unwrap();
        let id = |r| dag.node_index[&DagNode::RunBuildScript(r)];
        assert!(dag.edges_out[&id(probe)].contains(&id(consumer)));
        assert!(!dag.edges_out[&id(plain)].contains(&id(consumer)));
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

    /// A proc macro declared as a cross dep has only a host unit
    /// (placement by reachability), so a declared crate's edge to it
    /// must resolve to that host unit.
    #[test]
    fn declared_crate_dep_on_a_vendored_proc_macro_resolves_to_its_host_unit() {
        let mut fx = Fixture::new();
        let (ext, _) = fx.model.external_deps.insert(
            "hello-derive".into(),
            ExternalDepDef {
                name: "hello-derive".into(),
                source: DepSource::CratesIo {
                    version: "0.1".into(),
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
            CrateType::Lib,
            false,
            &[("hello_derive", DepResolution::External(ext))],
        );
        let mut vctx = VendorBuildContext::new();
        let mut pm = vendor_lib("hello-derive", TargetSpec::Host, vec![]);
        pm.kind = CrateKind::ProcMacro;
        let vid = vctx.add(pm).unwrap();
        let units = fx.lower(&vctx);
        let k = units.get(units.unit_of(kernel).unwrap());
        assert_eq!(
            k.deps[0].target,
            DepTarget::Unit(units.unit_of_vendor(vid).unwrap())
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
        assert_eq!(meta(&units, kernel).short(), "a40f22247f6edc19");
        assert_eq!(
            units
                .get(units.unit_of_vendor(vid).unwrap())
                .metadata
                .short(),
            "f79c960a19482999"
        );
        assert_eq!(
            units.config_metadata(fx.target).unwrap().short(),
            "730bec2b770ecb2d"
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
                    .insert("LOG".into(), quon_model::ResolvedValue::Bool(true));
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
                fx.resolved.profile.opt_level = quon_model::OptLevel::Three;
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
                .insert("LOG".into(), quon_model::ResolvedValue::Bool(log));
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

    // ---------------------------------------------------------------
    // Build-script runs, synthetic: a declared host bin stands in for
    // the script, so the data path is tested without a vendor graph.
    // ---------------------------------------------------------------

    mod script_runs {
        use super::*;
        use crate::build_script::{BuildScriptOutput, LinkArg, LinkArgTarget, PackageInfo};
        use crate::compile::command_builder::{build_unit_command, closure_link_search};
        use crate::compile::{ArtifactMap, CompileCtx, CompileUnitInput, RustcInfo};
        use std::sync::Arc;

        /// `pkg_build` (host bin, the script) runs for `pkg` (cross
        /// lib); `mid` (cross lib) uses `pkg`; `kernel` (cross bin)
        /// uses `mid`.
        struct Runs {
            fx: Fixture,
            units: UnitGraph,
            run: RunId,
            script: UnitId,
            pkg: UnitId,
            mid: UnitId,
            kernel: UnitId,
        }

        fn runs() -> Runs {
            let mut fx = Fixture::new();
            let script = fx.add_crate("pkg_build", CrateType::Bin, true, &[]);
            let pkg = fx.add_crate("pkg", CrateType::Lib, false, &[]);
            let mid = fx.add_crate(
                "mid",
                CrateType::Lib,
                false,
                &[("pkg", DepResolution::Project(pkg))],
            );
            let kernel = fx.add_crate(
                "kernel",
                CrateType::Bin,
                false,
                &[("mid", DepResolution::Project(mid))],
            );
            let mut units = fx.lower(&VendorBuildContext::new());
            let [script, pkg, mid, kernel] =
                [script, pkg, mid, kernel].map(|h| units.unit_of(h).unwrap());
            let run = units
                .add_script_run(ScriptRun {
                    script,
                    owner: pkg,
                    package: PackageInfo {
                        name: "pkg".into(),
                        version: "0.0.0".into(),
                        manifest_dir: PathBuf::from("/proj/crates/pkg"),
                        manifest_path: PathBuf::from("/proj/crates/pkg/Cargo.toml"),
                        ..PackageInfo::default()
                    },
                })
                .unwrap();
            Runs {
                fx,
                units,
                run,
                script,
                pkg,
                mid,
                kernel,
            }
        }

        fn output(cfg: &str) -> BuildScriptOutput {
            BuildScriptOutput {
                cfgs: vec![cfg.into()],
                check_cfgs: vec!["cfg(has_feature_x)".into()],
                env: vec![("PKG_GENERATED".into(), "yes".into())],
                link_libs: vec!["static=pkgnative".into()],
                link_search: vec!["native=/opt/pkg/lib".into()],
                link_args: vec![LinkArg {
                    applies_to: LinkArgTarget::All,
                    arg: "-Tpkg.ld".into(),
                }],
                ..BuildScriptOutput::default()
            }
        }

        fn ctx(root: &std::path::Path) -> CompileCtx {
            CompileCtx::new(
                BuildLayout::new(root.join("build"), "demo"),
                Arc::new(RustcInfo {
                    rustc_path: PathBuf::from("/usr/bin/rustc"),
                    rustc_arg: "rustc".into(),
                    version: "rustc 0.0.0".into(),
                    host_triple: "x86_64-unknown-linux-gnu".into(),
                    commit_hash: None,
                    release: "0.0.0".into(),
                    sysroot: PathBuf::from("/fake-sysroot"),
                    rust_src: None,
                    mtime_ns: 0,
                }),
            )
        }

        /// Every unit finished, at fake paths, plus the config crate.
        fn finished(r: &Runs) -> ArtifactMap {
            let mut map = ArtifactMap::new();
            for (id, u) in r.units.iter() {
                map.insert_unit(id, PathBuf::from(format!("/out/lib{}.rlib", u.crate_name)));
            }
            map.set_config_crate(r.fx.target, PathBuf::from("/out/libdemo_config.rlib"));
            map
        }

        fn argv(r: &Runs, unit: UnitId, map: &ArtifactMap) -> (Vec<String>, [u8; 32]) {
            let ctx = ctx(std::path::Path::new("/proj"));
            let cmd = build_unit_command(
                &ctx,
                &CompileUnitInput {
                    model: &r.fx.model,
                    resolved: &r.fx.resolved,
                    units: &r.units,
                    unit,
                    artifacts: map,
                    sysroot_dir: Some(std::path::Path::new("/sysroot")),
                },
            )
            .unwrap();
            let args = cmd
                .builder
                .args()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            (args, cmd.builder.hash())
        }

        fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
            args.windows(2).any(|w| w[0] == flag && w[1] == value)
        }

        #[test]
        fn run_node_sits_between_script_and_owner() {
            let r = runs();
            let dag = build_dag(&r.fx.resolved, &r.fx.model, &r.units).unwrap();
            let id = |n: DagNode| dag.node_index[&n];
            let run = id(DagNode::RunBuildScript(r.run));
            assert!(dag.edges_out[&id(DagNode::Compile(r.script))].contains(&run));
            assert!(dag.edges_out[&id(DagNode::Sysroot(r.fx.target))].contains(&run));
            assert!(dag.edges_out[&run].contains(&id(DagNode::Compile(r.pkg))));
            assert_eq!(dag.edges_out[&run].len(), 1, "only the owner reads the run");
        }

        #[test]
        fn an_owner_has_one_run() {
            let mut r = runs();
            let again = r.units.run(r.run).clone();
            assert!(r.units.add_script_run(again).is_err());
        }

        #[test]
        fn run_output_reaches_the_owner_command() {
            let r = runs();
            let mut map = finished(&r);
            map.set_build_script(r.run, output("has_x"));
            let ctx = ctx(std::path::Path::new("/proj"));
            let cmd = build_unit_command(
                &ctx,
                &CompileUnitInput {
                    model: &r.fx.model,
                    resolved: &r.fx.resolved,
                    units: &r.units,
                    unit: r.pkg,
                    artifacts: &map,
                    sysroot_dir: Some(std::path::Path::new("/sysroot")),
                },
            )
            .unwrap();
            let args: Vec<String> = cmd
                .builder
                .args()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(has_pair(&args, "--cfg", "has_x"), "{args:?}");
            assert!(
                has_pair(&args, "--check-cfg", "cfg(has_feature_x)"),
                "{args:?}"
            );
            assert!(has_pair(&args, "-l", "static=pkgnative"), "{args:?}");
            assert!(has_pair(&args, "-L", "native=/opt/pkg/lib"), "{args:?}");
            assert!(
                !args.iter().any(|a| a.starts_with("link-arg=")),
                "libraries take no link args: {args:?}"
            );
            let envs = cmd.builder.envs();
            let out_dir = r
                .units
                .run_dir(r.run, &ctx.layout, &r.fx.model, &r.fx.resolved)
                .unwrap()
                .join("out");
            assert_eq!(
                envs.get(std::ffi::OsStr::new("OUT_DIR")),
                Some(&out_dir.into_os_string())
            );
            assert_eq!(
                envs.get(std::ffi::OsStr::new("PKG_GENERATED")),
                Some(&std::ffi::OsString::from("yes"))
            );
        }

        #[test]
        fn changed_output_changes_the_owner_command_and_same_output_does_not() {
            let r = runs();
            let mut map = finished(&r);
            map.set_build_script(r.run, output("has_x"));
            let (_, first) = argv(&r, r.pkg, &map);
            map.set_build_script(r.run, output("has_x"));
            let (_, rerun) = argv(&r, r.pkg, &map);
            assert_eq!(
                first, rerun,
                "a rerun with the same output must not rebuild"
            );
            map.set_build_script(r.run, output("has_y"));
            let (_, changed) = argv(&r, r.pkg, &map);
            assert_ne!(first, changed, "a changed output must rebuild the owner");
        }

        #[test]
        fn owner_without_its_run_finished_is_a_scheduler_bug() {
            let r = runs();
            let map = finished(&r);
            let ctx = ctx(std::path::Path::new("/proj"));
            let err = build_unit_command(
                &ctx,
                &CompileUnitInput {
                    model: &r.fx.model,
                    resolved: &r.fx.resolved,
                    units: &r.units,
                    unit: r.pkg,
                    artifacts: &map,
                    sysroot_dir: Some(std::path::Path::new("/sysroot")),
                },
            )
            .err()
            .expect("must fail");
            assert!(err.to_string().contains("scheduler bug"), "{err}");
        }

        #[test]
        fn link_search_from_a_transitive_dep_reaches_the_bin() {
            let r = runs();
            let mut map = finished(&r);
            map.set_build_script(r.run, output("has_x"));
            // What each Compile node stores in its result, bottom-up.
            let pkg_ls = closure_link_search(&r.units, r.pkg, &map);
            assert_eq!(pkg_ls, ["native=/opt/pkg/lib"]);
            map.set_link_search(r.pkg, pkg_ls);
            let mid_ls = closure_link_search(&r.units, r.mid, &map);
            assert_eq!(
                mid_ls,
                ["native=/opt/pkg/lib"],
                "mid has no script of its own"
            );
            map.set_link_search(r.mid, mid_ls);

            let (args, _) = argv(&r, r.kernel, &map);
            assert!(has_pair(&args, "-L", "native=/opt/pkg/lib"), "{args:?}");
            // The script's own flags stay with its owner.
            assert!(!args.iter().any(|a| a == "has_x" || a == "static=pkgnative"));
        }

        #[test]
        fn link_args_apply_to_executables() {
            let mut r = runs();
            // Move the run onto the kernel to see an executable take it.
            let mut units = r.fx.lower(&VendorBuildContext::new());
            let mut package = r.units.run(r.run).package.clone();
            package.name = "kernel".into();
            r.run = units
                .add_script_run(ScriptRun {
                    script: r.script,
                    owner: r.kernel,
                    package,
                })
                .unwrap();
            r.units = units;
            let mut map = finished(&r);
            map.set_build_script(r.run, output("has_x"));
            let (args, _) = argv(&r, r.kernel, &map);
            assert!(has_pair(&args, "-C", "link-arg=-Tpkg.ld"), "{args:?}");
        }

        /// The owner compiled with real rustc: fresh when the run's
        /// output is unchanged, rebuilt when it changes.
        #[test]
        #[ignore]
        fn owner_rebuilds_only_when_run_output_changes() {
            let Ok(info) = RustcInfo::probe() else {
                eprintln!("skipped: rustc probe failed");
                return;
            };
            let tmp = tempfile::tempdir().unwrap();
            let mut fx = Fixture::new();
            fx.resolved.project_root = tmp.path().to_path_buf();
            let script = fx.add_crate("pkg_build", CrateType::Bin, true, &[]);
            let pkg = fx.add_crate("pkg", CrateType::Lib, true, &[]);
            let src = tmp.path().join("crates/pkg/src/lib.rs");
            std::fs::create_dir_all(src.parent().unwrap()).unwrap();
            std::fs::write(
                &src,
                "#[cfg(has_x)] pub const X: u32 = 1;\n#[cfg(has_y)] pub const Y: u32 = 2;\n",
            )
            .unwrap();
            let mut units = fx.lower(&VendorBuildContext::new());
            let (script, pkg) = (units.unit_of(script).unwrap(), units.unit_of(pkg).unwrap());
            let run = units
                .add_script_run(ScriptRun {
                    script,
                    owner: pkg,
                    package: PackageInfo::default(),
                })
                .unwrap();
            let ctx = CompileCtx::new(
                BuildLayout::new(tmp.path().join("build"), "demo"),
                Arc::new(info),
            );
            let compile = |cfg: &str| {
                let mut map = ArtifactMap::new();
                map.set_build_script(
                    run,
                    BuildScriptOutput {
                        cfgs: vec![cfg.into()],
                        ..BuildScriptOutput::default()
                    },
                );
                crate::compile::compile_unit(
                    &ctx,
                    CompileUnitInput {
                        model: &fx.model,
                        resolved: &fx.resolved,
                        units: &units,
                        unit: pkg,
                        artifacts: &map,
                        sysroot_dir: None,
                    },
                    &mut Vec::new(),
                )
                .expect("compile")
                .1
            };
            assert!(!compile("has_x"), "cold build");
            assert!(compile("has_x"), "same run output must stay fresh");
            assert!(!compile("has_y"), "changed run output must rebuild");
        }
    }
}
