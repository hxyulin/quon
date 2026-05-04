//! Centralised path arithmetic for the gluon build tree.
//!
//! `BuildLayout` owns the mapping from logical build artefacts (a crate's
//! incremental directory, a target's sysroot, the cache manifest) to the
//! concrete `PathBuf` they live at on disk. It performs **no I/O**: every
//! method is a pure function of the layout's root plus its arguments.
//!
//! Routing every `build/...` path through this type keeps the directory
//! layout in one place, so later reorganisation (e.g. sharding by target,
//! adding a content-addressed store) only has to touch one file.

use super::driver::DriverKind;
use gluon_model::{CrateDef, ResolvedProfile, TargetDef};
use std::path::{Path, PathBuf};

/// Pure path-arithmetic view of the gluon build directory.
///
/// All getters return computed paths; none of them touch the filesystem.
/// Callers are responsible for creating directories on demand (typically
/// the compile step just before it writes into a given path).
///
/// # Per-driver namespacing
///
/// User-crate output directories are partitioned by the active
/// [`DriverKind`] so that `gluon check` and `gluon clippy` cannot
/// clobber `gluon build` artifacts (and vice versa). Concretely:
///
/// - `DriverKind::Rustc` (the default for `gluon build`) keeps the
///   historical paths: `<root>/host/<crate>/`,
///   `<root>/cross/<target>/<profile>/<crate>/`, etc.
/// - `DriverKind::Check` writes under `<root>/tool/check/...`.
/// - `DriverKind::Clippy` writes under `<root>/tool/clippy/...`.
///
/// **Sysroot, generated config crate, and the cache manifest are
/// deliberately *not* partitioned.** The sysroot is built with regular
/// rustc regardless of the user-facing driver, so re-running it for
/// every check/clippy invocation would just waste time. The config
/// crate is similarly driver-agnostic. The cache manifest can safely
/// be shared because [`super::rustc::RustcCommandBuilder::hash`]
/// already incorporates the program path and `--emit` kinds, so
/// driver-specific entries cannot collide on key.
#[derive(Debug, Clone)]
pub struct BuildLayout {
    /// Absolute or relative root under which every derived path lives.
    /// Usually `<project>/build`, but this type treats it opaquely.
    root: PathBuf,
    /// Project name used to derive the generated config crate directory.
    project_name: String,
    /// Active driver kind. Determines whether user-crate output paths
    /// get a `tool/<kind>/` prefix or use the historical `gluon build`
    /// layout. Defaults to [`DriverKind::Rustc`].
    driver: DriverKind,
}

impl BuildLayout {
    /// Construct a new layout rooted at `root` for the default
    /// `gluon build` driver. Existing call sites continue to work
    /// unchanged and observe the historical path layout.
    pub fn new(root: impl Into<PathBuf>, project_name: impl Into<String>) -> Self {
        Self::with_driver(root, project_name, DriverKind::Rustc)
    }

    /// Construct a new layout for an explicit driver. Used by
    /// `gluon check` and `gluon clippy` so their output goes under
    /// `tool/check/` / `tool/clippy/` instead of clobbering build
    /// artifacts.
    pub fn with_driver(
        root: impl Into<PathBuf>,
        project_name: impl Into<String>,
        driver: DriverKind,
    ) -> Self {
        Self {
            root: root.into(),
            project_name: project_name.into(),
            driver,
        }
    }

    /// Return the active driver. Used by `compile_crate` to pick the
    /// program binary and emit overrides; not normally relevant to
    /// path computation by external callers.
    pub fn driver(&self) -> DriverKind {
        self.driver
    }

