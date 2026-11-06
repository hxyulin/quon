//! Helpers shared by the `quon` integration tests. Each test file
//! that uses them declares `mod support;`, and not every file uses
//! every helper.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// `<repo>/tests/fixtures/<name>`.
pub fn fixture_dir(name: &str) -> PathBuf {
    repo_root().join("tests/fixtures").join(name)
}

/// Copy `src` (a path relative to the repository root) into a fresh
/// temp dir and return the temp dir and the copy, which keeps the
/// source's directory name. The temp dir must outlive the test.
pub fn setup(src: &str) -> (TempDir, PathBuf) {
    let src = repo_root().join(src);
    assert!(src.is_dir(), "no project at {src:?}");
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dst = tmp.path().join(src.file_name().expect("project dir name"));
    copy_dir_all(&src, &dst).expect("copy project into tempdir");
    (tmp, dst)
}

/// [`setup`] for `tests/fixtures/<name>`.
pub fn setup_fixture(name: &str) -> (TempDir, PathBuf) {
    setup(&format!("tests/fixtures/{name}"))
}

/// Recursive copy that skips build outputs (`build/`, `esp/`,
/// `target/`). A developer who ran a build inside a fixture leaves
/// those behind, and copying them makes a test depend on that state.
pub fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        if ty.is_dir() && (name == "build" || name == "esp" || name == "target") {
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

/// Whether rustc with `rust-src` is available. Prints why a test skips
/// when it is not; use as `if !rustc_available("name") { return; }`.
pub fn rustc_available(test_name: &str) -> bool {
    match quon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => true,
        Ok(_) => {
            eprintln!("{test_name}: skipped, rust-src component not installed");
            false
        }
        Err(e) => {
            eprintln!("{test_name}: skipped, rustc probe failed: {e}");
            false
        }
    }
}

/// Copy a prebuilt sysroot for each of `project`'s builtin targets into
/// `project/build`, so the test's first build does not spend 15 s
/// building `core`, `compiler_builtins` and `alloc`. See
/// `quon_core::testing`. Tests about the sysroot itself skip this.
/// Returns how many sysroots it seeded, which the first build counts
/// as cached.
pub fn seed_sysroot(project: &Path) -> usize {
    quon_core::testing::seed_project(project, None)
        .unwrap_or_else(|e| panic!("seeding the sysroot for {project:?}: {e}"))
}

/// [`seed_sysroot`] for a test that runs `quon` with `RUSTC=rustc`.
pub fn seed_sysroot_for(project: &Path, rustc: &Path) -> usize {
    quon_core::testing::seed_project(project, Some(rustc.as_os_str()))
        .unwrap_or_else(|e| panic!("seeding the sysroot for {project:?}: {e}"))
}

/// The `quon` binary run in `project`, with stdout and stderr piped.
pub fn quon(project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_quon"));
    cmd.current_dir(project);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Run `quon <args>` in `project`; panic on failure, return stderr.
pub fn quon_ok(project: &Path, args: &[&str]) -> String {
    let out = quon(project).args(args).output().expect("spawn quon");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "quon {args:?} failed:\n{stderr}");
    stderr
}

/// `(done, cached)` from the `(<noun> N, cached M)` counts that end the
/// `Finished` line, where `noun` is `built`, `checked` or `linted`.
pub fn counts(stderr: &str, noun: &str) -> (usize, usize) {
    let open = format!("({noun} ");
    let rest = stderr
        .lines()
        .find_map(|l| l.split_once(open.as_str()).map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("no `({noun} N, cached M)` summary in:\n{stderr}"));
    let (done, cached) = rest.split_once(", cached ").expect("summary format");
    (
        done.trim().parse().expect("count"),
        cached.trim_end_matches(')').trim().parse().expect("count"),
    )
}

/// `(built, cached)` of a `quon build`.
pub fn summary(stderr: &str) -> (usize, usize) {
    counts(stderr, "built")
}
