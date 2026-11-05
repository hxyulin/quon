//! Rustc command assembly for one vendored-crate build.
//!
//! This module is to vendor builds what
//! [`super::command_builder::build_rustc_command`] is to user-declared
//! crates: a pure command builder that takes a fully-resolved input
//! bundle and returns a [`RustcCommandBuilder`] plus the expected
//! output rlib path. No process spawning, no cache integration —
//! that's the scheduler's job.
//!
//! # Why not reuse `build_rustc_command`?
//!
//! The user-crate path takes a `CompileCrateInput { model, resolved,
//! crate_ref, artifacts, sysroot_dir }` — heavily coupled to the model
//! arena. Vendor builds don't have a `CrateDef` (they're synthesised
//! from cargo metadata at vendor_sync time, not declared in the user's
//! `gluon.rs`), don't have a `ResolvedCrateRef`, and don't need most
//! of `ResolvedConfig`. Bending vendor inputs to fit the existing
//! shape would require placeholder fields and inverted control flow.
//!
//! A future refactor may extract a shared "rustc inputs core" both
//! shapes hand off to, but the cost of inventing that abstraction now
//! is high and the cost of two parallel command-builders is low.
//!
//! # Profiles
//!
//! Cross units take the caller's `(opt_level, debug_info)`, which the
//! scheduler reads from the active profile; their output directory is
//! profile-keyed. Host units ignore both and use
//! [`super::HOST_OPT_LEVEL`] and [`super::HOST_DEBUG_INFO`], because
//! their output directory is shared by every profile.
//!
//! # Lints
//!
//! Every vendored unit gets `--cap-lints allow`, as Cargo does for
//! non-workspace crates. The user cannot fix lints in vendored code,
//! and a `#![deny(..)]` in a dependency must not turn a newer
//! nightly's new lint into a build failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gluon_model::{CrateType, TargetSpec};

use super::compile_utils::proc_macro_file_name;
use super::rustc::{Emit, RustcCommandBuilder};
use crate::vendor::cross::graph::VendorCrateBuild;

#[derive(Debug, thiserror::Error)]
pub enum VendorCommandError {
    /// Cross build was requested but no `--target` triple was supplied.
    /// This is a scheduler bug — the scheduler must derive the triple
    /// from the build's `TargetSpec::Cross(handle)` before calling.
    #[error(
        "vendor build {crate_name} targets Cross(_) but no target triple was supplied \
         (scheduler bug)"
    )]
    MissingTargetTriple { crate_name: String },

    /// Cross build was requested but no sysroot directory was supplied.
    /// Same scheduler-bug class as `MissingTargetTriple`.
    #[error(
        "vendor build {crate_name} targets Cross(_) but no sysroot directory was supplied \
         (scheduler bug)"
    )]
    MissingSysroot { crate_name: String },

    /// A dep edge declares an extern name with no corresponding entry
    /// in `extern_artifacts`. The scheduler must build every dep before
    /// invoking the parent build's command.
    #[error(
        "vendor build {crate_name} has dep edge `{extern_name}` with no resolved artifact \
         in extern_artifacts (scheduler bug — dep not built before parent)"
    )]
    UnresolvedDepEdge {
        crate_name: String,
        extern_name: String,
    },
}

