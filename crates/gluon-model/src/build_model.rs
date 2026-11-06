use crate::handle::{Arena, Handle, Named};
use crate::kconfig::{ConfigOptionDef, ConfigValue, PresetDef};
use crate::source::SourceSpan;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub(crate) fn default_true() -> bool {
    true
}

/// The complete build model produced by evaluating `gluon.rs`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BuildModel {
    pub project: Option<ProjectDef>,
    pub targets: Arena<TargetDef>,
    pub profiles: Arena<ProfileDef>,
    pub groups: Arena<GroupDef>,
    pub crates: Arena<CrateDef>,
    pub rules: Arena<RuleDef>,
    pub pipelines: Arena<PipelineDef>,
    pub external_deps: Arena<ExternalDepDef>,
    /// Map of `(target, cargo-target-name)` → absolute path of the
    /// rlib (for libs) or dylib (for proc-macros) that satisfies it.
    ///
    /// Populated by `vendor::auto_register_vendored_deps` from
    /// `gluon.lock` after `gluon vendor` has run. Consumed by the
    /// compile path: `compile::command_builder` looks up each
    /// `DepResolution::External(handle)` against this map (using the
    /// dep's declared target and the cargo-normalised crate name) and
    /// emits `--extern <alias>=<path>` for the consumer's rustc
    /// invocation.
    ///
    /// `target = Host` covers the cargo-as-builder pipeline (host
    /// proc-macros, host helper crates). Cross-target keys cover the
    /// gluon-driven rustc pipeline added in v2 (e.g. `bitflags`
    /// compiled against the kernel target with the gluon-built
    /// sysroot).
    ///
    /// Runtime-only — never serialised. The on-disk source of truth
    /// is `gluon.lock`; the in-memory map is a load-time cache that
    /// the compile path consults. `#[serde(skip)]` keeps the model's
    /// canonical JSON shape stable across vendor reruns and avoids
    /// the JSON-string-key constraint a `BTreeMap` with a struct key
    /// would otherwise impose.
    #[serde(skip)]
    pub external_artifacts: BTreeMap<ExternalArtifactKey, PathBuf>,
    /// Directories holding the host vendor build's libraries,
    /// transitive deps included. `compile::command_builder` adds one
    /// `-L dependency=` per entry to any crate with a host external
    /// dep. Populated from `gluon.lock` alongside
    /// [`Self::external_artifacts`]; runtime-only for the same reasons.
    #[serde(skip)]
    pub external_search_dirs: BTreeSet<PathBuf>,
    /// EFI System Partition declarations. Each entry describes a set of
    /// compiled artifacts to be assembled into a FAT-layout directory
    /// under `build/cross/<target>/<profile>/esp/<name>/`. Consumed by
    /// the `EspBuild` scheduler node at build time and auto-wired into
    /// `gluon run --uefi` when there is exactly one declaration.
    #[serde(default)]
    pub esps: Arena<EspDef>,
    /// Config options live in an `Arena` so the model has a uniform handle
    /// shape across all named declarations. External references to options
    /// are still by name string (resolver, kconfig, etc.) — they go through
    /// `arena.lookup(name)` to obtain a handle.
    pub config_options: Arena<ConfigOptionDef>,
    pub presets: BTreeMap<String, PresetDef>,
    /// Bootloader configuration (singleton per project). `None` when the
    /// `gluon.rs` did not declare a `cfg.bootloader(...)` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootloader: Option<BootloaderDef>,
    /// Disk image definitions. Zero or more named images describing how
    /// to assemble bootable disk images from build artifacts.
    #[serde(default)]
    pub images: Arena<ImageDef>,
    /// Build-step declarations. Each entry binds a named build step to
    /// a runner host-bin crate, a set of input env vars, literal env
    /// vars, and a single output filename. Outputs land in
    /// `ArtifactMap` under the build step's name and can be referenced
    /// anywhere a crate name is accepted (image entries, qemu disks,
    /// other build-step inputs).
    #[serde(default)]
    pub build_steps: Arena<BuildStepDef>,
    /// Runners (`cfg.runner(...)`): how `gluon run`, the integration
    /// tests and the boot tests execute a profile's run binary. A
    /// profile picks one with `.runner(name)`.
    #[serde(default)]
    pub runners: Arena<RunnerDef>,
    /// Pinned downloads (`cfg.download(...)`, `cfg.limine(...)`).
    /// ESP and image entries refer to them by name.
    #[serde(default)]
    pub downloads: Arena<DownloadDef>,
    /// Boot test suites (`cfg.boot_test(...)`), run by `gluon test`.
    #[serde(default)]
    pub boot_tests: Arena<BootTestDef>,
    /// Environment of every host test run (`cfg.host_tests().env(..)`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_test_env: BTreeMap<String, HostTestEnv>,
}

/// The value of a variable set for host test runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostTestEnv {
    /// Set as written: `.env(name, value)`.
    Value(String),
    /// A path relative to the project root, made absolute when the tests
    /// run, since each runs from its own package directory:
    /// `.env_path(name, path)`.
    Path(PathBuf),
}

/// A boot test suite: `gluon test` builds `profile`, boots its boot
/// binary, and passes the suite when QEMU exits with the success exit
/// code. What the kernel runs once booted is up to the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootTestDef {
    pub name: String,
    pub profile: Handle<ProfileDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for BootTestDef {
    fn name(&self) -> &str {
        &self.name
    }
}

/// A file fetched over HTTPS and checked against a pinned SHA-256.
/// `gluon-core::download` fetches it the first time an ESP or image
/// entry needs it and caches it under `build/downloads/<sha256>/`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadDef {
    pub name: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file. Empty means "not pinned
    /// yet": the build fails and prints the fetched file's hash.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl DownloadDef {
    /// File name of the cached copy: the URL's last path segment, so
    /// `build/downloads/` stays readable.
    pub fn file_name(&self) -> &str {
        let path = self.url.split(['?', '#']).next().unwrap_or_default();
        match path.rsplit('/').next() {
            Some(name) if !name.is_empty() && name != "." && name != ".." => name,
            _ => "download",
        }
    }
}

