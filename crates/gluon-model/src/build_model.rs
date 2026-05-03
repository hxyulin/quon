use crate::handle::{Arena, Handle, Named};
use crate::kconfig::{ConfigOptionDef, ConfigValue, PresetDef};
use crate::source::SourceSpan;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// EFI System Partition declarations. Each entry describes a set of
    /// compiled artifacts to be assembled into a FAT-layout directory
    /// under `build/cross/<target>/<profile>/esp/<name>/`. Consumed by
    /// the `EspBuild` scheduler node at build time and auto-wired into
    /// `gluon run --uefi` when there is exactly one declaration.
    #[serde(default)]
    pub esps: Arena<EspDef>,
    /// Keyed by option NAME (e.g. `"CONFIG_FOO"`); options are not in an arena
    /// because every reference to them is by string identifier.
    pub config_options: BTreeMap<String, ConfigOptionDef>,
    pub presets: BTreeMap<String, PresetDef>,
    /// Bootloader configuration (singleton per project).
    #[serde(default)]
    pub bootloader: BootloaderDef,
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
    /// Stub QEMU configuration; details land in a later sub-project.
    #[serde(default)]
    pub qemu: QemuDef,
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
    /// Bare-metal targets almost always want `Some("abort")`. Mixing panic
    /// strategies across sysroot rlibs and downstream crates fails at link
    /// time with `error: the crate ... is compiled with a different panic
    /// strategy`, so this must be consistent across all crates for a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_strategy: Option<String>,
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
    /// `--crate-type=bin`; gluon additionally injects
    /// `-C link-arg=--oformat=binary` so rust-lld writes a raw image
    /// instead of an ELF, and uses a `.bin` artifact extension so
    /// downstream consumers (`image()`, `${artifact:foo}`) get a stable
    /// path. Linker scripts are permitted (and usually required, since
    /// flat binaries have no PT_LOAD-driven layout to fall back on).
    #[serde(rename = "raw-binary")]
    RawBinary,
}

impl CrateType {
    /// String passed to rustc's `--crate-type=` flag. `RawBinary` maps
    /// to `bin` because rustc has no native concept of flat-binary
    /// output; the difference is the linker `--oformat=binary` flag,
    /// which is appended at command-assembly time, not communicated
    /// via crate-type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
            Self::ProcMacro => "proc-macro",
            Self::StaticLib => "staticlib",
            Self::RawBinary => "bin",
        }
    }

    /// True for crate types that produce an executable (ELF or flat).
    /// Used by the rustc command builder to decide whether linker
    /// scripts and binary-output conventions apply.
    pub fn is_executable(&self) -> bool {
        matches!(self, Self::Bin | Self::RawBinary)
    }
}