/// Inputs to [`build_vendor_rustc_command`]. Bundled into a struct so
/// the call signature stays legible — every field is required, and
/// the scheduler resolves them all upfront.
pub struct VendorCompileInput<'a> {
    /// The vendor build to compile.
    pub build: &'a VendorCrateBuild,
    /// Map from `extern_name` to the path of the dep's already-built
    /// artifact (rlib for libs, .so/.dylib for proc-macros). The
    /// scheduler must populate one entry per `build.deps` edge before
    /// calling — a missing entry produces [`VendorCommandError::UnresolvedDepEdge`].
    pub extern_artifacts: &'a BTreeMap<String, PathBuf>,
    /// Per-target sysroot directory. Required when
    /// `build.target == TargetSpec::Cross(_)`; ignored when host. The
    /// caller derives via [`super::BuildLayout::sysroot_dir`] for the
    /// target the scheduler resolved from `build.target`.
    pub sysroot_dir: Option<&'a Path>,
    /// Directories holding the artifacts of transitive deps, each passed
    /// as `-L dependency=<dir>`. `--extern` covers only direct deps, but
    /// rustc also loads every crate recorded in a direct dep's metadata
    /// by name, so it needs a search path for the rest. Each vendor
    /// unit has its own output directory, so this holds one entry per
    /// transitive dep. Sorted and deduplicated by the caller.
    pub search_dirs: &'a [PathBuf],
    /// Output directory for this crate's artifacts. Caller derives via
    /// [`super::BuildLayout::host_vendor_dep_dir`] or
    /// [`super::BuildLayout::cross_vendor_dep_dir`].
    pub out_dir: &'a Path,
    /// Path to the rustc binary to invoke. Usually
    /// `RustcInfo::probe()?.path()`.
    pub rustc: &'a Path,
    /// Target triple string (e.g. `"x86_64-unknown-none"`). Required
    /// for cross builds, ignored for host. Pulled from the
    /// `TargetDef::spec` field of the cross target.
    pub target_triple: Option<&'a str>,
    /// Whether `target_triple` is a built-in rustc triple (vs. a path
    /// to a JSON spec). Mirrors `TargetDef::builtin`.
    pub target_triple_builtin: bool,
    /// Optimization level (`-C opt-level=N`) from the active profile.
    /// Applies to cross units only; host units use
    /// [`super::HOST_OPT_LEVEL`].
    pub opt_level: u8,
    /// Whether to emit debug info (`-C debuginfo=2`) from the active
    /// profile. Applies to cross units only; host units use
    /// [`super::HOST_DEBUG_INFO`].
    pub debug_info: bool,
}