impl Named for DownloadDef {
    fn name(&self) -> &str {
        &self.name
    }
}

/// Project metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectDef {
    pub name: String,
    pub version: String,
    /// Override for the generated config crate's name. Resolved to
    /// `<project>_config` at resolve time when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_crate_name: Option<String>,
    /// Override for the cfg flag prefix. Resolved to a sanitised `<project>`
    /// at resolve time when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfg_prefix: Option<String>,
    /// Override for the per-developer config file path. Resolved to
    /// `.gluon-config` at resolve time when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_override_file: Option<PathBuf>,
    /// Name of the profile to use when the user did not pass
    /// `-p/--profile` on the command line. When `None`, the CLI falls
    /// back to the first profile in alphabetical order — which is a
    /// footgun for projects with `debug`/`dev`/`release` profiles
    /// because it silently picks `debug`. Setting this makes the
    /// intent explicit and stable across fresh clones.
    ///
    /// Validated at intern time: the named profile must exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
}

/// Panic strategy for a target — wire format is the lowercase rustc
/// spelling so existing JSON model dumps round-trip unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanicStrategy {
    Abort,
    Unwind,
}

impl PanicStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Unwind => "unwind",
        }
    }
}

/// A compilation target definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetDef {
    pub name: String,
    pub spec: String,
    /// If true, `spec` is a rustc builtin triple (not a file path).
    #[serde(default)]
    pub builtin: bool,
    /// The panic strategy rustc should use for this target. When `Some`,
    /// `-C panic=<strategy>` is passed to every rustc invocation that
    /// builds a crate for this target — including sysroot crates.
    ///
    /// Bare-metal targets almost always want `Some(PanicStrategy::Abort)`.
    /// Mixing panic strategies across sysroot rlibs and downstream crates
    /// fails at link time with `error: the crate ... is compiled with a
    /// different panic strategy`, so this must be consistent across all
    /// crates for a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_strategy: Option<PanicStrategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Crate output type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrateType {
    #[default]
    Lib,
    Bin,
    ProcMacro,
    #[serde(rename = "staticlib")]
    StaticLib,
    /// Flat-binary executable. From rustc's perspective this is still
    /// `--crate-type=bin`; gluon emits `<name>.elf` from rustc and runs
    /// a post-link `rust-objcopy -O binary` step to produce the flat
    /// `<name>.bin` that downstream consumers (`image()`,
    /// `${artifact:foo}`) resolve to. The `.elf` is preserved as a
    /// debugger-facing sidecar (`gdb`, `addr2line`, `objdump`). Linker
    /// scripts are permitted (and usually required, since flat
    /// binaries have no PT_LOAD-driven layout to fall back on).
    #[serde(rename = "raw-binary")]
    RawBinary,
    /// A libtest harness executable: rustc `--test` instead of
    /// `--crate-type`. Manifest expansion lowers host unit tests and
    /// `harness = true` integration tests of `cfg.host_tests()`
    /// packages to it.
    Test,
}

impl CrateType {
    /// String passed to rustc's `--crate-type=` flag. `RawBinary` maps
    /// to `bin` because rustc has no native concept of flat-binary
    /// output; the flat `.bin` is produced by a post-link
    /// `rust-objcopy` step, not by rustc itself.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
            Self::ProcMacro => "proc-macro",
            Self::StaticLib => "staticlib",
            Self::RawBinary => "bin",
            // Not a `--crate-type` value; `RustcCommandBuilder` passes
            // `--test` instead.
            Self::Test => "test",
        }
    }

    /// True for crate types that produce an executable (ELF or flat).
    /// Used by the rustc command builder to decide whether linker
    /// scripts and binary-output conventions apply.
    pub fn is_executable(&self) -> bool {
        matches!(self, Self::Bin | Self::RawBinary | Self::Test)
    }
}

/// A build profile definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileDef {
    pub name: String,
    /// Resolved parent profile. `None` when the profile does not inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<Handle<ProfileDef>>,
    /// Resolved target: a declared target, or the host for
    /// `.target_host()`. `None` when the profile inherits its target
    /// from a parent (resolution walks the chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetSpec>,
    pub opt_level: Option<u8>,
    pub debug_info: Option<bool>,
    pub lto: Option<String>,
    /// Resolved run binary. `None` means "use the profile's preset/default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_binary: Option<Handle<CrateDef>>,
    /// The runner that executes the run binary. `None` inherits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<Handle<RunnerDef>>,
    pub preset: Option<String>,
    pub config: BTreeMap<String, ConfigValue>,
    pub qemu_memory: Option<u32>,
    pub qemu_cores: Option<u32>,
    pub qemu_extra_args: Option<Vec<String>>,
    /// Profile-level boot-mode override. Wins over the project-level
    /// `qemu().boot_mode(...)` and is itself overridden by CLI flags
    /// (`--uefi` / `--direct` / `--bios`). Lets a single config define
    /// `dev-bios` and `dev-uefi` profiles that share crates and the
    /// disk image while diverging only on firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_boot_mode: Option<BootMode>,
    pub test_timeout: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Resolved target binding for a group or crate.
///
/// `Host` is the build-machine triple, populated by gluon-core at
/// resolve time from `rustc -vV`. `Cross(Handle<TargetDef>)` is a
/// user-defined target whose name resolves to a handle in the
/// `targets` arena.
///
/// Implements `Ord`/`Hash` so it can serve as a `BTreeMap`/`HashMap`
/// key — used by `BuildModel::external_artifacts` to disambiguate
/// host-built and cross-built copies of the same crate name.
#[derive(
    Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case", tag = "kind", content = "handle")]
pub enum TargetSpec {
    #[default]
    Host,
    Cross(Handle<TargetDef>),
}

