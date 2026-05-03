//! End-to-end tests for sub-project #3 vendoring.
//!
//! These exercise the full pipeline:
//!
//!   gluon.rs → load_build_model → vendor_sync → auto_register_vendored_deps
//!
//! against a real fixture on disk. Unit tests already cover every
//! individual helper under `crates/gluon-core/src/vendor/`; these are
//! the tests that catch integration bugs between the Config builder,
//! the lockfile format, and the model load path.
//!
//! # Two tiers
//!
//! 1. **Path-only**: always runs. Uses `gluon.path-only.rs` which
//!    declares a single `DepSource::Path` dep and never touches the
//!    network. Exercises the `.path(...)` builder, the
//!    `vendor_sync` Path branch, lockfile round-trip,
//!    fingerprint-stable fast path, and auto-registration.
//!
//! 2. **Network-gated**: runs only when
//!    `--features gluon-core/network-tests` is enabled. Uses the full
//!    `gluon.rs` which adds a real `bitflags` crates.io dep and
//!    actually shells out to `cargo vendor`. Catches regressions in
//!    the manifest generator, `cargo vendor` wrapper, and directory
//!    checksum.

use gluon_core::compile::BuildLayout;
use gluon_core::model::{CrateType, DepSource};
use gluon_core::vendor::{self, VendorOptions};
use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from `CARGO_MANIFEST_DIR` (which is `crates/gluon-core`
/// for this test crate) to the workspace root, then descend into
/// `tests/fixtures/<name>`. Mirrors the helper in `kconfig_e2e.rs`.
fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gluon-core has a two-level parent")
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Copy the fixture into a tempdir so tests don't mutate the tree
/// under version control. `gluon.lock` and `build/vendor-workspace/`
/// both live inside the project root, so doing this in place would
/// leave files behind across runs.
///
/// The sibling `vendor-minimal-helper` fixture has to come along too
/// because the build script references it via `../vendor-minimal-helper`.
/// We preserve the relative sibling layout by copying both into the
/// same parent inside the tempdir.
fn stage_fixture(tmp: &Path) -> PathBuf {
    let project_src = fixture_dir("vendor-minimal");
    let helper_src = fixture_dir("vendor-minimal-helper");

    let project_dst = tmp.join("vendor-minimal");
    let helper_dst = tmp.join("vendor-minimal-helper");

    copy_dir_recursive(&project_src, &project_dst);
    copy_dir_recursive(&helper_src, &helper_dst);

    project_dst
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

/// Install the path-only build script as `gluon.rs` so
/// `load_build_model` picks the path-only form.
fn use_path_only_script(project: &Path) {
    fs::copy(project.join("gluon.path-only.rs"), project.join("gluon.rs"))
        .expect("install path-only gluon.rs");
}

#[test]
#[ignore]
fn path_only_fixture_vendors_and_auto_registers() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!("path_only_fixture_vendors_and_auto_registers: skipped — rustc probe failed");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = stage_fixture(tmp.path());
    use_path_only_script(&project);

    // Load the model via load_build_model. Pre-check the model has
    // our path dep before vendor_sync to make sure the .path(...)
    // builder method actually wired it up.
    let model = gluon_core::load_build_model(&project).expect("load_build_model");
    let helper_handle = model
        .external_deps
        .lookup("vendor_minimal_helper")
        .expect("path dep registered");
    let helper = model.external_deps.get(helper_handle).unwrap();
    match &helper.source {
        DepSource::Path { path } => assert_eq!(path, "../vendor-minimal-helper"),
        other => panic!("expected Path source, got {other:?}"),
    }

    // Run vendor_sync. No network hit — path deps skip cargo.
    let layout = BuildLayout::new(project.join("build"), "vendor-minimal");
    let lock = vendor::vendor_sync(&model, &layout, &project, VendorOptions::default())
        .expect("vendor_sync");
    assert_eq!(lock.packages.len(), 1);
    let pkg = &lock.packages[0];
    assert_eq!(pkg.name, "vendor_minimal_helper");
    assert_eq!(pkg.version, "0.3.1");
    assert_eq!(pkg.source, "path+../vendor-minimal-helper");
    assert!(pkg.checksum.is_none(), "path deps carry no checksum");

    // gluon.lock was written at the project root.
    assert!(project.join("gluon.lock").exists());

    // vendor_check reports clean immediately after sync.
    let report = vendor::vendor_check(&model, &layout, &project).expect("vendor_check");
    assert!(report.is_clean(), "report: {report:?}");

    // Second sync is the fast path — no state change, returns the
    // existing lock unchanged.
    let lock2 = vendor::vendor_sync(&model, &layout, &project, VendorOptions::default())
        .expect("vendor_sync again");
    assert_eq!(lock, lock2);

    // Auto-register populates model.crates with a synthetic entry
    // pointing at the real path dep directory.
    let mut model_for_reg = model.clone();
    vendor::auto_register_vendored_deps(&mut model_for_reg, &layout, &project)
        .expect("auto_register");
    let crate_handle = model_for_reg
        .crates
        .lookup("vendor_minimal_helper")
        .expect("synthetic crate");
    let krate = model_for_reg.crates.get(crate_handle).unwrap();
    assert_eq!(krate.edition, "2021");
    assert_eq!(krate.crate_type, CrateType::Lib);
    let vendored_group = model_for_reg
        .groups
        .lookup(vendor::VENDORED_GROUP_NAME)
        .expect("vendored group present");
    assert_eq!(krate.group, vendored_group);
    assert!(krate.path.contains("vendor-minimal-helper"));

    // And the synthetic group is registered.
    assert!(
        model_for_reg
            .groups
            .lookup(vendor::VENDORED_GROUP_NAME)
            .is_some()
    );
}

#[test]
#[ignore]
fn path_only_fixture_detects_stale_lock_after_model_edit() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!("path_only_fixture_detects_stale_lock_after_model_edit: skipped — rustc probe failed");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = stage_fixture(tmp.path());
    use_path_only_script(&project);

    // Vendor once so gluon.lock exists.
    let model_v1 = gluon_core::load_build_model(&project).expect("load v1");
    let layout = BuildLayout::new(project.join("build"), "vendor-minimal");
    vendor::vendor_sync(&model_v1, &layout, &project, VendorOptions::default()).expect("vendor v1");

    // Build v2's model in-process: same project/target as v1 but with
    // an extra `bitflags` crates.io dep. This replicates a user editing
    // the build script after the initial vendor run, without touching
    // the on-disk gluon.rs.
    use gluon_config::Config;
    use gluon_model::intern;
    let mut cfg = Config::new();
    cfg.project("vendor-minimal", "0.1.0");
    let t = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(t).opt_level(0).debug_info(true);
    cfg.dependency("vendor_minimal_helper")
        .path("../vendor-minimal-helper");
    cfg.dependency("bitflags").version("2.11");
    let raw = cfg.into_raw();
    let (model_v2, diags) = intern(raw);
    assert!(diags.is_empty(), "intern diags: {diags:?}");

    // vendor_check must flag the mismatch.
    let report = vendor::vendor_check(&model_v2, &layout, &project).expect("check v2");
    assert!(report.fingerprint_mismatch, "{report:?}");
    assert!(!report.is_clean());

    // auto_register on the new model must bail out with the
    // stale-state diagnostic.
    let mut model_mut = model_v2.clone();
    let err = vendor::auto_register_vendored_deps(&mut model_mut, &layout, &project)
        .expect_err("stale lock must error");
    assert!(err.to_string().contains("stale"), "{err}");
}