/// Assemble the rustc invocation for one vendored-crate build.
///
/// Returns the configured [`RustcCommandBuilder`], the path the
/// resulting rlib (or .so for proc-macros) will live at after rustc
/// completes (`<out_dir>/lib<crate>.rlib` for libs,
/// `<out_dir>/lib<crate>.so` for proc-macros on Linux, `.dylib` on
/// macOS, `.dll` on Windows), and the depfile path
/// (`<out_dir>/<crate>.d`).
///
/// Both paths are computed by mirroring the `--crate-name` /
/// `--out-dir` rules rustc itself uses, *not* by inspecting the
/// filesystem after the fact. That keeps them predictable to the
/// scheduler before the build runs — the cache layer needs the
/// depfile path *before* spawning rustc to seed its freshness query
/// from the previous run's depfile contents.
///
/// Vendor builds emit `[Emit::Link, Emit::DepInfo]`. The depfile is
/// not optional: gluon's cache layer requires a depfile after every
/// successful rustc spawn so source-file changes (`cargo vendor` re-
/// extracting an upstream source-only patch onto the same lockfile
/// version) invalidate downstream entries. See
/// [`super::compile_and_cache`] for the freshness contract.
pub fn build_vendor_rustc_command(
    input: &VendorCompileInput<'_>,
) -> Result<(RustcCommandBuilder, PathBuf, PathBuf), VendorCommandError> {
    let build = input.build;
    let crate_name = &build.cargo_target_name;

    // --- Validate scheduler invariants for cross builds upfront ---
    let is_cross = matches!(build.target, TargetSpec::Cross(_));
    if is_cross && input.target_triple.is_none() {
        return Err(VendorCommandError::MissingTargetTriple {
            crate_name: crate_name.clone(),
        });
    }
    if is_cross && input.sysroot_dir.is_none() {
        return Err(VendorCommandError::MissingSysroot {
            crate_name: crate_name.clone(),
        });
    }

    let mut b = RustcCommandBuilder::new(input.rustc);

    // --- Crate identity / source ---
    b.input(&build.lib_path);
    b.crate_name(crate_name);
    b.crate_type(crate_kind_to_type(build));
    b.edition(&build.edition);

    // --- Target / sysroot (cross only) ---
    // Host builds compile against the system rustc + system stdlib; no
    // --target / --sysroot. Cross builds use gluon's per-target sysroot
    // (which already contains core/alloc/compiler_builtins).
    if let Some(triple) = input.target_triple
        && is_cross
    {
        b.target(triple, input.target_triple_builtin);
    }
    if let Some(sysroot) = input.sysroot_dir
        && is_cross
    {
        b.sysroot(sysroot);
    }

    // --- Output ---
    b.out_dir(input.out_dir);
    // Emit both the artifact (rlib/.so) and a depfile. The depfile lives
    // alongside the artifact at `<out_dir>/<crate>.d`; threading the path
    // through `emit_with_dep_info_path` keeps the "rustc paths predictable
    // before the spawn" invariant (the cache layer needs the depfile path
    // up-front to seed its freshness query from the previous run).
    let depfile_path = input.out_dir.join(format!("{crate_name}.d"));
    b.emit_with_dep_info_path(&[Emit::Link, Emit::DepInfo], &depfile_path);

    // --- Profile knobs ---
    // Host units share one profile-independent output directory, so
    // they must not take the active profile's flags. See the module
    // docs.
    if is_cross {
        b.opt_level(input.opt_level);
        b.debug_info(input.debug_info);
    } else {
        b.opt_level(super::HOST_OPT_LEVEL);
        b.debug_info(super::HOST_DEBUG_INFO);
    }
    b.raw_arg("--cap-lints").raw_arg("allow");

    // --- Features → --cfg feature="<name>" ---
    // Cargo's contract: every feature in the resolved set becomes a
    // `feature="<name>"` cfg flag for the rustc invocation. Iteration
    // order is fixed by the input slice (already sorted in
    // ResolvedCrate / VendorCrateBuild).
    for feat in &build.features {
        b.cfg(&format!("feature=\"{feat}\""));
    }

    // --- Dep edges → --extern <name>=<path> ---
    // Iterate `build.deps` (already sorted by extern_name in
    // graph::build_vendor_nodes), look each one up in extern_artifacts.
    // A missing entry is a scheduler bug — surface it loudly rather
    // than silently dropping the edge.
    for edge in &build.deps {
        let path = input
            .extern_artifacts
            .get(&edge.extern_name)
            .ok_or_else(|| VendorCommandError::UnresolvedDepEdge {
                crate_name: crate_name.clone(),
                extern_name: edge.extern_name.clone(),
            })?;
        b.extern_crate(&edge.extern_name, path);
    }
    for dir in input.search_dirs {
        b.library_search_dependency(dir);
    }

    // --- Output path ---
    // We don't pass `-C extra-filename`: every unit gets its own
    // out_dir, keyed by platform (host vs cross/<target>/<profile>), so
    // a host and a cross unit of one crate never share a file.
    let out_path = expected_artifact_path(input.out_dir, crate_name, build);
    Ok((b, out_path, depfile_path))
}

fn crate_kind_to_type(build: &VendorCrateBuild) -> CrateType {
    match build.kind {
        crate::vendor::cross::metadata::CrateKind::Lib => CrateType::Lib,
        crate::vendor::cross::metadata::CrateKind::ProcMacro => CrateType::ProcMacro,
    }
}

fn expected_artifact_path(out_dir: &Path, crate_name: &str, build: &VendorCrateBuild) -> PathBuf {
    use crate::vendor::cross::metadata::CrateKind;
    match build.kind {
        CrateKind::Lib => out_dir.join(format!("lib{crate_name}.rlib")),
        CrateKind::ProcMacro => out_dir.join(proc_macro_file_name(crate_name)),
    }
}