/// Composite key for `BuildModel::external_artifacts`.
///
/// `target` is the compile target the artifact was built against.
/// `cargo_target_name` is the cargo-normalised crate name (hyphens
/// replaced with underscores: `proc-macro2` → `proc_macro2`). The
/// compile path normalises user-declared dep names to this form
/// before lookup so the map matches both the cargo-as-builder
/// pipeline (which emits underscore-normalised names from `cargo
/// metadata`) and direct user declarations.
///
/// Why a composite key rather than a nested `BTreeMap<TargetSpec,
/// BTreeMap<String, PathBuf>>`: lookups are always `(target, name)`
/// pairs — never "all artifacts for a target" or "all targets for
/// a name". Flat keying keeps the lookup a single hash/compare.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExternalArtifactKey {
    pub target: TargetSpec,
    pub cargo_target_name: String,
}

/// A group of crates with shared compilation behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDef {
    pub name: String,
    /// Target for all crates in this group. `TargetSpec::Host` = host triple.
    /// Required at the group level; groups always pin a target triple or host.
    pub target: TargetSpec,
    pub default_edition: String,
    pub crates: Vec<String>,
    pub shared_flags: Vec<String>,
    /// Whether crates in this group are project crates (for clippy linting).
    pub is_project: bool,
    /// Whether crates in this group should be linked with the config crate.
    pub config: bool,
    /// Skip sysroot and config-crate wiring for crates in this group.
    ///
    /// Set this when the group's target cannot satisfy a `core`/`alloc`/
    /// `compiler_builtins` build (e.g. 16-bit real-mode boot sectors,
    /// or any custom target whose data layout / atomic width disallows
    /// the standard library's intrinsics). Crates in a `no_sysroot`
    /// group are responsible for declaring `#![no_core]` (or an
    /// equivalent restricted attribute) themselves; they must not
    /// depend on `core`, `alloc`, or the auto-injected config crate.
    ///
    /// Caveat: the profile's running-target sysroot is still always
    /// built (the config crate compiles against it). If the profile
    /// target itself falls in a `no_sysroot` group, the build will
    /// fail trying to compile the config crate — pick a different
    /// target for the profile (kernel target, not boot-sector target).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_sysroot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Default for GroupDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            target: TargetSpec::Host,
            default_edition: "2024".into(),
            crates: Vec::new(),
            shared_flags: Vec::new(),
            is_project: true,
            config: false,
            no_sysroot: false,
            span: None,
        }
    }
}

/// A crate definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrateDef {
    pub name: String,
    pub path: String,
    pub edition: String,
    pub crate_type: CrateType,
    /// Target for this crate (inherited from group).
    pub target: TargetSpec,
    pub deps: BTreeMap<String, DepDef>,
    pub dev_deps: BTreeMap<String, DepDef>,
    pub features: Vec<String>,
    pub root: Option<String>,
    /// Per-crate linker script.
    pub linker_script: Option<String>,
    /// Resolved group. Mandatory; intern errors if absent.
    pub group: Handle<GroupDef>,
    /// Whether this crate is a project crate (for clippy linting).
    pub is_project_crate: bool,
    /// Extra `--cfg` flags for this crate.
    pub cfg_flags: Vec<String>,
    /// Extra `rustc` flags for this crate.
    pub rustc_flags: Vec<String>,
    /// Config options that must be enabled for this crate to be compiled.
    pub requires_config: Vec<String>,
    /// Ordering-only dependencies on other crates. Creates DAG edges
    /// without `--extern` flags.
    #[serde(default)]
    pub artifact_deps: Vec<String>,
    /// Environment variables to inject into rustc's process environment
    /// when compiling this crate. Each value is the *name* of another
    /// crate whose primary build output path (absolute, canonicalised)
    /// will be substituted at compile time.
    ///
    /// Use case: a bootloader crate declares
    /// `artifact_env = { "KERNEL_PATH": "kernel" }` and its source does
    /// `static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));`
    /// to embed the kernel binary. The Rhai builder auto-adds the
    /// referenced crate to `artifact_deps` so the DAG enforces ordering.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_env: BTreeMap<String, String>,
    /// Set on crates lowered from a Cargo manifest; `None` for crates
    /// declared with `group.add(...)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo: Option<CargoUnitDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// What a crate lowered from a Cargo manifest carries beyond a
/// declared crate: the data rustc needs that Cargo would pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoUnitDef {
    /// `--crate-name`: the lib name or bin name, `-` replaced by `_`.
    /// The crate's `name` is the unit name (`syn@host`), which differs.
    pub rustc_name: String,
    pub package: CargoPackageDef,
    /// Every feature the package declares, sorted, for
    /// `--check-cfg cfg(feature, values(...))`.
    pub declared_features: Vec<String>,
    /// Extra `--check-cfg` arguments from
    /// `[lints.rust.unexpected_cfgs] check-cfg = [...]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_cfg: Vec<String>,
    /// Lint levels from `[lints]`, sorted by priority, then name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lints: Vec<CargoLint>,
    /// `CARGO_PKG_*`, `CARGO_MANIFEST_*`, `CARGO_CRATE_NAME` and, where
    /// they apply, `CARGO_PRIMARY_PACKAGE` and `CARGO_BIN_NAME`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The package's build script, which runs for this unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_script: Option<CargoBuildScriptDef>,
    /// The unit is an integration test for a Gluon target. `gluon
    /// build` and `gluon run` skip it; `gluon test` builds and boots
    /// it. Host tests are [`CrateType::Test`] crates instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_test: bool,
    /// Only `gluon test` builds the unit: an integration test, a
    /// dev-dependency, or a copy of a dependency whose features or deps
    /// differ for the tests. `gluon build` and `gluon run` skip it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test_only: bool,
    /// The unit is the host tests' copy of a package: the package
    /// itself or one of its dependencies, resolved with the tests' own
    /// features. It goes into the unit metadata hash, so it never
    /// shares an output file with the build's host copy of the same
    /// package, even when their features and deps match.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_test: bool,
    /// On a host-test package's lib test: the crate name of the same
    /// package's plain lib, which its doctests link as `--extern`. Set
    /// unless the lib says `doctest = false`. See `compile::doctest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctest_lib: Option<String>,
}

impl CrateDef {
    /// Whether the crate is an integration test lowered from `tests/`
    /// that boots in QEMU.
    pub fn is_test(&self) -> bool {
        self.cargo.as_ref().is_some_and(|c| c.is_test)
    }

