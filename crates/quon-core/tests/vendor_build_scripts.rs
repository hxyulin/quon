//! End-to-end: vendored crates with build scripts, built offline.
//!
//! The `vendor-cross-build-script` fixture carries its own vendor
//! directory. The test writes the cross vendor workspace the way `quon
//! vendor` would, points it at that directory, and runs `quon build`
//! through the library. See the fixture's `quon.rs` for the crates.
//!
//! The build spawns rustc and builds a sysroot, so it is `#[ignore]`d
//! like the other build tests.
//!
//! The tests behind the `network-tests` feature vendor real crates
//! (zerocopy, bytemuck_derive and the syn stack) from crates.io.

use quon_core::compile::BuildLayout;
use quon_model::TargetSpec;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const TARGET: &str = "x86_64-unknown-none";

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

/// The one run directory (it holds `out/`) under `dir` whose name
/// starts with `prefix`. Compiled scripts sit next to runs of host
/// owners.
fn only_run_dir(dir: &Path, prefix: &str) -> PathBuf {
    let found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("out").is_dir())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{prefix}* under {}: {found:?}",
        dir.display()
    );
    found.into_iter().next().unwrap()
}

/// Every file under `dir` whose name contains `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_containing(&path, needle));
        } else if path.file_name().unwrap().to_string_lossy().contains(needle) {
            out.push(path);
        }
    }
    out
}

struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    vendor: PathBuf,
    model: quon_core::model::BuildModel,
    resolved: quon_model::ResolvedConfig,
    rustc_info: Arc<quon_core::RustcInfo>,
    /// Sysroots copied in before the first build, which counts them as
    /// cached.
    seeded: usize,
}

impl Project {
    fn stage() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("project");
        copy_dir_recursive(&fixture_dir("vendor-cross-build-script"), &root);
        let model = quon_core::load_build_model(&root).expect("load_build_model");
        let resolved =
            quon_core::resolve_config(&model, "dev", None, &root, None).expect("resolve_config");

        // What `quon vendor` leaves behind: the workspace manifest, the
        // vendored crates and a lockfile.
        let layout = layout(&root);
        let scratch = layout.vendor_workspace_for_target(TARGET);
        let target = model.targets.lookup(TARGET).expect("target");
        quon_core::vendor::manifest_gen::write_vendor_workspace(
            &scratch,
            &model.external_deps,
            TargetSpec::Cross(target),
        )
        .expect("write vendor workspace");
        let vendor = scratch.join("vendor");
        copy_dir_recursive(&root.join("vendor"), &vendor);
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(scratch.join("Cargo.toml"))
            .args([
                "--config",
                "source.crates-io.replace-with=\"vendored-sources\"",
            ])
            .arg("--config")
            .arg(format!(
                "source.vendored-sources.directory={:?}",
                vendor.to_str().unwrap()
            ))
            .status()
            .expect("spawn cargo generate-lockfile");
        assert!(status.success(), "cargo generate-lockfile failed");

        let rustc_info = Arc::new(quon_core::RustcInfo::load_or_probe(&layout).expect("probe"));
        let seeded = quon_core::testing::seed_sysroots(
            &root.join("build"),
            &model,
            rustc_info.rustc_arg.as_ref(),
        )
        .expect("seed sysroot");
        Self {
            _tmp: tmp,
            root,
            vendor,
            model,
            resolved,
            rustc_info,
            seeded,
        }
    }

    /// Run a build; return `(built, cached)`.
    fn build(&self) -> (usize, usize) {
        let ctx = quon_core::CompileCtx::new(layout(&self.root), Arc::clone(&self.rustc_info));
        let summary = quon_core::build_with_workers(&ctx, &self.model, &self.resolved, 4)
            .expect("quon build");
        (summary.built, summary.cached)
    }

    fn edit(&self, rel: &str, contents: &str) {
        fs::write(self.vendor.join(rel), contents).expect("edit vendored file");
    }
}

fn layout(root: &Path) -> BuildLayout {
    BuildLayout::new(root.join("build"), "vendor-cross-build-script")
}