    /// The cache-key suffix for the active driver. Cache keys must be
    /// disambiguated across `rustc` / `check` / `clippy` because each
    /// driver produces a distinct on-disk artifact set; without the
    /// suffix `gluon build` and `gluon check` would race over a single
    /// `BuildRecord` slot.
    ///
    /// Returns `""` for [`DriverKind::Rustc`], `":check"` for
    /// [`DriverKind::Check`], `":clippy"` for [`DriverKind::Clippy`].
    pub fn cache_key_suffix(&self) -> &'static str {
        match self.driver {
            DriverKind::Rustc => "",
            DriverKind::Check => ":check",
            DriverKind::Clippy => ":clippy",
        }
    }

    /// The build root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The base directory under which **user-crate** outputs live for
    /// the active driver. Returns the build root for `Rustc` and
    /// `<root>/tool/<kind>` for `Check`/`Clippy`. Sysroot and config
    /// crate paths intentionally do not route through this helper —
    /// they always use the bare root.
    fn user_crate_root(&self) -> PathBuf {
        match self.driver {
            DriverKind::Rustc => self.root.clone(),
            DriverKind::Check => self.root.join("tool").join("check"),
            DriverKind::Clippy => self.root.join("tool").join("clippy"),
        }
    }

    /// Path to the top-level cache manifest (`<root>/cache-manifest.json`).
    pub fn cache_manifest(&self) -> PathBuf {
        self.root.join("cache-manifest.json")
    }

    /// Per-target sysroot directory (`<root>/sysroot/<target>/`).
    ///
    /// Keyed by [`TargetDef::name`] (the gluon target name), not the
    /// underlying spec/triple — two targets with the same triple but
    /// different specs must not share a sysroot directory.
    pub fn sysroot_dir(&self, target: &TargetDef) -> PathBuf {
        self.root.join("sysroot").join(&target.name)
    }

    /// The directory rustc expects sysroot libs in
    /// (`<sysroot>/lib/rustlib/<target>/lib`).
    pub fn sysroot_lib_dir(&self, target: &TargetDef) -> PathBuf {
        self.sysroot_dir(target)
            .join("lib")
            .join("rustlib")
            .join(&target.name)
            .join("lib")
    }

    /// Per-host-crate artifact directory.
    ///
    /// `gluon build`: `<root>/host/<crate>/`.
    /// `gluon check`: `<root>/tool/check/host/<crate>/`.
    /// `gluon clippy`: `<root>/tool/clippy/host/<crate>/`.
    pub fn host_artifact_dir(&self, krate: &CrateDef) -> PathBuf {
        self.user_crate_root().join("host").join(&krate.name)
    }

    /// Per-cross-crate artifact directory.
    ///
    /// `gluon build`: `<root>/cross/<target>/<profile>/<crate>/`.
    /// `gluon check`/`clippy`: same path under `<root>/tool/<kind>/`.
    pub fn cross_artifact_dir(
        &self,
        target: &TargetDef,
        profile: &ResolvedProfile,
        krate: &CrateDef,
    ) -> PathBuf {
        self.user_crate_root()
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
            .join(&krate.name)
    }

    /// Final link/image output directory for a (target, profile) pair.
    ///
    /// `gluon build`: `<root>/cross/<target>/<profile>/final/`.
    /// `gluon check`/`clippy`: same path under `<root>/tool/<kind>/`.
    pub fn cross_final_dir(&self, target: &TargetDef, profile: &ResolvedProfile) -> PathBuf {
        self.user_crate_root()
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
            .join("final")
    }

    /// ESP assembly output directory for a named `EspDef` under the
    /// profile's cross target.
    ///
    /// `gluon build`: `<root>/cross/<target>/<profile>/esp/<name>/`.
    ///
    /// The ESP directory is populated by the `EspBuild` scheduler node
    /// from the primary outputs of the named source crates. Keying by
    /// the profile's cross target (not the source crates' individual
    /// targets — they may differ, as in the bootloader-with-embedded-kernel
    /// case) keeps the output path stable regardless of which crate
    /// targets the user wires into the ESP entries.
    pub fn esp_dir(
        &self,
        target: &TargetDef,
        profile: &ResolvedProfile,
        esp_name: &str,
    ) -> PathBuf {
        self.user_crate_root()
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
            .join("esp")
            .join(esp_name)
    }

    /// Path of an image artifact (`<root>/images/<name>.img`).
    ///
    /// Image outputs are profile-agnostic: a single `image()` block in
    /// `gluon.rs` produces one canonical artifact, regardless of which
    /// profile triggered the build. This reflects how images are
    /// consumed downstream — `qemu().disk(image_ref("..."))` references
    /// an image by name, not by (profile, name) pair, and rebuilding
    /// the same image into different paths per profile would just waste
    /// disk and confuse the cache.
    pub fn image_path(&self, image_name: &str) -> PathBuf {
        self.user_crate_root()
            .join("images")
            .join(format!("{image_name}.img"))
    }

    /// Per-build-step output directory
    /// (`<root>/build-steps/<name>/`).
    ///
    /// Build steps are profile-agnostic for the same reason images are:
    /// a single `build_step("name")` block produces one canonical
    /// output that downstream consumers reference by name. The runner
    /// is spawned with `GLUON_OUTPUT_PATH` pointing at
    /// `build_step_dir(name).join(<output_filename>)`.
    pub fn build_step_dir(&self, step_name: &str) -> PathBuf {
        self.user_crate_root().join("build-steps").join(step_name)
    }

    /// Directory containing the generated config crate
    /// (`<root>/generated/<project>_config/`).
    ///
    /// The `_config` suffix is the MVP-M default. Overriding the name via
    /// `ProjectDef::config_crate_name` is out of scope for this chunk.
    pub fn generated_config_crate_dir(&self) -> PathBuf {
        self.root
            .join("generated")
            .join(format!("{}_config", self.project_name))
    }

    /// Per-crate incremental compilation directory.
    ///
    /// `gluon build`: `<root>/incremental/<crate>/`.
    /// `gluon check`/`clippy`: under `<root>/tool/<kind>/incremental/<crate>/`
    /// — separate so the metadata-only and full-build incremental
    /// state don't trample each other.
    pub fn incremental_dir(&self, krate: &CrateDef) -> PathBuf {
        self.user_crate_root().join("incremental").join(&krate.name)
    }

    /// Cache file for the probed [`super::RustcInfo`]
    /// (`<root>/.rustc-info.json`).
    pub fn rustc_info_cache(&self) -> PathBuf {
        self.root.join(".rustc-info.json")
    }

    /// Per-crate output directory for a host-pinned vendored crate
    /// (`<root>/host/vendor-deps/<crate>/`).
    ///
    /// Vendored crates are kept in a sibling tree (`vendor-deps/`)
    /// rather than mixed into the user crate dir so a stray cargo run
    /// or a user-declared crate named the same as a vendored one can't
    /// trample its artifacts. `<crate>` is the cargo-normalised name
    /// (hyphens → underscores) so the path matches what rustc emits
    /// from `--crate-name`.
    pub fn host_vendor_dep_dir(&self, cargo_target_name: &str) -> PathBuf {
        self.user_crate_root()
            .join("host")
            .join("vendor-deps")
            .join(cargo_target_name)
    }

    /// Per-crate output directory for a cross-target vendored crate
    /// (`<root>/cross/<target>/<profile>/vendor-deps/<crate>/`).
    ///
    /// The `vendor-deps/` separation parallels [`Self::host_vendor_dep_dir`]
    /// for cross builds. Profile-keyed because rustc invocations differ
    /// per profile (opt level, debug info), even though cargo's
    /// resolution itself is profile-agnostic — that means the same
    /// vendored crate may be compiled twice, once per profile, when
    /// consumers across profiles use it.
    pub fn cross_vendor_dep_dir(
        &self,
        target: &TargetDef,
        profile: &ResolvedProfile,
        cargo_target_name: &str,
    ) -> PathBuf {
        self.user_crate_root()
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
            .join("vendor-deps")
            .join(cargo_target_name)
    }

    // -------- vendor paths (sub-project #3) --------

    /// Path to the populated vendor directory (`<project>/vendor/`).
    ///
    /// Populated by `gluon vendor` via `cargo vendor`, then read by the
    /// compile path through [`super::super::vendor::auto_register_vendored_deps`].
    /// Lives at the *project* root (not the build root) so it is
    /// visible to users and can be committed / gitignored per project
    /// policy. Gitignored by default.
    pub fn vendor_dir(&self, project_root: &Path) -> PathBuf {
        project_root.join("vendor")
    }

    /// Scratch Cargo workspace for the **host** target's vendored deps
    /// (`<root>/vendor-workspaces/host/`).
    ///
    /// Contains a generated `Cargo.toml` + stub `lib.rs` synthesised
    /// from the host-targeted subset of `BuildModel::external_deps`,
    /// plus the `Cargo.lock` that cargo writes during `cargo vendor`.
    /// The `Cargo.lock` inside this dir is the authoritative resolution
    /// pin; it is intended to be committed via a `.gitignore` carveout
    /// so repeat vendor runs are deterministic across machines.
    ///
    /// Cross-target deps (declared via `cfg.dependency(...).target(t)`)
    /// get a sibling workspace at
    /// [`Self::vendor_workspace_for_target`].
    pub fn vendor_workspace_dir(&self) -> PathBuf {
        self.vendor_workspace_for_target("host")
    }

    /// Scratch Cargo workspace for a specific target's vendored deps
    /// (`<root>/vendor-workspaces/<target>/`).
    ///
    /// `target_name` is the unqualified name of a `TargetDef` from the
    /// model (or the literal `"host"` for the host pipeline). One
    /// workspace per target keeps the per-target dep set, `Cargo.lock`,
    /// and any future cross-target divergence (e.g. per-target features)
    /// physically isolated.
    pub fn vendor_workspace_for_target(&self, target_name: &str) -> PathBuf {
        self.root.join("vendor-workspaces").join(target_name)
    }

    /// Path to the project-root `gluon.lock` file.
    ///
    /// `gluon.lock` is a thin TOML peer of `Cargo.lock`: it pins the
    /// vendored set and carries a fingerprint over the declared
    /// `external_deps` so `gluon build` can detect staleness without
    /// re-running `cargo vendor`.
    pub fn gluon_lock(&self, project_root: &Path) -> PathBuf {
        project_root.join("gluon.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::Handle;

    fn layout() -> BuildLayout {
        BuildLayout::new("/tmp/fake", "demo")
    }

    fn target() -> TargetDef {
        TargetDef {
            name: "x86_64-test".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        }
    }

    fn profile() -> ResolvedProfile {
        ResolvedProfile {
            name: "debug".into(),
            target: Handle::new(0),
            opt_level: 0,
            debug_info: true,
            lto: None,
            boot_binary: None,
            qemu_memory: None,
            qemu_cores: None,
            qemu_extra_args: Vec::new(),
            qemu_boot_mode: None,
            test_timeout: None,
        }
    }

    fn krate() -> CrateDef {
        use gluon_model::{CrateType, TargetSpec};
        use std::collections::BTreeMap;
        CrateDef {
            name: "kernel".into(),
            path: String::new(),
            edition: "2024".into(),
            crate_type: CrateType::Lib,
            target: TargetSpec::Host,
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            features: Vec::new(),
            root: None,
            linker_script: None,
            // The `layout` tests don't actually use `crate_def.group`,
            // so a placeholder dangling handle is fine.
            group: Handle::new(0),
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        }
    }

    #[test]
    fn root_returned_verbatim() {
        assert_eq!(layout().root(), Path::new("/tmp/fake"));
    }

    #[test]
    fn cache_key_suffix_returns_empty_for_rustc_driver() {
        let l = BuildLayout::with_driver("/tmp/fake", "demo", DriverKind::Rustc);
        assert_eq!(l.cache_key_suffix(), "");
    }

    #[test]
    fn cache_key_suffix_returns_check_for_check_driver() {
        let l = BuildLayout::with_driver("/tmp/fake", "demo", DriverKind::Check);
        assert_eq!(l.cache_key_suffix(), ":check");
    }

    #[test]
    fn cache_key_suffix_returns_clippy_for_clippy_driver() {
        let l = BuildLayout::with_driver("/tmp/fake", "demo", DriverKind::Clippy);
        assert_eq!(l.cache_key_suffix(), ":clippy");
    }

    #[test]
    fn cache_manifest_path() {
        assert_eq!(
            layout().cache_manifest(),
            PathBuf::from("/tmp/fake/cache-manifest.json")
        );
    }

    #[test]
    fn sysroot_paths() {
        let l = layout();
        let t = target();
        assert_eq!(
            l.sysroot_dir(&t),
            PathBuf::from("/tmp/fake/sysroot/x86_64-test")
        );
        assert_eq!(
            l.sysroot_lib_dir(&t),
            PathBuf::from("/tmp/fake/sysroot/x86_64-test/lib/rustlib/x86_64-test/lib")
        );
    }

    #[test]
    fn host_artifact_path() {
        assert_eq!(
            layout().host_artifact_dir(&krate()),
            PathBuf::from("/tmp/fake/host/kernel")
        );
    }

    #[test]
    fn cross_artifact_path() {
        assert_eq!(
            layout().cross_artifact_dir(&target(), &profile(), &krate()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug/kernel")
        );
    }

    #[test]
    fn cross_final_path() {
        assert_eq!(
            layout().cross_final_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug/final")
        );
    }

    #[test]
    fn generated_config_crate_path() {
        assert_eq!(
            layout().generated_config_crate_dir(),
            PathBuf::from("/tmp/fake/generated/demo_config")
        );
    }

    #[test]
    fn incremental_path() {
        assert_eq!(
            layout().incremental_dir(&krate()),
            PathBuf::from("/tmp/fake/incremental/kernel")
        );
    }

    #[test]
    fn rustc_info_cache_path() {
        assert_eq!(
            layout().rustc_info_cache(),
            PathBuf::from("/tmp/fake/.rustc-info.json")
        );
    }

    // ---------------------------------------------------------------
    // Vendor paths (sub-project #3)
    // ---------------------------------------------------------------

    #[test]
    fn vendor_dir_is_project_relative_not_build_relative() {
        // vendor/ lives at the project root, not under build/. This is
        // deliberate: users see and commit (or gitignore) vendor/ at
        // the top of the tree, and the build path reads from there.
        let l = layout();
        let project = Path::new("/tmp/project");
        assert_eq!(l.vendor_dir(project), PathBuf::from("/tmp/project/vendor"));
    }

    #[test]
    fn vendor_workspace_dir_is_under_build_root() {
        // The scratch Cargo workspace is gluon-managed and lives under
        // build/vendor-workspaces/<target>/. The host-targeted set lives
        // at host/; cross targets get sibling subdirectories.
        assert_eq!(
            layout().vendor_workspace_dir(),
            PathBuf::from("/tmp/fake/vendor-workspaces/host")
        );
    }

    #[test]
    fn vendor_workspace_for_target_uses_target_name() {
        assert_eq!(
            layout().vendor_workspace_for_target("x86_64-kernel"),
            PathBuf::from("/tmp/fake/vendor-workspaces/x86_64-kernel")
        );
    }

    #[test]
    fn host_vendor_dep_dir_uses_cargo_normalised_name() {
        // proc-macro2 → proc_macro2 in the path so the dir matches what
        // rustc emits from --crate-name.
        assert_eq!(
            layout().host_vendor_dep_dir("proc_macro2"),
            PathBuf::from("/tmp/fake/host/vendor-deps/proc_macro2")
        );
    }

    #[test]
    fn cross_vendor_dep_dir_keys_on_target_profile_and_crate() {
        assert_eq!(
            layout().cross_vendor_dep_dir(&target(), &profile(), "bitflags"),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug/vendor-deps/bitflags")
        );
    }

    #[test]
    fn vendor_workspace_dir_shared_across_drivers() {
        // Vendoring is a build-tree-wide concern; check/clippy must
        // reuse the same scratch workspace that `gluon build` would.
        let rustc = layout();
        let check = check_layout();
        assert_eq!(rustc.vendor_workspace_dir(), check.vendor_workspace_dir());
    }

    #[test]
    fn gluon_lock_path_is_at_project_root() {
        let project = Path::new("/tmp/project");
        assert_eq!(
            layout().gluon_lock(project),
            PathBuf::from("/tmp/project/gluon.lock")
        );
    }

    // ---------------------------------------------------------------
    // T3: per-driver namespacing
    // ---------------------------------------------------------------

    fn check_layout() -> BuildLayout {
        BuildLayout::with_driver("/tmp/fake", "demo", DriverKind::Check)
    }

    fn clippy_layout() -> BuildLayout {
        BuildLayout::with_driver("/tmp/fake", "demo", DriverKind::Clippy)
    }

    #[test]
    fn check_driver_namespaces_user_crate_dirs_under_tool_check() {
        let l = check_layout();
        assert_eq!(
            l.host_artifact_dir(&krate()),
            PathBuf::from("/tmp/fake/tool/check/host/kernel")
        );
        assert_eq!(
            l.cross_artifact_dir(&target(), &profile(), &krate()),
            PathBuf::from("/tmp/fake/tool/check/cross/x86_64-test/debug/kernel")
        );
        assert_eq!(
            l.cross_final_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/tool/check/cross/x86_64-test/debug/final")
        );
        assert_eq!(
            l.incremental_dir(&krate()),
            PathBuf::from("/tmp/fake/tool/check/incremental/kernel")
        );
    }

    #[test]
    fn clippy_driver_namespaces_user_crate_dirs_under_tool_clippy() {
        let l = clippy_layout();
        assert_eq!(
            l.host_artifact_dir(&krate()),
            PathBuf::from("/tmp/fake/tool/clippy/host/kernel")
        );
    }

    #[test]
    fn rustc_driver_keeps_historical_paths() {
        // Default `BuildLayout::new` must produce the same paths as
        // before T3 — anything else would silently break gluon build.
        let l = layout();
        assert_eq!(
            l.host_artifact_dir(&krate()),
            PathBuf::from("/tmp/fake/host/kernel"),
            "Rustc driver must keep historical layout"
        );
        assert_eq!(
            l.cross_artifact_dir(&target(), &profile(), &krate()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug/kernel")
        );
    }

    #[test]
    fn sysroot_paths_are_shared_across_drivers() {
        // Sysroot must NOT vary by driver — check/clippy reuse the
        // sysroot built by gluon build (and vice versa). This is the
        // whole reason we don't put a per-driver prefix on
        // `sysroot_dir`.
        let rustc = layout();
        let check = check_layout();
        let clippy = clippy_layout();
        let t = target();
        assert_eq!(rustc.sysroot_dir(&t), check.sysroot_dir(&t));
        assert_eq!(rustc.sysroot_dir(&t), clippy.sysroot_dir(&t));
    }

    #[test]
    fn cache_manifest_and_config_crate_paths_are_shared_across_drivers() {
        // Same rationale as sysroot: cache hashes already discriminate
        // between driver-flavored entries via program path + emit
        // kinds, so the manifest can safely be shared. The generated
        // config crate is identical for build/check/clippy.
        let rustc = layout();
        let check = check_layout();
        assert_eq!(rustc.cache_manifest(), check.cache_manifest());
        assert_eq!(
            rustc.generated_config_crate_dir(),
            check.generated_config_crate_dir()
        );
    }

    #[test]
    fn check_and_build_user_crate_dirs_do_not_collide() {
        // The whole point of T3: running gluon check then gluon build
        // (or vice versa) must not let one's `.rmeta` clobber the
        // other's `.rlib` in the same directory.
        let rustc = layout();
        let check = check_layout();
        let k = krate();
        assert_ne!(rustc.host_artifact_dir(&k), check.host_artifact_dir(&k));
    }

    #[test]
    fn targets_with_same_triple_different_names_do_not_collide() {
        let l = layout();
        let t1 = TargetDef {
            name: "x86_64-a".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        };
        let t2 = TargetDef {
            name: "x86_64-b".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        };
        assert_ne!(l.sysroot_dir(&t1), l.sysroot_dir(&t2));
    }
}