    /// Whether only `gluon test` builds the crate; see
    /// [`CargoUnitDef::test_only`].
    pub fn is_test_only(&self) -> bool {
        self.cargo.as_ref().is_some_and(|c| c.test_only)
    }
}

/// The build script that runs for a manifest unit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoBuildScriptDef {
    /// Name of the compiled script (`probe@build-script`). Units with
    /// the same name share one compile.
    pub unit: String,
    /// Script source, relative to the crate's `path`.
    pub path: String,
    pub edition: String,
    /// Build-dependencies: extern name → name of the host crate.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deps: BTreeMap<String, String>,
    /// The package's `links` key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<String>,
}

/// The package a manifest unit is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoPackageDef {
    pub name: String,
    pub version: String,
    /// Manifest directory, relative to the project root when it lies
    /// under it.
    pub manifest_dir: String,
    #[serde(default, skip_serializing_if = "CargoPackageSource::is_path")]
    pub source: CargoPackageSource,
}

/// Where a manifest unit's package comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CargoPackageSource {
    /// A workspace member or another path dependency.
    #[default]
    Path,
    /// A registry package, read from Cargo's registry cache or a vendor
    /// directory.
    Registry {
        /// The `.crate` checksum from `Cargo.lock`, when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checksum: Option<String>,
    },
}

impl CargoPackageSource {
    pub fn is_path(&self) -> bool {
        matches!(self, Self::Path)
    }
}

/// One `[lints]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoLint {
    /// `rust`, `clippy` or `rustdoc`.
    pub tool: String,
    pub name: String,
    /// `allow`, `warn`, `deny` or `forbid`.
    pub level: String,
    pub priority: i8,
}

/// Resolved dependency target. Project deps point to a sibling
/// crate; external deps point to an [`ExternalDepDef`] in the
/// workspace's `external_deps` arena.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "handle")]
pub enum DepResolution {
    Project(Handle<CrateDef>),
    External(Handle<ExternalDepDef>),
}

/// A dependency specification within a crate definition.
///
/// This is the strict single-form representation. The map key on
/// `CrateDef::deps` is the extern name; the [`resolution`](Self::resolution)
/// field discriminates between a sibling project crate and an external
/// dep entry in the workspace's `external_deps` arena.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepDef {
    pub resolution: DepResolution,
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// When `true`, this dependency is optional and only linked when a
    /// feature gate enables it.
    #[serde(default)]
    pub optional: bool,
    /// When `false`, the dependency's default features are suppressed.
    /// Defaults to `true` (Cargo convention).
    #[serde(default = "default_true")]
    pub default_features: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Source location for an external dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DepSource {
    /// crates.io with exact version.
    CratesIo { version: String },
    /// Git repository.
    Git { url: String, reference: GitRef },
    /// Local path (not vendored, used in-place).
    Path { path: String },
}

/// Git reference type for git-sourced dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitRef {
    Rev(String),
    Tag(String),
    Branch(String),
    Default,
}

/// An external dependency declaration from `gluon.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalDepDef {
    pub name: String,
    pub source: DepSource,
    pub features: Vec<String>,
    pub default_features: bool,
    /// Extra `--cfg` flags to pass when compiling this dependency.
    pub cfg_flags: Vec<String>,
    /// Extra `rustc` flags to pass when compiling this dependency.
    pub rustc_flags: Vec<String>,
    /// Compile target this dep is built against. `Host` (default)
    /// goes through the cargo-as-builder pipeline. `Cross(_)` goes
    /// through the gluon-driven rustc pipeline so the dep is built
    /// against the user's custom target with the gluon-built sysroot
    /// — this is how kernel-side crates (bitflags, volatile, …) get
    /// compiled.
    ///
    /// `#[serde(default)]` so persisted models from before this field
    /// existed deserialise unchanged (every existing dep was
    /// implicitly host-target).
    #[serde(default)]
    pub target: TargetSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A rule for custom artifact generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    pub name: String,
    /// Inputs are dispatched by string at runtime; no handles.
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub depends_on: Vec<String>,
    pub handler: RuleHandler,
    /// Working directory for rule execution. `None` defaults to the build
    /// directory. Set to `"project_root"` to run from the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// How a rule's artifact is generated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleHandler {
    /// A built-in Rust function identified by name.
    Builtin(String),
    /// Script source code for user-defined rule callbacks.
    Script(String),
}

/// A build pipeline definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineDef {
    pub name: String,
    pub stages: Vec<PipelineStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A single step in a build pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    pub name: String,
    /// Resolved group handles, populated by the intern pass. Dangling
    /// references are dropped (with a diagnostic) at intern time so
    /// downstream consumers never see an unresolved entry.
    pub inputs: Vec<Handle<GroupDef>>,
    /// Optional rule to run for this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

/// Bootloader protocol identifier. Common protocols (`uefi`,
/// `multiboot`, `multiboot2`) are typed; everything else is preserved
/// verbatim via [`BootloaderKind::Custom`] so projects with bespoke
/// bootloaders keep working without code changes here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BootloaderKind {
    Uefi,
    Multiboot,
    Multiboot2,
    /// Escape hatch — preserves the raw protocol identifier for
    /// projects with custom bootloaders.
    Custom(String),
}

impl Default for BootloaderKind {
    fn default() -> Self {
        Self::Custom(String::new())
    }
}

impl BootloaderKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Uefi => "uefi",
            Self::Multiboot => "multiboot",
            Self::Multiboot2 => "multiboot2",
            Self::Custom(s) => s.as_str(),
        }
    }

    /// Parse a kind string into the typed enum. Unknown strings map
    /// to `Custom(s)` so existing configs keep working without
    /// gluon-model code changes.
    ///
    /// We expose this as an inherent (infallible) `from_str` rather than
    /// `std::str::FromStr` because the conversion never fails — every
    /// input maps to *some* variant via the `Custom` escape hatch — and
    /// the trait would force callers through `Result<_, !>` for no
    /// benefit.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "uefi" => Self::Uefi,
            "multiboot" => Self::Multiboot,
            "multiboot2" => Self::Multiboot2,
            other => Self::Custom(other.to_string()),
        }
    }
}