/// A build profile definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileDef {
    pub name: String,
    /// Resolved parent profile. `None` when the profile does not inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<Handle<ProfileDef>>,
    /// Resolved target. `None` when the profile inherits its target from
    /// a parent (resolution walks the chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Handle<TargetDef>>,
    pub opt_level: Option<u8>,
    pub debug_info: Option<bool>,
    pub lto: Option<String>,
    /// Resolved boot binary. `None` means "use the profile's preset/default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_binary: Option<Handle<CrateDef>>,
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
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "handle")]
pub enum TargetSpec {
    #[default]
    Host,
    Cross(Handle<TargetDef>),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
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
    /// String references to group names.
    pub inputs: Vec<String>,
    /// Sibling resolved handles, populated by the intern pass.
    #[serde(default)]
    pub inputs_handles: Vec<Option<Handle<GroupDef>>>,
    /// Optional rule to run for this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

/// Bootloader configuration. Describes the boot protocol and links the
/// bootloader entry crate so tools can reason about boot-time dependencies.
///
/// The `extras` map provides a forward-compatible escape hatch for
/// protocol-specific metadata that does not yet have a typed field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootloaderDef {
    /// Boot protocol identifier: `"uefi"`, `"multiboot"`, `"multiboot2"`,
    /// `"custom"`, etc.
    pub kind: String,
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
    /// Image format: `"fat12"`, `"fat16"`, `"fat32"`, `"raw"`, `"iso9660"`.
    /// Ignored when `partition_table` is set (the format is implicitly
    /// `"raw"` — a partitioned disk container).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
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

/// QEMU configuration for `gluon run` (and future `gluon test`).
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
    /// Ordered list of `(source_crate, dest_path_within_esp)` entries.
    #[serde(default)]
    pub entries: Vec<EspEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A single entry in an [`EspDef`]: a handle to a sibling crate whose
/// primary build output should be copied to `dest_path` within the ESP
/// directory. The handle is resolved by the intern pass after the model
/// is fully built; entries whose source crate does not exist are
/// dropped (with a diagnostic) rather than carried forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EspEntry {
    pub source_crate: Handle<CrateDef>,
    pub dest_path: String,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildStepDef {
    pub name: String,
    /// Name of the runner crate. Must resolve to a host `bin` crate in
    /// the model. Populated as a string at parse time and resolved to
    /// `runner_handle` during the intern pass.
    pub runner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_handle: Option<Handle<CrateDef>>,
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
/// `Literal` participates as the literal string. The `CrateHandle`
/// variant is populated during the intern pass alongside the unresolved
/// `Crate` form, mirroring how [`DepDef`] tracks both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStepInput {
    /// Path to another crate's primary build artifact. Resolved at run
    /// time from the [`Handle<CrateDef>`] populated during interning.
    Crate {
        crate_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crate_handle: Option<Handle<CrateDef>>,
    },
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
    fn default_build_model_is_empty() {
        let m = BuildModel::default();
        assert!(m.project.is_none());
        assert!(m.targets.is_empty());
        assert!(m.profiles.is_empty());
        assert!(m.groups.is_empty());
        assert!(m.crates.is_empty());
        assert!(m.rules.is_empty());
        assert!(m.pipelines.is_empty());
        assert!(m.external_deps.is_empty());
        assert!(m.config_options.is_empty());
        assert!(m.presets.is_empty());
        assert!(m.esps.is_empty());
    }

    #[test]
    fn esp_def_round_trips_through_json() {
        // Mint a real CrateDef handle so EspEntry.source_crate (now a
        // non-optional Handle<CrateDef>) round-trips through serde.
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
            },
        );
        let (_, inserted) = m.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source_crate: crate_h,
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                span: None,
            },
        );
        assert!(inserted);

        let json = serde_json::to_string(&m).unwrap();
        let de: BuildModel = serde_json::from_str(&json).unwrap();
        let h = de.esps.lookup("default").expect("esp name index rebuilt");
        let esp = de.esps.get(h).unwrap();
        assert_eq!(esp.entries.len(), 1);
        assert_eq!(esp.entries[0].source_crate, crate_h);
        assert_eq!(esp.entries[0].dest_path, "EFI/BOOT/BOOTX64.EFI");
    }

    #[test]
    fn crate_def_artifact_env_round_trips() {
        // Construct a real GroupDef arena to mint a handle, since
        // CrateDef.group is now a non-optional Handle<GroupDef>.
        let mut groups: Arena<GroupDef> = Arena::new();
        let (group, _) = groups.insert("host".into(), GroupDef::default());
        let mut c = CrateDef {
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
            group,
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        };
        c.artifact_env.insert("KERNEL_PATH".into(), "kernel".into());
        c.artifact_deps.push("kernel".into());

        let json = serde_json::to_string(&c).unwrap();
        let de: CrateDef = serde_json::from_str(&json).unwrap();
        assert_eq!(
            de.artifact_env.get("KERNEL_PATH").map(String::as_str),
            Some("kernel")
        );
        assert_eq!(de.artifact_deps, vec!["kernel".to_string()]);
    }

    #[test]
    fn build_model_json_round_trip() {
        let mut m = BuildModel {
            project: Some(ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, inserted) = m.targets.insert(
            "x86_64".into(),
            TargetDef {
                name: "x86_64".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        assert!(inserted);

        let json = serde_json::to_string(&m).unwrap();
        let de: BuildModel = serde_json::from_str(&json).unwrap();
        assert_eq!(de.project.as_ref().unwrap().name, "demo");
        assert_eq!(de.targets.len(), 1);
        let h = de.targets.lookup("x86_64").expect("name index rebuilt");
        assert_eq!(de.targets.get(h).unwrap().spec, "x86_64-unknown-none");
    }

    #[test]
    fn bootloader_def_round_trips_through_json() {
        // Mint a real CrateDef handle so the resolved entry_crate field
        // (now a typed handle) round-trips through serde unchanged.
        let mut groups: Arena<GroupDef> = Arena::new();
        let (group, _) = groups.insert("g".into(), GroupDef::default());
        let mut crates: Arena<CrateDef> = Arena::new();
        let (entry_handle, _) = crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2024".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Host,
                deps: BTreeMap::new(),
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

        let bl = BootloaderDef {
            kind: "uefi".into(),
            protocol: Some("gop".into()),
            entry_crate: Some(entry_handle),
            config_file: Some("boot.cfg".into()),
            extras: BTreeMap::new(),
            span: None,
        };
        let json = serde_json::to_string(&bl).unwrap();
        let de: BootloaderDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.kind, "uefi");
        assert_eq!(de.protocol.as_deref(), Some("gop"));
        assert_eq!(de.entry_crate, Some(entry_handle));
        assert_eq!(de.config_file.as_deref(), Some("boot.cfg"));
        assert!(de.extras.is_empty());
    }

    #[test]
    fn image_def_round_trips_through_json() {
        let mut m = BuildModel::default();
        let (_, inserted) = m.images.insert(
            "disk".into(),
            ImageDef {
                name: "disk".into(),
                format: Some("fat32".into()),
                size_mb: Some(64),
                entries: vec![ImageEntry {
                    source: ImageSource::Crate("bootloader".into()),
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                partition_table: None,
                raw_writes: Vec::new(),
                partitions: Vec::new(),
                extras: BTreeMap::new(),
                span: None,
            },
        );
        assert!(inserted);

        let json = serde_json::to_string(&m).unwrap();
        let de: BuildModel = serde_json::from_str(&json).unwrap();
        let h = de.images.lookup("disk").expect("image name index rebuilt");
        let img = de.images.get(h).unwrap();
        assert_eq!(img.format.as_deref(), Some("fat32"));
        assert_eq!(img.size_mb, Some(64));
        assert_eq!(img.entries.len(), 1);
        assert_eq!(img.entries[0].dest_path, "EFI/BOOT/BOOTX64.EFI");
        match &img.entries[0].source {
            ImageSource::Crate(name) => assert_eq!(name, "bootloader"),
            other => panic!("expected ImageSource::Crate, got {other:?}"),
        }
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
}
