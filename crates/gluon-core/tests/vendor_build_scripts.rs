//! End-to-end: vendored crates with build scripts, built offline.
//!
//! The `vendor-cross-build-script` fixture carries its own vendor
//! directory. The test writes the cross vendor workspace the way `gluon
//! vendor` would, points it at that directory, and runs `gluon build`
//! through the library. See the fixture's `gluon.rs` for the crates.
//!
//! The build spawns rustc and builds a sysroot, so it is `#[ignore]`d
//! like the other build tests.

use gluon_core::compile::BuildLayout;
use gluon_model::TargetSpec;
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
    model: gluon_core::model::BuildModel,
    resolved: gluon_model::ResolvedConfig,
    rustc_info: Arc<gluon_core::RustcInfo>,
}

impl Project {
    fn stage() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("project");
        copy_dir_recursive(&fixture_dir("vendor-cross-build-script"), &root);
        let model = gluon_core::load_build_model(&root).expect("load_build_model");
        let resolved =
            gluon_core::resolve_config(&model, "dev", None, &root, None).expect("resolve_config");

        // What `gluon vendor` leaves behind: the workspace manifest, the
        // vendored crates and a lockfile.
        let layout = layout(&root);
        let scratch = layout.vendor_workspace_for_target(TARGET);
        let target = model.targets.lookup(TARGET).expect("target");
        gluon_core::vendor::manifest_gen::write_vendor_workspace(
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

        let rustc_info = Arc::new(gluon_core::RustcInfo::load_or_probe(&layout).expect("probe"));
        Self {
            _tmp: tmp,
            root,
            vendor,
            model,
            resolved,
            rustc_info,
        }
    }

    /// Run a build; return `(built, cached)`.
    fn build(&self) -> (usize, usize) {
        let ctx = gluon_core::CompileCtx::new(layout(&self.root), Arc::clone(&self.rustc_info));
        let summary = gluon_core::build_with_workers(&ctx, &self.model, &self.resolved, 4)
            .expect("gluon build");
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
    match gluon_core::RustcInfo::probe() {
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
    assert!(built > 0 && cached == 0, "built {built}, cached {cached}");

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
    assert_eq!(p.build(), (0, built), "second build");

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