/// Bootloader configuration. Describes the boot protocol and links the
/// bootloader entry crate so tools can reason about boot-time dependencies.
///
/// The `extras` map provides a forward-compatible escape hatch for
/// protocol-specific metadata that does not yet have a typed field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootloaderDef {
    /// Boot protocol identifier. Common values are typed via
    /// [`BootloaderKind`]; bespoke protocols round-trip via
    /// [`BootloaderKind::Custom`].
    pub kind: BootloaderKind,
    /// Optional protocol sub-variant or version (e.g. `"gop"` for UEFI
    /// with GOP-only graphics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Resolved bootloader entry crate. `None` when the user has not
    /// declared one (legal — some bootloaders are pure config files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_crate: Option<Handle<CrateDef>>,
    /// Path to a bootloader configuration file (e.g. `boot.cfg` for
    /// Limine). Resolved relative to the project root by consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
    /// Forward-compatible escape hatch for protocol-specific metadata
    /// that does not yet have a typed field above.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Filesystem flavour for a flat-FS image. The wire format is the
/// lowercase mnemonic so existing JSON model dumps round-trip
/// unchanged. Adding a new format is an additive enum-variant change
/// (no model break) — the dispatch sites in `gluon-core::image`
/// already match exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Fat12,
    Fat16,
    Fat32,
    Raw,
    Iso9660,
}

impl ImageFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fat12 => "fat12",
            Self::Fat16 => "fat16",
            Self::Fat32 => "fat32",
            Self::Raw => "raw",
            Self::Iso9660 => "iso9660",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "fat12" => Self::Fat12,
            "fat16" => Self::Fat16,
            "fat32" => Self::Fat32,
            "raw" => Self::Raw,
            "iso9660" => Self::Iso9660,
            _ => return None,
        })
    }
}

/// Disk image configuration. Describes how to assemble a bootable image
/// from build artifacts.
///
/// Two shapes:
///
/// - **Flat-FS image** (no `partition_table`, no `partitions`):
///   `entries` are placed directly into a single filesystem (e.g. a
///   FAT32 ESP for `qemu -drive file=,format=raw + OVMF`). This is the
///   Phase 2 surface and remains the default.
/// - **Partitioned hybrid disk** (`partition_table = Some(Mbr)`):
///   `raw_writes` place bytes at LBA offsets (boot sector code, stage
///   blobs), and each `partitions` entry describes a sub-region with
///   its own filesystem. `entries` must be empty in this mode — file
///   placement happens *inside* a [`PartitionDef`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageDef {
    pub name: String,
    /// Image filesystem flavour. Ignored when `partition_table` is set
    /// (the format is implicitly `Raw` — a partitioned disk container).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ImageFormat>,
    /// Optional image size in MiB. When `None`, the builder determines
    /// the minimum size from the entries (flat-FS) or from the largest
    /// LBA written by raw_writes/partitions (partitioned disk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mb: Option<u32>,
    /// Flat-FS entries. Mutually exclusive with `partitions` — if both
    /// are present the builder errors at validation time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ImageEntry>,
    /// Partition table to emit at sector 0. `None` means flat image
    /// (no MBR/GPT). Phase 3 implements MBR only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_table: Option<PartitionTable>,
    /// Raw byte placements at LBA offsets. Used for boot-sector code
    /// (LBA 0), stage blobs (low LBAs reserved before the first
    /// partition), and any other "this exact byte sequence at this
    /// exact sector" need. Order is preserved for diagnostics; the
    /// actual writes don't depend on it (each has its own LBA).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw_writes: Vec<RawWrite>,
    /// Partitioned regions of the disk, each with its own filesystem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<PartitionDef>,
    /// Forward-compatible escape hatch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A single entry in an [`ImageDef`]: something to copy into the image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    /// Where the data comes from.
    pub source: ImageSource,
    /// Destination path within the image filesystem.
    pub dest_path: String,
}

/// Source of data for an [`ImageEntry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSource {
    /// A compiled crate's primary build output.
    Crate(String),
    /// A file on the host filesystem (relative to project root).
    File(String),
    /// An assembled ESP directory (by name).
    Esp(String),
    /// A pinned download (see [`DownloadDef`]), by name.
    Download(String),
}

/// Partition-table flavour. Sector 0 layout differs by variant — MBR
/// uses the legacy DOS partition table at bytes 446..510 + the 0x55AA
/// signature; GPT writes a protective MBR plus a header at LBA 1.
///
/// Phase 3 implements MBR only. The variant is kept open so that GPT
/// can be added without churning the model wire format.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartitionTable {
    Mbr,
    Gpt,
}

/// A raw byte placement at a specific LBA (sector) on the disk.
///
/// This is the primitive that lets a `gluon.rs` script say "the BIOS
/// boot sector is the output of crate X, written at sector 0" or
/// "stage 1.5 lives at sector 66 and must fit in 958 sectors". Use
/// `partitions` instead for filesystem-shaped writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawWrite {
    /// Logical Block Address (sector index, 512 bytes per sector) of
    /// the first byte to write.
    pub lba: u64,
    /// Where the bytes come from.
    pub source: RawSource,
    /// Optional cap on the number of sectors the source may occupy.
    /// When set and exceeded, image building errors out — useful for
    /// hard size constraints (e.g. "stage 1.1 must fit in 64 sectors
    /// because stage 1.5 starts at LBA 66").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sectors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Source of bytes for a [`RawWrite`].
///
/// Tagged enum (rather than newtype-around-string) for the same reason
/// as `StampSource` in the image builder: a future `Computed` variant
/// (post-layout-callback bytes) will need additional fields, and
/// internally-tagged serde wants a struct payload so `kind` has a
/// place to land alongside the data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RawSource {
    /// A compiled crate's primary artifact (typically a `RawBinary`
    /// crate's `.bin` file). The crate must produce a single contiguous
    /// blob of bytes — ELF inputs are a configuration error and are
    /// caught at image-build time.
    Crate { name: String },
    /// A file on the host filesystem (relative to project root). Used
    /// for pre-built blobs the user wants to drop in unchanged
    /// (e.g. a Stage1Meta binary produced by an out-of-band build
    /// step).
    File { path: String },
    /// Inline bytes. Useful for tiny constant payloads — magic numbers,
    /// signature blocks, padding. Not intended for kilobyte-scale data
    /// (the model is serialised into the cache key, so big inline
    /// blobs balloon stamp files).
    Bytes { bytes: Vec<u8> },
}

