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
use gluon_model::{ResolvedProfile, TargetDef};
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
/// - `DriverKind::Rustc` (the default for `gluon build`) uses the
///   build root: `<root>/cross/<target>/<profile>/deps/`, etc.
/// - `DriverKind::Check` writes under `<root>/tool/check/...`.
/// - `DriverKind::Clippy` writes under `<root>/tool/clippy/...`.
///
/// **Sysroot, generated config crate, fixed-host-profile units,
/// vendored crates and the fingerprint records are deliberately *not*
/// partitioned.** They are built with plain rustc regardless of the
/// user-facing driver, so re-running them for every check/clippy
/// invocation would just waste time. The records can be shared
/// because the keys of driver-specific units carry the driver
/// ([`Self::cache_key_suffix`]).
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

    /// Directory of the fingerprint records (`<root>/.fingerprint/`).
    /// Shared by every driver: keys carry the driver where the outputs
    /// differ, and fixed-host-profile units have one record for all.
    pub fn fingerprint_dir(&self) -> PathBuf {
        self.root.join(".fingerprint")
    }

    /// The single-file cache of older Gluon versions
    /// (`<root>/cache-manifest.json`). Only read to remove it; see
    /// [`crate::cache::remove_legacy_cache`].
    pub fn legacy_cache_manifest(&self) -> PathBuf {
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

    /// Directory of the fixed-host-profile units (proc macros, their
    /// deps, vendored host crates): `<root>/host/`. Holds `deps/` and
    /// `incremental/`. Not partitioned by driver: these units are
    /// compiled with plain rustc under every driver (see
    /// `UnitSettings::always_rustc`).
    pub fn host_dir(&self) -> PathBuf {
        self.root.join("host")
    }

    /// Output directory of the `gluon clippy` lint pass over project
    /// crates that every driver compiles with plain rustc:
    /// `<driver root>/host/lint/`. See
    /// `command_builder::needs_lint_pass`.
    pub fn lint_dir(&self) -> PathBuf {
        self.user_crate_root().join("host").join("lint")
    }

    /// Directory of the host units that use the active profile (host
    /// tools, build-step runners): `<driver root>/host/profile/<profile>/`.
    /// Each profile keeps its own copy, so switching back and forth
    /// does not rebuild them. The `profile/` level keeps a profile
    /// named `deps` from colliding with [`Self::host_dir`]'s children.
    pub fn host_profile_dir(&self, profile: &ResolvedProfile) -> PathBuf {
        self.user_crate_root()
            .join("host")
            .join("profile")
            .join(&profile.name)
    }

    /// Directory of the declared cross units for a (target, profile)
    /// pair: `<driver root>/cross/<target>/<profile>/`. Holds `deps/`,
    /// `final/`, `esp/` and `incremental/`.
    pub fn cross_dir(&self, target: &TargetDef, profile: &ResolvedProfile) -> PathBuf {
        self.user_crate_root()
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
    }

    /// Like [`Self::cross_dir`] but always under the build root, for
    /// cross units every driver compiles the same way: vendored crates
    /// and the generated config crate.
    pub fn shared_cross_dir(&self, target: &TargetDef, profile: &ResolvedProfile) -> PathBuf {
        self.root
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
    }

    /// Final link/image output directory for a (target, profile) pair.
    ///
    /// `gluon build`: `<root>/cross/<target>/<profile>/final/`.
    /// `gluon check`/`clippy`: same path under `<root>/tool/<kind>/`.
    pub fn cross_final_dir(&self, target: &TargetDef, profile: &ResolvedProfile) -> PathBuf {
        self.cross_dir(target, profile).join("final")
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

    /// Cache of pinned downloads: `<root>/downloads/`. Shared by every
    /// driver and profile, since each file is keyed by its SHA-256.
    /// See `crate::download`.
    pub fn downloads_dir(&self) -> PathBuf {
        self.root.join("downloads")
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

    /// Cache file for the probed [`super::RustcInfo`]
    /// (`<root>/.rustc-info.json`).
    pub fn rustc_info_cache(&self) -> PathBuf {
        self.root.join(".rustc-info.json")
    }

    /// Cache file for probed target cfg sets
    /// (`<root>/target-cfg.json`); see `manifest::platform`.
    pub fn target_cfg_cache(&self) -> PathBuf {
        self.root.join("target-cfg.json")
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

    #[test]
    fn cache_key_suffix_per_driver() {
        let cases = [
            (DriverKind::Rustc, ""),
            (DriverKind::Check, ":check"),
            (DriverKind::Clippy, ":clippy"),
        ];
        for (driver, want) in cases {
            let l = BuildLayout::with_driver("/tmp/fake", "demo", driver);
            assert_eq!(l.cache_key_suffix(), want, "driver {driver:?}");
        }
    }

    #[test]
    fn fixed_paths() {
        let l = layout();
        let cases = [
            (
                "fingerprint_dir",
                l.fingerprint_dir(),
                "/tmp/fake/.fingerprint",
            ),
            (
                "cross_final_dir",
                l.cross_final_dir(&target(), &profile()),
                "/tmp/fake/cross/x86_64-test/debug/final",
            ),
            (
                "generated_config_crate_dir",
                l.generated_config_crate_dir(),
                "/tmp/fake/generated/demo_config",
            ),
            (
                "rustc_info_cache",
                l.rustc_info_cache(),
                "/tmp/fake/.rustc-info.json",
            ),
        ];
        for (name, got, want) in cases {
            assert_eq!(got, PathBuf::from(want), "{name}");
        }
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
    fn unit_platform_dirs() {
        let l = layout();
        assert_eq!(l.host_dir(), PathBuf::from("/tmp/fake/host"));
        assert_eq!(
            l.host_profile_dir(&profile()),
            PathBuf::from("/tmp/fake/host/profile/debug")
        );
        assert_eq!(
            l.cross_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug")
        );
        assert_eq!(
            l.shared_cross_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug")
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
            l.host_profile_dir(&profile()),
            PathBuf::from("/tmp/fake/tool/check/host/profile/debug")
        );
        assert_eq!(
            l.cross_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/tool/check/cross/x86_64-test/debug")
        );
        assert_eq!(
            l.cross_final_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/tool/check/cross/x86_64-test/debug/final")
        );
    }

    #[test]
    fn clippy_driver_namespaces_user_crate_dirs_under_tool_clippy() {
        let l = clippy_layout();
        assert_eq!(
            l.cross_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/tool/clippy/cross/x86_64-test/debug")
        );
    }

    #[test]
    fn shared_dirs_ignore_the_driver() {
        // Units every driver compiles the same way live under the
        // build root, so `gluon check` reuses what `gluon build` made.
        let rustc = layout();
        let check = check_layout();
        assert_eq!(rustc.host_dir(), check.host_dir());
        assert_eq!(
            rustc.shared_cross_dir(&target(), &profile()),
            check.shared_cross_dir(&target(), &profile())
        );
        assert_ne!(
            rustc.cross_dir(&target(), &profile()),
            check.cross_dir(&target(), &profile())
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
    fn fingerprint_and_config_crate_paths_are_shared_across_drivers() {
        // Keys of driver-specific units carry the driver, so the
        // records can share one directory. The generated config crate
        // is identical for build/check/clippy.
        let rustc = layout();
        let check = check_layout();
        assert_eq!(rustc.fingerprint_dir(), check.fingerprint_dir());
        assert_eq!(
            rustc.generated_config_crate_dir(),
            check.generated_config_crate_dir()
        );
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
