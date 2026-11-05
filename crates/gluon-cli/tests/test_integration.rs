//! End-to-end CLI tests for `gluon test` against the `cargo-tests`
//! fixture. `--dry-run` and `--no-build` skip the build, and a script
//! stands in for QEMU, so these need neither a bare-metal toolchain nor
//! QEMU. Stub OVMF files satisfy the UEFI firmware lookup.

#![cfg(unix)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn setup_fixture() -> (TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/cargo-tests")
        .canonicalize()
        .expect("fixture exists");
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dst = tmp.path().join("cargo-tests");
    copy_dir_all(&src, &dst).expect("copy fixture into tempdir");
    fs::write(dst.join("OVMF_CODE.fd"), b"fake code").unwrap();
    fs::write(dst.join("OVMF_VARS.fd"), b"fake vars").unwrap();
    (tmp, dst)
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == "build" {
            continue;
        }
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn gluon(fixture: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gluon"))
        .current_dir(fixture)
        .env("OVMF_CODE", fixture.join("OVMF_CODE.fd"))
        .env("OVMF_VARS", fixture.join("OVMF_VARS.fd"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn gluon")
}

/// Point the fixture's QEMU at a script that exits 1 when its argv
/// names the `fails` test and 33, the success status, otherwise.
fn use_fake_qemu(fixture: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = fixture.join("fake-qemu");
    fs::write(
        &script,
        "#!/bin/sh\ncase \"$*\" in *fails*) exit 1 ;; esac\nexit 33\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let rs = fixture.join("gluon.rs");
    let text = fs::read_to_string(&rs).unwrap();
    fs::write(
        &rs,
        text.replace("qemu-system-x86_64", script.to_str().unwrap()),
    )
    .unwrap();
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn dry_run_boots_each_test_from_its_own_esp() {
    let (_tmp, fixture) = setup_fixture();
    let out = gluon(&fixture, &["test", "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "one command per test: {stdout}");
    assert!(lines[0].contains("esp/default.tests/boot"), "{}", lines[0]);
    assert!(lines[1].contains("esp/default.tests/fails"), "{}", lines[1]);
}

#[test]
fn reports_each_test_and_fails_when_one_fails() {
    let (_tmp, fixture) = setup_fixture();
    use_fake_qemu(&fixture);
    let out = gluon(&fixture, &["test", "--no-build"]);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("test boot ... ok"), "{err}");
    assert!(
        err.contains("test fails ... FAILED (QEMU exited with 1)"),
        "{err}"
    );
    assert!(err.contains("1 of 2 tests failed, 1 passed"), "{err}");
}

#[test]
fn filter_selects_tests_by_name() {
    let (_tmp, fixture) = setup_fixture();
    use_fake_qemu(&fixture);
    let out = gluon(&fixture, &["test", "--no-build", "boot"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("test boot ... ok"), "{err}");
    assert!(!err.contains("test fails"), "{err}");
    assert!(err.contains("test result: ok. 1 passed"), "{err}");
}

#[test]
fn direct_boot_passes_the_test_binary_as_the_kernel() {
    let (_tmp, fixture) = setup_fixture();
    let out = gluon(&fixture, &["test", "--dry-run", "--direct", "boot"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let args: Vec<&str> = stdout.split_whitespace().collect();
    let kernel = args
        .windows(2)
        .find(|w| w[0] == "-kernel")
        .map(|w| w[1])
        .expect("-kernel in direct mode");
    assert!(kernel.ends_with("/final/boot"), "{kernel}");
}

#[test]
fn gluon_run_ignores_the_tests() {
    let (_tmp, fixture) = setup_fixture();
    let out = gluon(&fixture, &["run", "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("esp/default"), "{stdout}");
    assert!(!stdout.contains("default.tests"), "{stdout}");
}