/// A partitioned region of the disk, identified by an LBA range and a
/// filesystem type.
///
/// `start_lba` and `end_lba` are both inclusive — the partition occupies
/// sectors `[start_lba, end_lba]`. `end_lba == None` means "extend to
/// the last available sector of the image"; the builder fills it in
/// once `size_mb` is known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionDef {
    pub name: String,
    pub start_lba: u64,
    /// Inclusive end LBA. `None` means "fill to end of image" — the
    /// builder resolves this once the total image size is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_lba: Option<u64>,
    /// Filesystem to format inside the partition. `"fat32"` is the only
    /// value Phase 3 understands; `"raw"` is reserved for "partitioned
    /// region with no filesystem on top, populated via raw_writes
    /// scoped to its LBA range" — not yet implemented.
    pub fs_type: String,
    /// MBR partition type byte. Common values: `0x83` (Linux),
    /// `0xEF` (EFI System Partition), `0x0C` (FAT32 LBA).
    pub partition_type: u8,
    /// MBR active/bootable flag. Setting this on an ESP for legacy
    /// BIOS+UEFI hybrid disks is the typical case.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bootable: bool,
    /// Files placed inside the partition's filesystem. Same shape as
    /// `ImageDef::entries` for flat-FS images.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ImageEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for ImageDef {
    fn name(&self) -> &str {
        &self.name
    }
}

/// A runner: how a profile's run binary, and the integration tests
/// built for its target, are executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerDef {
    pub name: String,
    pub kind: RunnerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for RunnerDef {
    fn name(&self) -> &str {
        &self.name
    }
}

/// What a runner does with the binary it is given.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerKind {
    /// Boot it under QEMU: `cfg.runner(name).qemu(program)`.
    Qemu(Box<QemuDef>),
    /// Run a command: `cfg.runner(name).command([...])`.
    Command(CommandRunnerDef),
}

/// A command runner. Each `{binary}` in `argv` is replaced by the
/// binary's path; `gluon run -- <args>` appends the extra arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRunnerDef {
    pub argv: Vec<String>,
    /// The exit status that counts as success. `None` means 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_exit_code: Option<i32>,
}

/// QEMU configuration for `gluon run` and `gluon test`.
///
/// All fields are optional at the model level — defaults are filled in by
/// [`resolve_qemu`](../../gluon_core/run/resolve/fn.resolve_qemu.html) at
/// runtime. Profile-level overrides (`ProfileDef::qemu_memory`,
/// `qemu_cores`, `qemu_extra_args`, `test_timeout`) take precedence over
/// the values here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QemuDef {
    /// QEMU binary to invoke (e.g. `"qemu-system-x86_64"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Machine type (`-machine <...>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Memory in MiB (`-m <...>M`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    /// Core count (`-smp <...>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// Serial policy. `Stdio` is the default applied at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<SerialMode>,
    /// Display policy. `None` here means "user didn't say" — the runner
    /// keeps its implicit `-display none` default for headless / CI use.
    /// To get a window, set this to [`DisplayMode::Default`] (let QEMU
    /// pick its compiled-in default backend) or [`DisplayMode::Spec`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayMode>,
    /// Extra QEMU arguments appended after every gluon-managed flag.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Default boot mode for this profile. `None` means direct kernel boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_mode: Option<BootMode>,
    /// Explicit OVMF CODE firmware path. Overrides env/system fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovmf_code: Option<PathBuf>,
    /// Explicit OVMF VARS firmware path. Overrides env/system fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovmf_vars: Option<PathBuf>,
    /// EFI System Partition source. Mutually exclusive variants.
    /// Used by UEFI mode for the `EFI/BOOT/BOOT*.EFI` source. Ignored
    /// when `disk` is set (a hybrid disk supplies its own ESP via a
    /// FAT32 partition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub esp: Option<EspSource>,
    /// Boot disk attached to QEMU as the first IDE drive. Mode-agnostic:
    /// BIOS mode boots from it directly via SeaBIOS, and UEFI mode lets
    /// OVMF locate `EFI/BOOT/BOOTX64.EFI` inside the disk's FAT32
    /// partition. Mutually compatible with `esp` only when `esp` is the
    /// VVFAT directory variant — but the typical hybrid-disk shape
    /// leaves `esp` unset entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskSource>,
    /// `isa-debug-exit` I/O port (for the future test harness). Defaults
    /// to `0xf4` at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_exit_port: Option<u16>,
    /// Success exit code written by the kernel to `test_exit_port`.
    /// Defaults to `0x10` at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_success_code: Option<u8>,
    /// QEMU process exit status that `gluon run` reports as success.
    /// `None` means only status 0 succeeds. With `isa-debug-exit`, a
    /// kernel that writes `v` makes QEMU exit with `(v << 1) | 1`, so a
    /// kernel writing `0x10` needs `Some(33)`. When set, every other
    /// status, 0 included, is a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_exit_code: Option<i32>,
    /// Seconds before a test run is killed. Applied at resolve time when
    /// the runner spawns QEMU for `gluon test`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout: Option<u32>,
    /// Extra QEMU arguments appended only during `gluon test`, after
    /// both `extra_args` and any profile-level overrides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Boot method selected for `gluon run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootMode {
    /// Direct kernel boot via `-kernel <path>`. QEMU parses the ELF
    /// and jumps to its entry point. Works for any kernel QEMU's
    /// direct loader understands (multiboot, ELF64 with a plain
    /// entry point, etc.).
    Direct,
    /// UEFI boot via OVMF pflash firmware. An optional ESP source
    /// provides the bootable `EFI/BOOT/BOOTX64.EFI` (or equivalent).
    Uefi,
    /// Legacy BIOS boot via QEMU's default firmware (SeaBIOS for
    /// `pc`/`q35` machines). No `-kernel` and no pflash are emitted;
    /// the boot path comes entirely from the disk image attached as
    /// the first IDE drive. Pair with a hybrid disk (MBR boot code at
    /// LBA 0) or a flat `EspSource::Image` containing the BIOS
    /// loader. `EspSource::Dir` (VVFAT) is rejected — VVFAT skips
    /// the MBR/boot-sector area, so SeaBIOS sees no bootable device.
    Bios,
}

