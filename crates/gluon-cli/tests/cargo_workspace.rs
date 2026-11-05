//! End-to-end builds of projects whose crates come from a Cargo
//! workspace (`cfg.cargo_workspace` and `group.package`): the
//! `cargo-build` fixture and every example under `examples/`.
//!
//! Each test copies its project into a temp dir and runs the real
//! `gluon` binary there. They spawn rustc and build sysroots, so they
//! are `#[ignore]`-gated like the other build tests in `integration.rs`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Copy `rel` (relative to the repository root) into a fresh temp dir,
/// skipping build outputs, and return the temp dir and the copy.
fn setup(rel: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dst = tmp.path().join("project");
    copy_dir_all(&repo_root().join(rel), &dst).expect("copy project");
    (tmp, dst)
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        if ty.is_dir() && (name == "build" || name == "esp") {
            continue;
        }
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(name))?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dst.join(name))?;
        }
    }
    Ok(())
}

fn require_rustc_or_skip(test_name: &str) -> Option<()> {
    match gluon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => Some(()),
        Ok(_) => {
            eprintln!("{test_name}: skipped — rust-src component not installed");
            None
        }
        Err(e) => {
            eprintln!("{test_name}: skipped — rustc probe failed: {e}");
            None
        }
    }
}

/// Run `gluon build` in `project`; panic on failure, return stderr.
fn build_ok(project: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_gluon"))
        .arg("build")
        .current_dir(project)
        .output()
        .expect("spawn gluon build");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "gluon build failed:\n{stderr}");
    stderr
}

/// `(built, cached)` from the `built N, cached M` summary line.
fn summary(stderr: &str) -> (usize, usize) {
    let line = stderr
        .lines()
        .find_map(|l| l.trim().strip_prefix("built "))
        .unwrap_or_else(|| panic!("no `built N, cached M` summary in:\n{stderr}"));
    let (built, cached) = line.split_once(", cached ").expect("summary format");
    (
        built.trim().parse().unwrap(),
        cached.trim().parse().unwrap(),
    )
}

#[test]
#[ignore]
fn cargo_build_fixture_builds_then_caches() {
    let Some(()) = require_rustc_or_skip("cargo_build_fixture_builds_then_caches") else {
        return;
    };
    let (_tmp, project) = setup("tests/fixtures/cargo-build");

    let first = build_ok(&project);
    let (built, cached) = summary(&first);
    assert!(built > 0, "{first}");
    assert_eq!(cached, 0, "{first}");
    assert!(
        project
            .join("build/cross/x86_64-unknown-none/dev/final/kernel")
            .is_file(),
        "kernel binary missing"
    );

    let second = build_ok(&project);
    assert_eq!(summary(&second), (0, built), "{second}");
}

/// A feature change in one manifest rebuilds that package's units and
/// their dependents, and nothing else: the feature reaches the argv,
/// and the lib's new metadata hash changes the bin's `--extern`.
#[test]
#[ignore]
fn manifest_feature_edit_rebuilds_dependents() {
    let Some(()) = require_rustc_or_skip("manifest_feature_edit_rebuilds_dependents") else {
        return;
    };
    let (_tmp, project) = setup("tests/fixtures/cargo-build");
    build_ok(&project);

    let manifest = project.join("crates/kernel/Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let edited = text.replace("default = [\"log\"]", "default = []");
    assert_ne!(text, edited, "fixture manifest changed shape");
    fs::write(&manifest, edited).unwrap();

    let stderr = build_ok(&project);
    // kernel@lib and the kernel bin.
    assert_eq!(summary(&stderr).0, 2, "{stderr}");
}

#[test]
#[ignore]
fn examples_build() {
    let Some(()) = require_rustc_or_skip("examples_build") else {
        return;
    };
    for example in ["basic-kernel", "with-config", "uefi-with-bootloader"] {
        let (_tmp, project) = setup(&format!("examples/{example}"));
        let stderr = build_ok(&project);
        assert_eq!(summary(&stderr).1, 0, "{example}: {stderr}");
    }
}