#[test]
#[ignore]
fn vendored_build_scripts_run_and_rerun_only_when_their_inputs_change() {
    match quon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => {}
        _ => {
            eprintln!("skipped: needs rustc with the rust-src component");
            return;
        }
    }
    let p = Project::stage();
    let host_triple = p.rustc_info.host_triple.clone();

    // Cold build. The fixture's crates fail to compile unless their
    // scripts ran with the right environment: pm2 and probe-sys need
    // their probes to succeed, hello-derive needs its build-dep's cfg,
    // consumer needs DEP_PROBE_KEY and CARGO_CFG_TARGET_OS.
    let (built, cached) = p.build();
    assert!(
        built > 0 && cached == p.seeded,
        "built {built}, cached {cached}"
    );

    let build = p.root.join("build");
    let host_runs = build.join("host/build");
    let target_runs = build.join(format!("cross/{TARGET}/dev/build"));
    let read = |dir: PathBuf| fs::read_to_string(dir.join("out/target.txt")).unwrap();
    assert_eq!(
        read(only_run_dir(&host_runs, "pm2-")),
        host_triple,
        "pm2 is a dep of a proc macro, so its script configures the host"
    );
    assert_eq!(read(only_run_dir(&target_runs, "probe-sys-")), TARGET);
    assert!(
        only_run_dir(&target_runs, "probe-sys-")
            .join("output")
            .is_file()
    );

    // The proc macro and its deps exist only on the host.
    assert!(
        files_containing(&build.join("cross"), "pm2").is_empty(),
        "{:?}",
        files_containing(&build.join("cross"), "pm2")
    );
    assert!(files_containing(&build.join("cross"), "hello_derive").is_empty());
    let dylibs: Vec<PathBuf> = files_containing(&build.join("host/deps"), "hello_derive-")
        .into_iter()
        .filter(|p| p.extension() == Some(std::ffi::OsStr::new(std::env::consts::DLL_EXTENSION)))
        .collect();
    assert_eq!(dylibs.len(), 1, "{dylibs:?}");

    // Nothing changed: nothing runs.
    assert_eq!(p.build(), (0, built + p.seeded), "second build");

    // A rerun-if-changed input changes but the script prints the same
    // directives (it trims the file): only that run reruns.
    p.edit("probe-sys/probe.txt", "plain\n\n");
    assert_eq!(p.build().0, 1, "only probe-sys's run");

    // The output changes: the owner and its dependents rebuild, and
    // nothing else. consumer's run stays fresh: its DEP_PROBE_KEY did
    // not change.
    p.edit("probe-sys/probe.txt", "fancy\n");
    assert_eq!(
        p.build().0,
        4,
        "probe-sys's run, then probe-sys, consumer and kernel"
    );

    // A file the script does not name changes nothing.
    p.edit("probe-sys/README.md", "edited\n");
    assert_eq!(p.build().0, 0, "unrelated file");

    // A script that names no input reruns on any package edit, and its
    // unchanged output leaves the owner fresh.
    p.edit("hello-derive/README.md", "edited\n");
    assert_eq!(p.build().0, 1, "only hello-derive's run");
}

// ------------------------------------------------------------------
// Network-gated: real crates.io packages through `cargo vendor`.
// ------------------------------------------------------------------

/// Copy `fixture`, optionally install `quon_file` as its `quon.rs`,
/// vendor its cross deps from crates.io, and build it. Returns the
/// project root, or `None` when the toolchain cannot build sysroots.
#[cfg(feature = "network-tests")]
fn vendor_and_build(
    tmp: &Path,
    fixture: &str,
    quon_file: Option<&str>,
    prepare: impl FnOnce(&Path),
) -> Option<PathBuf> {
    match quon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => {}
        _ => {
            eprintln!("skipped: needs rustc with the rust-src component");
            return None;
        }
    }
    let root = tmp.join(fixture);
    copy_dir_recursive(&fixture_dir(fixture), &root);
    if let Some(file) = quon_file {
        fs::copy(root.join(file), root.join("quon.rs")).expect("install quon.rs");
    }
    prepare(&root);
    let model = quon_core::load_build_model(&root).expect("load_build_model");
    let resolved =
        quon_core::resolve_config(&model, "dev", None, &root, None).expect("resolve_config");
    let layout = BuildLayout::new(root.join("build"), fixture);
    quon_core::vendor::cross::populate_cross_vendor_workspaces(
        &model,
        &layout,
        quon_core::vendor::VendorOptions::default(),
    )
    .expect("cargo vendor (network)");
    let rustc_info = Arc::new(quon_core::RustcInfo::load_or_probe(&layout).expect("probe"));
    quon_core::testing::seed_sysroots(&root.join("build"), &model, rustc_info.rustc_arg.as_ref())
        .expect("seed sysroot");
    let ctx = quon_core::CompileCtx::new(layout, rustc_info);
    quon_core::build_with_workers(&ctx, &model, &resolved, 4).expect("quon build");
    Some(root)
}