/// Serial output policy for QEMU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialMode {
    /// Forward QEMU's first serial port to the host stdio (default).
    Stdio,
    /// Disable serial (`-serial none`).
    None,
    /// Write serial output to a file (`-serial file:<path>`).
    File(PathBuf),
}

/// Display policy for QEMU.
///
/// Distinct from `Option::None` on the field that holds this: the field
/// being absent means "user didn't say, keep the implicit headless
/// default". This enum is for users who explicitly chose a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    /// Pass `-display none` — explicit headless. Same wire effect as the
    /// implicit default, but documents the user's intent.
    None,
    /// Omit `-display` entirely so QEMU uses its compiled-in default
    /// backend (cocoa on macOS, gtk/sdl on Linux, etc.). Use this when
    /// you want a window without picking the backend yourself.
    Default,
    /// Pass `-display <spec>` verbatim. Examples: `"gtk"`, `"cocoa"`,
    /// `"sdl"`, `"vnc=:0"`. Gluon doesn't validate the spec — that's
    /// QEMU's vocabulary.
    Spec(String),
}

/// Source for the EFI System Partition in UEFI boot mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EspSource {
    /// Local directory mounted via QEMU's VVFAT driver
    /// (`-drive format=raw,file=fat:rw:<dir>`).
    Dir(PathBuf),
    /// Pre-built raw disk image
    /// (`-drive format=raw,file=<img>`).
    Image(PathBuf),
}

/// Source for QEMU's boot disk. Mode-agnostic: BIOS mode reads it
/// directly via SeaBIOS, UEFI mode lets OVMF find a FAT32 ESP
/// partition inside it. Tagged-enum because future variants
/// (`Vvfat(PathBuf)`, `Cdrom(PathBuf)`) are likely.
///
/// `Image(PathBuf)` is for a *pre-built* disk image (the user
/// supplies the path). `ImageRef(name)` defers resolution to
/// runtime — at qemu-resolve time we look up `name` in the
/// scheduler's image artifact map (the `ImageDef` named `name`
/// must exist and have been built).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiskSource {
    /// Pre-built raw disk image at a fixed path.
    Image { path: PathBuf },
    /// Reference to an image declared via `image("<name>")`. Resolved
    /// to a path at qemu-spawn time so the disk is whichever image
    /// the scheduler produced for the active build.
    ImageRef { name: String },
}

/// EFI System Partition declaration. Describes a set of build artifacts
/// that should be copied into a FAT-layout directory for UEFI boot.
///
/// Unlike `EspSource::Dir`, which points at a user-maintained directory
/// on disk, an `EspDef` is *generated* by the build pipeline from named
/// sibling crates. The output directory lives under
/// `build/cross/<target>/<profile>/esp/<name>/` and is refreshed by the
/// `EspBuild` scheduler node whenever a source artifact changes.
///
/// The typical shape is a single entry for the bootloader:
/// ```rust,ignore
/// cfg.esp("default").add("bootloader", "EFI/BOOT/BOOTX64.EFI");
/// ```
///
/// `gluon run` auto-picks this ESP when a profile is `boot_mode("uefi")`
/// and the user has not set an explicit `qemu().esp_dir(...)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EspDef {
    pub name: String,
    /// Ordered list of `(source, dest_path_within_esp)` entries.
    #[serde(default)]
    pub entries: Vec<EspEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A single entry in an [`EspDef`]: a file copied to `dest_path`
/// within the ESP directory. Crate names are resolved to handles by
/// the intern pass after the model is fully built; entries whose
/// source crate does not exist are dropped (with a diagnostic) rather
/// than carried forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EspEntry {
    pub source: EspEntrySource,
    pub dest_path: String,
}

/// Where an [`EspEntry`]'s bytes come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EspEntrySource {
    /// A sibling crate's primary build output.
    Crate(Handle<CrateDef>),
    /// A host file, relative to the project root. Projects ship
    /// bootloader configuration files (such as `limine.conf`) this way.
    File(String),
    /// A pinned download (see [`DownloadDef`]).
    Download(Handle<DownloadDef>),
}

impl EspEntrySource {
    /// The source crate, for entries that copy a crate's output.
    pub fn crate_handle(&self) -> Option<Handle<CrateDef>> {
        match self {
            Self::Crate(h) => Some(*h),
            Self::File(_) | Self::Download(_) => None,
        }
    }
}