// -------------------- tests --------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::cross::graph::{VendorCrateBuild, VendorDepEdge};
    use crate::vendor::cross::metadata::{CrateId, CrateKind};
    use gluon_model::{Handle, TargetDef};
    use std::ffi::OsString;

    fn cross_target() -> TargetSpec {
        TargetSpec::Cross(Handle::<TargetDef>::new(0))
    }

    fn build_lib(name: &str, target: TargetSpec, deps: Vec<VendorDepEdge>) -> VendorCrateBuild {
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: "1.0.0".into(),
            },
            cargo_target_name: name.replace('-', "_"),
            target,
            kind: CrateKind::Lib,
            manifest_dir: PathBuf::from(format!("/vendor/{name}-1.0.0")),
            lib_path: PathBuf::from(format!("/vendor/{name}-1.0.0/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps,
        }
    }

    fn build_proc_macro(name: &str) -> VendorCrateBuild {
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: "1.0.0".into(),
            },
            cargo_target_name: name.replace('-', "_"),
            target: TargetSpec::Host,
            kind: CrateKind::ProcMacro,
            manifest_dir: PathBuf::from(format!("/vendor/{name}-1.0.0")),
            lib_path: PathBuf::from(format!("/vendor/{name}-1.0.0/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps: vec![],
        }
    }

    fn rustc_path() -> PathBuf {
        PathBuf::from("/usr/bin/rustc")
    }

    fn args_to_strings(b: &RustcCommandBuilder) -> Vec<String> {
        b.args()
            .iter()
            .map(|a: &OsString| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn cross_lib_emits_target_and_sysroot() {
        let b = build_lib("bitflags", cross_target(), vec![]);
        let externs = BTreeMap::new();
        let out_dir = PathBuf::from("/build/cross/x86_64-test/dev/vendor-deps/bitflags");
        let sysroot = PathBuf::from("/build/sysroot/x86_64-test");
        let rustc = rustc_path();

        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 0,
            debug_info: true,
        };
        let (cmd, out_path, depfile_path) = build_vendor_rustc_command(&input).expect("ok");

        let args = args_to_strings(&cmd);
        assert!(
            args.iter().any(|a| a == "--target=x86_64-unknown-none"),
            "missing --target: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--sysroot"),
            "missing --sysroot: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "lib"),
            "missing --crate-type lib: {args:?}"
        );
        assert_eq!(
            out_path,
            PathBuf::from("/build/cross/x86_64-test/dev/vendor-deps/bitflags/libbitflags.rlib"),
        );
        assert_eq!(
            depfile_path,
            PathBuf::from("/build/cross/x86_64-test/dev/vendor-deps/bitflags/bitflags.d"),
        );
    }

    #[test]
    fn host_proc_macro_omits_target_and_sysroot() {
        let b = build_proc_macro("syn");
        let externs = BTreeMap::new();
        let out_dir = PathBuf::from("/build/host/vendor-deps/syn");
        let rustc = rustc_path();

        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: None,
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: None,
            target_triple_builtin: false,
            opt_level: 3,
            debug_info: false,
        };
        let (cmd, out_path, depfile_path) = build_vendor_rustc_command(&input).expect("ok");

        // Depfile path mirrors the artifact's basename without the `lib`
        // prefix or extension — `<out_dir>/<crate_name>.d`.
        assert_eq!(depfile_path, out_dir.join("syn.d"));

        let args = args_to_strings(&cmd);
        assert!(
            !args.iter().any(|a| a.starts_with("--target=")),
            "host build must not pass --target: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--sysroot"),
            "host build must not pass --sysroot: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "proc-macro"),
            "missing --crate-type proc-macro: {args:?}"
        );
        // Same naming rule as project proc macros: host DLL prefix and
        // extension, so `syn.dll` on Windows, not `libsyn.dll`.
        assert_eq!(out_path, out_dir.join(proc_macro_file_name("syn")));

        // Host units ignore the profile passed in (opt 3, no debug) and
        // use the fixed host profile.
        assert!(args.iter().any(|a| a == "opt-level=0"), "{args:?}");
        assert!(!args.iter().any(|a| a == "opt-level=3"), "{args:?}");
        assert!(args.iter().any(|a| a == "debuginfo=0"), "{args:?}");
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn vendored_units_cap_lints_on_both_platforms() {
        let cross = build_lib("bitflags", cross_target(), vec![]);
        let host = build_proc_macro("syn");
        let externs = BTreeMap::new();
        let sysroot = PathBuf::from("/sysroot");
        let out_dir = PathBuf::from("/out");
        let rustc = rustc_path();
        for (b, sysroot_dir, triple) in [
            (&cross, Some(sysroot.as_path()), Some("x86_64-unknown-none")),
            (&host, None, None),
        ] {
            let input = VendorCompileInput {
                build: b,
                extern_artifacts: &externs,
                search_dirs: &[],
                sysroot_dir,
                out_dir: &out_dir,
                rustc: &rustc,
                target_triple: triple,
                target_triple_builtin: true,
                opt_level: 2,
                debug_info: true,
            };
            let (cmd, _, _) = build_vendor_rustc_command(&input).expect("ok");
            let args = args_to_strings(&cmd);
            assert!(has_pair(&args, "--cap-lints", "allow"), "{args:?}");
        }
    }

    #[test]
    fn cross_unit_takes_active_profile() {
        let b = build_lib("bitflags", cross_target(), vec![]);
        let externs = BTreeMap::new();
        let sysroot = PathBuf::from("/sysroot");
        let out_dir = PathBuf::from("/out");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 3,
            debug_info: false,
        };
        let (cmd, _, _) = build_vendor_rustc_command(&input).expect("ok");
        let args = args_to_strings(&cmd);
        assert!(args.iter().any(|a| a == "opt-level=3"), "{args:?}");
    }

    #[test]
    fn search_dirs_become_dependency_search_paths() {
        let b = build_proc_macro("derive");
        let externs = BTreeMap::new();
        let dirs = vec![
            PathBuf::from("/build/host/vendor-deps/proc_macro2"),
            PathBuf::from("/build/host/vendor-deps/unicode_ident"),
        ];
        let out_dir = PathBuf::from("/out");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &dirs,
            sysroot_dir: None,
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: None,
            target_triple_builtin: false,
            opt_level: 0,
            debug_info: false,
        };
        let (cmd, _, _) = build_vendor_rustc_command(&input).expect("ok");
        let args = args_to_strings(&cmd);
        for dir in &dirs {
            let value = format!("dependency={}", dir.display());
            assert!(has_pair(&args, "-L", &value), "{args:?}");
        }
    }

    #[test]
    fn dep_edges_become_extern_args_in_sorted_order() {
        let deps = vec![
            VendorDepEdge {
                extern_name: "leaf".into(),
                crate_id: CrateId {
                    name: "leaf".into(),
                    version: "0.1.0".into(),
                },
                target: cross_target(),
            },
            VendorDepEdge {
                extern_name: "pm".into(),
                crate_id: CrateId {
                    name: "proc-macro2".into(),
                    version: "1.0.0".into(),
                },
                target: TargetSpec::Host,
            },
        ];
        let b = build_lib("bitflags", cross_target(), deps);

        let mut externs: BTreeMap<String, PathBuf> = BTreeMap::new();
        externs.insert("leaf".into(), PathBuf::from("/build/leaf/libleaf.rlib"));
        externs.insert("pm".into(), PathBuf::from("/build/pm/libproc_macro2.so"));

        let out_dir = PathBuf::from("/build/cross/t/p/vendor-deps/bitflags");
        let sysroot = PathBuf::from("/sysroot");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 0,
            debug_info: true,
        };
        let (cmd, _, _) = build_vendor_rustc_command(&input).expect("ok");
        let args = args_to_strings(&cmd);

        // Both --extern entries present, with the right paths.
        assert!(
            args.iter().any(|a| a == "leaf=/build/leaf/libleaf.rlib"),
            "missing leaf extern: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "pm=/build/pm/libproc_macro2.so"),
            "missing pm extern: {args:?}"
        );
    }

    #[test]
    fn features_become_cfg_feature_flags() {
        let mut b = build_lib("bitflags", cross_target(), vec![]);
        b.features = vec!["alloc".into(), "std".into()];
        let externs = BTreeMap::new();
        let out_dir = PathBuf::from("/out");
        let sysroot = PathBuf::from("/sysroot");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 0,
            debug_info: true,
        };
        let (cmd, _, _) = build_vendor_rustc_command(&input).expect("ok");
        let args = args_to_strings(&cmd);
        assert!(
            args.iter().any(|a| a == "feature=\"alloc\""),
            "missing alloc cfg: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "feature=\"std\""),
            "missing std cfg: {args:?}"
        );
    }

    #[test]
    fn emit_token_contains_dep_info_with_explicit_path() {
        // Explicit-path depfile is what `compile_and_cache` relies on:
        // it needs to know the depfile path BEFORE rustc spawns, to seed
        // the freshness query. If rustc were left to derive the path
        // from --out-dir + --crate-name itself (the default), the cache
        // layer would have to reconstruct that derivation by hand.
        let b = build_lib("bitflags", cross_target(), vec![]);
        let externs = BTreeMap::new();
        let out_dir = PathBuf::from("/build/cross/x86_64-test/dev/vendor-deps/bitflags");
        let sysroot = PathBuf::from("/sysroot");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 0,
            debug_info: true,
        };
        let (cmd, _, depfile_path) = build_vendor_rustc_command(&input).expect("ok");
        let args = args_to_strings(&cmd);
        let expected_emit = format!("--emit=link,dep-info={}", depfile_path.display());
        assert!(
            args.iter().any(|a| a == &expected_emit),
            "expected emit token {expected_emit:?} not found in args: {args:?}",
        );
    }

    #[test]
    fn cross_build_without_target_triple_errors() {
        let b = build_lib("bitflags", cross_target(), vec![]);
        let externs = BTreeMap::new();
        let out_dir = PathBuf::from("/out");
        let sysroot = PathBuf::from("/sysroot");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: None,
            target_triple_builtin: false,
            opt_level: 0,
            debug_info: true,
        };
        let err = build_vendor_rustc_command(&input).expect_err("must error");
        assert!(matches!(
            err,
            VendorCommandError::MissingTargetTriple { .. }
        ));
    }

    #[test]
    fn cross_build_without_sysroot_errors() {
        let b = build_lib("bitflags", cross_target(), vec![]);
        let externs = BTreeMap::new();
        let out_dir = PathBuf::from("/out");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: None,
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 0,
            debug_info: true,
        };
        let err = build_vendor_rustc_command(&input).expect_err("must error");
        assert!(matches!(err, VendorCommandError::MissingSysroot { .. }));
    }

    #[test]
    fn missing_extern_artifact_for_dep_errors() {
        let deps = vec![VendorDepEdge {
            extern_name: "missing".into(),
            crate_id: CrateId {
                name: "missing".into(),
                version: "1.0.0".into(),
            },
            target: cross_target(),
        }];
        let b = build_lib("bitflags", cross_target(), deps);
        let externs = BTreeMap::new(); // empty — dep not built
        let out_dir = PathBuf::from("/out");
        let sysroot = PathBuf::from("/sysroot");
        let rustc = rustc_path();
        let input = VendorCompileInput {
            build: &b,
            extern_artifacts: &externs,
            search_dirs: &[],
            sysroot_dir: Some(&sysroot),
            out_dir: &out_dir,
            rustc: &rustc,
            target_triple: Some("x86_64-unknown-none"),
            target_triple_builtin: true,
            opt_level: 0,
            debug_info: true,
        };
        let err = build_vendor_rustc_command(&input).expect_err("must error");
        match err {
            VendorCommandError::UnresolvedDepEdge {
                crate_name,
                extern_name,
            } => {
                assert_eq!(crate_name, "bitflags");
                assert_eq!(extern_name, "missing");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