// ------------------------------------------------------------------
// Network-gated: real cargo vendor of `bitflags` from crates.io.
// ------------------------------------------------------------------

#[cfg(feature = "network-tests")]
#[test]
fn full_fixture_vendors_bitflags_from_crates_io() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!("full_fixture_vendors_bitflags_from_crates_io: skipped — rustc probe failed");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = stage_fixture(tmp.path());

    let model = gluon_core::load_build_model(&project).expect("load_build_model");
    assert_eq!(model.external_deps.len(), 2);

    let layout = BuildLayout::new(project.join("build"), "vendor-minimal");
    let lock = vendor::vendor_sync(&model, &layout, &project, VendorOptions::default())
        .expect("vendor_sync (network)");

    // Both declared deps should be in the lock.
    assert_eq!(lock.packages.len(), 2);
    let names: Vec<&str> = lock.packages.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"bitflags"), "bitflags missing: {names:?}");
    assert!(
        names.contains(&"vendor_minimal_helper"),
        "helper missing: {names:?}"
    );

    // The bitflags entry must have a checksum (path deps don't).
    let bf = lock.packages.iter().find(|p| p.name == "bitflags").unwrap();
    assert!(bf.checksum.is_some());
    assert!(bf.version.starts_with("2.11"), "version: {}", bf.version);

    // cargo vendor populated vendor/bitflags/ (modern cargo uses
    // bare crate names — no version suffix — when there's only one
    // version of a crate in the graph). Accept either `bitflags` or
    // `bitflags-*` to be robust to cargo changing its mind later.
    let vendor_dir = layout.vendor_dir(&project);
    assert!(vendor_dir.exists(), "vendor/ missing");
    let has_bitflags = fs::read_dir(&vendor_dir)
        .expect("read vendor")
        .filter_map(|e| e.ok())
        .any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n == "bitflags" || n.starts_with("bitflags-")
        });
    assert!(has_bitflags, "no vendor/bitflags(-*)?/ dir");

    // vendor_check reports clean.
    let report = vendor::vendor_check(&model, &layout, &project).expect("check");
    assert!(report.is_clean(), "{report:?}");

    // auto-register inserts both crates into model.crates.
    let mut model_for_reg = model.clone();
    vendor::auto_register_vendored_deps(&mut model_for_reg, &layout, &project)
        .expect("auto_register");
    assert!(model_for_reg.crates.lookup("bitflags").is_some());
    assert!(
        model_for_reg
            .crates
            .lookup("vendor_minimal_helper")
            .is_some()
    );
}