/// A custom build step: invoke a host binary (a "runner crate") with a
/// curated environment, then register its output as a named artifact.
///
/// Unlike [`RuleDef`] (Rhai-scripted or Rust-builtin), a build step's
/// logic lives in user-authored Rust code compiled by gluon as an
/// ordinary host `bin` crate. The build step is the *invocation* of that
/// runner with specific inputs/outputs/env. One runner can be reused
/// across many invocations (e.g. a generic ELF stripper invoked once per
/// kernel binary); one build step has exactly one runner.
///
/// ### Protocol
///
/// Inputs are passed via environment variables. Each `inputs` entry maps
/// an env var name to a [`BuildStepInput`] source, and the runner reads
/// it via `std::env::var()`. The output path is communicated via the
/// reserved env var `GLUON_OUTPUT_PATH` — the runner is responsible for
/// writing exactly one file to that path. The env vars in `env` are
/// literal pass-through values and are merged after `inputs` in the
/// child environment.
///
/// ### Determinism
///
/// `inputs` and `env` are [`BTreeMap`]s so iteration order is stable,
/// which keeps the cache-key hash identical across runs. A build step's
/// output filename is the only allowed structural variation that affects
/// downstream paths; the build dir layout is otherwise pinned via the
/// step's `name`.
///
/// ### Forward compatibility
///
/// `outputs` is intentionally a `BTreeMap<String, String>` with
/// (currently) exactly one entry. The single-output convention is
/// enforced by validation and surfaced to the runner via
/// `GLUON_OUTPUT_PATH`. When multi-output runners become a real need,
/// this field can be widened without a model break.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStepDef {
    pub name: String,
    /// Resolved runner crate (must be a host `bin`). Mandatory; intern
    /// drops the entire build step (with a diagnostic) if the named
    /// runner cannot be resolved, so consumers do not need to recheck
    /// resolution.
    pub runner: Handle<CrateDef>,
    /// Input env vars. Each entry maps an env-var name (e.g.
    /// `"KERNEL_PATH"`) to a [`BuildStepInput`] source describing what
    /// the value should be at invocation time.
    #[serde(default)]
    pub inputs: BTreeMap<String, BuildStepInput>,
    /// Literal env vars passed verbatim to the runner. Merged after
    /// `inputs` (literal entries cannot reference artifact paths).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Output filename (no path separators). Gluon places the output
    /// file under `build/build-steps/<name>/<output>` and sets
    /// `GLUON_OUTPUT_PATH` accordingly.
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Source of a single environment-variable input to a [`BuildStepDef`].
///
/// `Crate` and `File` participate in the cache key by content hash;
/// `Literal` participates as the literal string. The intern pass
/// resolves crate references to a [`Handle<CrateDef>`]; entries whose
/// crate cannot be resolved are dropped from the input map (mirroring
/// the `EspDef` filter-on-intern pattern) so downstream consumers
/// never see a dangling reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStepInput {
    /// Path to another crate's primary build artifact, resolved at run
    /// time via `model.crates.get(handle)`.
    Crate { handle: Handle<CrateDef> },
    /// Path to a file in the project tree, joined to `project_root` and
    /// canonicalised at invocation time.
    File { path: String },
    /// A literal string value, passed through verbatim.
    Literal { value: String },
}

impl Named for TargetDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for ProfileDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for GroupDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for CrateDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for RuleDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for PipelineDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for ExternalDepDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for EspDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for BuildStepDef {
    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esp_def_round_trips_through_json() {
        // Mint a real CrateDef handle so the entry's crate source
        // round-trips through serde.
        let mut m = BuildModel::default();
        let (group_h, _) = m.groups.insert("uefi".into(), GroupDef::default());
        let (crate_h, _) = m.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Host,
                deps: BTreeMap::new(),
                dev_deps: BTreeMap::new(),
                features: Vec::new(),
                root: None,
                linker_script: None,
                group: group_h,
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
        let (_, inserted) = m.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![
                    EspEntry {
                        source: EspEntrySource::Crate(crate_h),
                        dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                    },
                    EspEntry {
                        source: EspEntrySource::File("limine.conf".into()),
                        dest_path: "limine.conf".into(),
                    },
                ],
                span: None,
            },
        );
        assert!(inserted);

        let json = serde_json::to_string(&m).unwrap();
        let de: BuildModel = serde_json::from_str(&json).unwrap();
        let h = de.esps.lookup("default").expect("esp name index rebuilt");
        let esp = de.esps.get(h).unwrap();
        assert_eq!(esp.entries.len(), 2);
        assert_eq!(esp.entries[0].source, EspEntrySource::Crate(crate_h));
        assert_eq!(esp.entries[0].dest_path, "EFI/BOOT/BOOTX64.EFI");
        assert_eq!(
            esp.entries[1].source,
            EspEntrySource::File("limine.conf".into())
        );
    }

    #[test]
    fn panic_strategy_serde_lowercase() {
        let json = serde_json::to_string(&PanicStrategy::Abort).unwrap();
        assert_eq!(json, r#""abort""#);
        let de: PanicStrategy = serde_json::from_str(r#""unwind""#).unwrap();
        assert_eq!(de, PanicStrategy::Unwind);
    }

    #[test]
    fn crate_type_serde_matches_as_str() {
        for t in [
            CrateType::Lib,
            CrateType::Bin,
            CrateType::ProcMacro,
            CrateType::StaticLib,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let trimmed = json.trim_matches('"');
            assert_eq!(
                trimmed,
                t.as_str(),
                "serde form must match as_str for {t:?}"
            );
        }
    }

    #[test]
    fn target_spec_serde_externally_tagged() {
        let host_json = serde_json::to_string(&TargetSpec::Host).unwrap();
        assert_eq!(host_json, r#"{"kind":"host"}"#);
        let cross_json = serde_json::to_string(&TargetSpec::Cross(Handle::new(7))).unwrap();
        assert_eq!(cross_json, r#"{"kind":"cross","handle":7}"#);

        let de_host: TargetSpec = serde_json::from_str(r#"{"kind":"host"}"#).unwrap();
        assert_eq!(de_host, TargetSpec::Host);
        let de_cross: TargetSpec = serde_json::from_str(r#"{"kind":"cross","handle":42}"#).unwrap();
        assert_eq!(de_cross, TargetSpec::Cross(Handle::new(42)));
    }

    #[test]
    fn dep_resolution_serde_externally_tagged() {
        let project_json = serde_json::to_string(&DepResolution::Project(Handle::new(3))).unwrap();
        assert_eq!(project_json, r#"{"kind":"project","handle":3}"#);
        let external_json =
            serde_json::to_string(&DepResolution::External(Handle::new(11))).unwrap();
        assert_eq!(external_json, r#"{"kind":"external","handle":11}"#);

        let de_project: DepResolution =
            serde_json::from_str(r#"{"kind":"project","handle":1}"#).unwrap();
        assert_eq!(de_project, DepResolution::Project(Handle::new(1)));
        let de_external: DepResolution =
            serde_json::from_str(r#"{"kind":"external","handle":2}"#).unwrap();
        assert_eq!(de_external, DepResolution::External(Handle::new(2)));
    }
}
