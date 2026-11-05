//! End-to-end: a multi-file `gluon.rs` (root + subsystem files via
//! `mod` + `#[path]`) compiles, runs, and produces a build model that
//! reflects each subsystem's registrations. Editing a subsystem file
//! between runs must invalidate the script-runner cache (the depfile
//! fingerprint catches it even though the entrypoint hash is unchanged).
//!
//! Mirrors the in-process pattern from `kconfig_e2e.rs` — calls
//! `gluon_core::load_build_model` directly rather than spawning the
//! `gluon` CLI. The CLI integration tests in `gluon-cli/tests/` are
//! reserved for behavior that actually exercises the binary.
//!
//! `#[ignore]`-gated and probe-or-skipped because it shells out to rustc.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from `CARGO_MANIFEST_DIR` (which is `crates/gluon-core` for
/// this test crate) to the workspace root, then descend into
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

/// Stage the on-disk fixture into a fresh tempdir so the test can edit
/// subsystem files between runs without polluting the repo. The
/// `gluon.rs` script writes its `build/` output relative to the staged
/// project root, so the tempdir is the natural sandbox.
fn stage_fixture_to_tmp(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).expect("fixture dir") {
        let entry = entry.unwrap();
        let from = entry.path();
        let name = entry.file_name();
        let to = dst.join(&name);
        if entry.file_type().unwrap().is_dir() {
            stage_fixture_to_tmp(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
#[ignore]
fn multi_file_gluon_rs_registers_each_subsystem_group() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!(
            "multi_file_gluon_rs_registers_each_subsystem_group: skipped — rustc probe failed"
        );
        return;
    }

    let src = fixture_dir("multi-file-script");
    assert!(src.is_dir(), "fixture missing at {src:?}");

    let tmp = tempfile::tempdir().expect("tmp");
    let project = tmp.path().join("project");
    stage_fixture_to_tmp(&src, &project);

    // First run: assert all three subsystem groups landed.
    let model = gluon_core::load_build_model(&project).expect("first load_build_model");
    assert!(
        model.groups.lookup("bootloader").is_some(),
        "bootloader group must be registered by bootloader/gluon.rs"
    );
    assert!(
        model.groups.lookup("kernel-v1").is_some(),
        "kernel-v1 group must be registered by kernel/gluon.rs"
    );
    assert!(
        model.groups.lookup("shared-utils").is_some(),
        "shared-utils group must be registered by shared/gluon.rs"
    );
    assert!(
        model.groups.lookup("kernel-v2").is_none(),
        "kernel-v2 must NOT exist yet — proves we're observing V1"
    );

    // Edit the kernel subsystem: rename group from `kernel-v1` to `kernel-v2`.
    // The entrypoint `gluon.rs` is byte-for-byte unchanged, so the entrypoint
    // hash is identical — only the depfile fingerprint catches this. If
    // cache invalidation is broken, V1 stays cached and `kernel-v2` is absent.
    let kernel_path = project.join("kernel/gluon.rs");
    let edited = fs::read_to_string(&kernel_path)
        .unwrap()
        .replace("kernel-v1", "kernel-v2");
    fs::write(&kernel_path, edited).expect("rewrite kernel subsystem");

    let model2 = gluon_core::load_build_model(&project).expect("second load_build_model");
    assert!(
        model2.groups.lookup("kernel-v2").is_some(),
        "edit to kernel/gluon.rs must be picked up — depfile fingerprint must invalidate the cache"
    );
    assert!(
        model2.groups.lookup("kernel-v1").is_none(),
        "post-edit, kernel-v1 must be gone — confirms we're not stitching old + new state"
    );
    assert!(
        model2.groups.lookup("bootloader").is_some(),
        "bootloader registration must still survive (only kernel was edited)"
    );
    assert!(
        model2.groups.lookup("shared-utils").is_some(),
        "shared-utils registration must still survive (only kernel was edited)"
    );
}
