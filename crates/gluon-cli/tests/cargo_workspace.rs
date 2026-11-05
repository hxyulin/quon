//! End-to-end builds of projects whose crates come from a Cargo
//! workspace (`cfg.cargo_workspace` and `group.package`): the
//! `cargo-build` and `cargo-registry` fixtures and every example under
//! `examples/`.
//!
//! Each test copies its project into a temp dir and runs the real
//! `gluon` binary there. They spawn rustc and build sysroots, so they
//! are `#[ignore]`-gated like the other build tests in `integration.rs`.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

// ---------------------------------------------------------------------
// Registry dependencies (`tests/fixtures/cargo-registry`)
// ---------------------------------------------------------------------

/// Run `gluon <args>` in `project` with `env` set on the child only.
fn gluon(project: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gluon"));
    cmd.args(args).current_dir(project);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn gluon")
}

/// [`gluon`] that must succeed; returns stdout and stderr.
fn gluon_ok(project: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> (String, String) {
    let out = gluon(project, args, env);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "gluon {args:?} failed:\n{stdout}\n{stderr}"
    );
    (stdout, stderr)
}

/// `$CARGO` for a child that must not run Cargo: any spawn fails.
const NO_CARGO: &str = "/nonexistent/cargo";

/// Switch the fixture's gluon.rs to read registry sources from
/// `vendor/`.
fn use_vendor_dir(project: &Path) {
    let path = project.join("gluon.rs");
    let text = fs::read_to_string(&path).unwrap();
    let edited = text.replace(
        "cfg.cargo_workspace(\".\");",
        "cfg.cargo_workspace(\".\").vendored(\"vendor\");",
    );
    assert_ne!(text, edited, "fixture gluon.rs changed shape");
    fs::write(path, edited).unwrap();
}

fn append(path: &Path, text: &str) {
    let mut s = fs::read_to_string(path).unwrap();
    s.push_str(text);
    fs::write(path, s).unwrap();
}

/// Offline: a `.cargo/config.toml` in the project replaces crates.io
/// with the fixture's `registry/` directory source, so `cargo fetch` and
/// `cargo metadata` work without the network. (A Cargo home config
/// would also redirect the cargo build of `gluon-config` that compiles
/// `gluon.rs`.) Covers both source modes:
/// registry (Cargo asked once, then the cached map) and a vendor
/// directory (no Cargo at all, sources stat'ed, stamp checked).
#[test]
#[ignore]
fn registry_dependency_builds_from_cargo_sources_then_vendor_dir() {
    let Some(()) = require_rustc_or_skip("registry_dependency_builds_from_cargo_sources") else {
        return;
    };
    let (_tmp, project) = setup("tests/fixtures/cargo-registry");
    fs::create_dir_all(project.join(".cargo")).unwrap();
    fs::write(
        project.join(".cargo/config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n\
             [source.fixture]\ndirectory = '{}'\n",
            project.join("registry").display()
        ),
    )
    .unwrap();
    let without_cargo = [("CARGO", OsStr::new(NO_CARGO))];

    let (_, first) = gluon_ok(&project, &["build"], &[]);
    let (built, cached) = summary(&first);
    assert!(built >= 2, "{first}");
    assert_eq!(cached, 0, "{first}");
    // `--cap-lints allow` silences the registry package's warning.
    assert!(!first.contains("unused variable"), "{first}");
    let map = fs::read_to_string(project.join("build/cargo-sources.json")).unwrap();
    assert!(map.contains("fakeflags-1.0.0"), "{map}");

    // The cached source map answers without Cargo.
    let (_, second) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&second), (0, built), "{second}");

    // Vendor mode, with the directory `gluon vendor` would fill.
    copy_dir_all(
        &project.join("registry/fakeflags-1.0.0"),
        &project.join("vendor/fakeflags-1.0.0"),
    )
    .unwrap();
    gluon_core::manifest::sources::write_vendor_stamp(&project, &project.join("vendor")).unwrap();
    use_vendor_dir(&project);
    // The source path moved: fakeflags and the kernel rebuild.
    let (_, third) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&third).0, 2, "{third}");

    // Vendored sources are stat'ed: an edit rebuilds.
    append(
        &project.join("vendor/fakeflags-1.0.0/src/lib.rs"),
        "\npub const MORE: u32 = 1;\n",
    );
    let (_, fourth) = gluon_ok(&project, &["build"], &without_cargo);
    assert!(summary(&fourth).0 >= 1, "{fourth}");

    append(&project.join("Cargo.lock"), "\n");
    let out = gluon(&project, &["build"], &without_cargo);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("Cargo.lock changed since the last `gluon vendor`"),
        "{stderr}"
    );
}

/// True when crates.io's sparse index answers a TCP connect.
fn crates_io_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(mut addrs) = ("index.crates.io", 443).to_socket_addrs() else {
        return false;
    };
    addrs.any(|a| TcpStream::connect_timeout(&a, std::time::Duration::from_secs(3)).is_ok())
}

/// Network: the fixture with `fakeflags` swapped for crates.io
/// `bitflags`. Builds from Cargo's registry cache, then `gluon vendor`
/// fills `vendor/`, prints the line to add, and a vendor-mode build runs
/// no Cargo. Skips when crates.io is unreachable.
///
/// It uses the default Cargo home: another one would also change the
/// cargo build of `gluon-config` in the shared target directory, which
/// tests running in parallel link against.
#[test]
#[ignore]
fn registry_dependency_from_crates_io_and_gluon_vendor() {
    let name = "registry_dependency_from_crates_io_and_gluon_vendor";
    let Some(()) = require_rustc_or_skip(name) else {
        return;
    };
    if !crates_io_reachable() {
        eprintln!("{name}: skipped — crates.io unreachable");
        return;
    }
    let (_tmp, project) = setup("tests/fixtures/cargo-registry");
    let manifest = project.join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let edited = text.replace(
        "fakeflags = \"1\"",
        "fakeflags = { package = \"bitflags\", version = \"2\" }",
    );
    assert_ne!(text, edited, "fixture manifest changed shape");
    fs::write(&manifest, edited).unwrap();
    fs::remove_file(project.join("Cargo.lock")).unwrap();
    let without_cargo = [("CARGO", OsStr::new(NO_CARGO))];
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let lock = Command::new(cargo)
        .arg("generate-lockfile")
        .current_dir(&project)
        .output()
        .expect("spawn cargo");
    if !lock.status.success() {
        eprintln!(
            "{name}: skipped — cargo generate-lockfile failed:\n{}",
            String::from_utf8_lossy(&lock.stderr)
        );
        return;
    }

    let (_, first) = gluon_ok(&project, &["build"], &[]);
    let (built, _) = summary(&first);
    let map = fs::read_to_string(project.join("build/cargo-sources.json")).unwrap();
    assert!(map.contains("registry/src"), "{map}");
    let (_, second) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&second), (0, built), "{second}");

    let (stdout, _) = gluon_ok(&project, &["vendor"], &[]);
    assert!(
        stdout.contains("Vendored 1 packages into vendor/."),
        "{stdout}"
    );
    assert!(
        stdout.contains("cfg.cargo_workspace(\".\").vendored(\"vendor\");"),
        "{stdout}"
    );
    assert!(project.join("vendor/.gluon-vendor-stamp").is_file());
    let (stdout, _) = gluon_ok(&project, &["vendor", "--check"], &without_cargo);
    assert!(stdout.contains("matches Cargo.lock"), "{stdout}");

    use_vendor_dir(&project);
    let (_, third) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&third).0, 2, "{third}");
}