/// The host build scripts of the syn stack ran, and none of it was
/// built for the target.
#[cfg(feature = "network-tests")]
fn assert_syn_stack_is_host_only(build: &Path) {
    let host_runs = build.join("host/build");
    for pkg in ["proc-macro2-", "quote-"] {
        let run = only_run_dir(&host_runs, pkg);
        assert!(run.join("output").is_file(), "{}", run.display());
    }
    for name in ["libsyn", "libproc_macro2", "libquote"] {
        assert!(
            files_containing(&build.join("cross"), name).is_empty(),
            "{name} built for the target"
        );
    }
}

/// The host dylib of proc macro `crate_name` under `build/host/deps`.
#[cfg(feature = "network-tests")]
fn host_dylib(build: &Path, crate_name: &str) -> PathBuf {
    let dylibs: Vec<PathBuf> = files_containing(&build.join("host/deps"), crate_name)
        .into_iter()
        .filter(|p| p.extension() == Some(std::ffi::OsStr::new(std::env::consts::DLL_EXTENSION)))
        .collect();
    assert_eq!(dylibs.len(), 1, "{dylibs:?}");
    dylibs.into_iter().next().unwrap()
}

/// A kernel crate that uses `bytemuck_derive` directly, as hadron's
/// crates use their derive crates.
#[cfg(feature = "network-tests")]
#[test]
fn syn_based_proc_macro_builds_on_the_host_for_a_kernel_crate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(root) = vendor_and_build(tmp.path(), "vendor-proc-macro-syn", None, |_| {}) else {
        return;
    };
    let build = root.join("build");
    assert_syn_stack_is_host_only(&build);
    host_dylib(&build, "bytemuck_derive-");
    assert!(build.join(format!("cross/{TARGET}/dev/deps")).is_dir());
}

/// zerocopy's script for a target: the run sits with the target units,
/// prints its `--check-cfg` lines and, on a current toolchain, no
/// `no_zerocopy_*` cfg.
#[cfg(feature = "network-tests")]
fn assert_zerocopy_ran_for(build: &Path, target_dir: &str) {
    let run = only_run_dir(
        &build.join(format!("cross/{target_dir}/dev/build")),
        "zerocopy-",
    );
    let output = fs::read_to_string(run.join("output")).expect("zerocopy output");
    assert!(output.contains("rustc-check-cfg="), "{output}");
    assert!(!output.contains("rustc-cfg=no_zerocopy"), "{output}");
    assert_syn_stack_is_host_only(build);
    host_dylib(build, "zerocopy_derive-");
}

#[cfg(feature = "network-tests")]
#[test]
fn zerocopy_with_derive_builds_for_a_builtin_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(root) = vendor_and_build(tmp.path(), "vendor-zerocopy", None, |_| {}) else {
        return;
    };
    assert_zerocopy_ran_for(&root.join("build"), TARGET);
}

/// The same with a custom JSON target, copied from the builtin spec, so
/// the script sees the spec's stem in `TARGET` and `RUST_TARGET_PATH`.
#[cfg(feature = "network-tests")]
#[test]
fn zerocopy_with_derive_builds_for_a_json_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let write_spec = |root: &Path| {
        let rustc = quon_core::RustcInfo::probe().expect("probe").rustc_path;
        let out = Command::new(rustc)
            .args(["-Zunstable-options", "--print", "target-spec-json"])
            .args(["--target", TARGET])
            .output()
            .expect("spawn rustc");
        assert!(
            out.status.success(),
            "rustc --print target-spec-json failed"
        );
        // rustc refuses a custom spec that claims to be builtin.
        let mut spec: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        spec.as_object_mut().unwrap().remove("is-builtin");
        let spec_path = root.join("targets/x86_64-quon-test.json");
        fs::create_dir_all(spec_path.parent().unwrap()).unwrap();
        fs::write(&spec_path, serde_json::to_vec_pretty(&spec).unwrap()).unwrap();
        // A relative spec path resolves against the working directory,
        // which is the project root under the CLI but not in this test
        // process, so point quon.rs at the absolute path.
        let quon_rs = root.join("quon.rs");
        let text = fs::read_to_string(&quon_rs).unwrap();
        let text = text.replace(
            "\"targets/x86_64-quon-test.json\"",
            &format!("{:?}", spec_path.to_str().unwrap()),
        );
        fs::write(&quon_rs, text).unwrap();
    };
    let Some(root) = vendor_and_build(
        tmp.path(),
        "vendor-zerocopy",
        Some("quon.json-target.rs"),
        write_spec,
    ) else {
        return;
    };
    assert_zerocopy_ran_for(&root.join("build"), "x86_64-quon-test");
}
