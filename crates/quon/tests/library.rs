//! The library API: [`quon::run`] and [`quon::Project`], called
//! in-process the way a repository's `xtask` calls them. The tests that
//! build spawn rustc, so they are `#[ignore]`d like the other build
//! tests.

mod support;

use quon::{ExitCode, Project};
use std::path::Path;

#[test]
fn a_bad_argument_is_a_usage_error_and_the_caller_keeps_running() {
    let code = quon::run(Path::new("."), ["--no-such-flag"]);
    assert_eq!(code, ExitCode::from(2));
}

#[test]
fn help_succeeds() {
    assert_eq!(quon::run(Path::new("."), ["--help"]), ExitCode::SUCCESS);
}

/// `run` searches for `quon.rs` from the directory it is given, not the
/// test's own current directory (inside this repository, which has a
/// `quon.rs` of its own). `Project` lists the entries and names the
/// files the build wrote.
#[test]
#[ignore]
fn run_builds_from_its_directory_and_project_names_the_artifacts() {
    if !support::rustc_available("run_builds_from_its_directory_and_project_names_the_artifacts") {
        return;
    }
    let (_tmp, fixture) = support::setup_fixture("run-entries");
    assert_eq!(quon::run(&fixture, ["build"]), ExitCode::SUCCESS);

    let project = Project::load(&fixture.join("board-a")).expect("load");
    assert_eq!(project.root(), fixture);
    let names: Vec<_> = project
        .entries()
        .iter()
        .map(|e| e.name.as_deref())
        .collect();
    assert_eq!(names, [Some("a"), Some("b")]);
    let a = project.entry("a").expect("entry a");
    assert_eq!(
        (a.package.as_str(), a.binary.as_str()),
        ("board-a", "board-a")
    );
    let meta: Vec<_> = a
        .meta
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(meta, [("chip", "chip-a")]);
    assert!(project.entry("b").expect("entry b").meta.is_empty());
    for entry in project.entries() {
        let path = project.artifact(entry, "dev").expect("artifact");
        assert!(
            path.starts_with(fixture.join("build")),
            "{}",
            path.display()
        );
        assert!(path.is_file(), "{} was not built", path.display());
    }
    // A missing profile is an error, not a panic.
    let err = project.artifact(a, "no-such-profile").unwrap_err();
    assert!(err.to_string().contains("no-such-profile"), "{err}");
}

/// `quon run` traps SIGINT while its runner is up. Afterwards the
/// signal must end the host process again, as it did before. The check
/// runs in a child copy of this test binary, since the signal ends it.
#[cfg(unix)]
#[test]
#[ignore]
fn sigint_ends_the_caller_again_after_quon_run() {
    use std::os::unix::process::ExitStatusExt;

    const CHILD: &str = "QUON_LIBRARY_SIGINT_FIXTURE";
    if let Some(fixture) = std::env::var_os(CHILD) {
        assert_eq!(quon::run(Path::new(&fixture), ["run"]), ExitCode::SUCCESS);
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(5));
        println!("survived SIGINT");
        return;
    }
    if !support::rustc_available("sigint_ends_the_caller_again_after_quon_run") {
        return;
    }
    let (_tmp, fixture) = support::setup_fixture("host-tool");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sigint_ends_the_caller_again_after_quon_run",
            "--include-ignored",
            "--nocapture",
        ])
        .env(CHILD, &fixture)
        .output()
        .expect("spawn the child test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello"),
        "quon run did not run hello: {stdout}"
    );
    assert_eq!(
        out.status.signal(),
        Some(signal_hook::consts::SIGINT),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Write a Cargo workspace whose `quon.rs` declares a run entry for each
/// member that depends on `bsp`, from `members()` of the workspace in
/// `workspace_dir`. `boards` are the member directories under `boards/`.
fn write_members_project(root: &Path, workspace_dir: &str, boards: &[&str]) {
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "Cargo.toml",
        "[workspace]\nresolver = \"3\"\nmembers = [\"bsp\", \"boards/*\", \"tool\"]\n",
    );
    write(
        "bsp/Cargo.toml",
        "[package]\nname = \"bsp\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write("bsp/src/lib.rs", "");
    write(
        "tool/Cargo.toml",
        "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write("tool/src/main.rs", "fn main() {}\n");
    for board in boards {
        write(
            &format!("boards/{board}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{board}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nbsp = {{ path = \"../../bsp\" }}\n"
            ),
        );
        write(&format!("boards/{board}/src/main.rs"), "fn main() {}\n");
    }
    write(
        "quon.rs",
        &format!(
            r#"use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {{
    q.project("members", "0.1.0");
    for member in q.cargo_workspace("{workspace_dir}").members() {{
        if member.depends_on("bsp") {{
            let firmware = q.add(Package::new(&member.name));
            q.run(&firmware).name(&member.name).meta("path", &member.path);
        }}
    }}
    Ok(())
}}
"#
        ),
    );
}

/// The compiled script binaries quon keeps for the project at `root`.
fn script_binaries(root: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir(root.join("build/script-runner/scripts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    dirs.sort();
    dirs
}

/// quon answers `members()` from the manifests while `quon.rs` runs, and
/// runs the script on every load, so a package added to the workspace
/// becomes an entry on the next load without recompiling the script.
#[test]
#[ignore]
fn quon_rs_declares_an_entry_per_member_and_sees_a_new_member_on_the_next_load() {
    if !support::rustc_available(
        "quon_rs_declares_an_entry_per_member_and_sees_a_new_member_on_the_next_load",
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    write_members_project(&root, ".", &["board-a"]);
    let entries = |project: &Project| -> Vec<(String, String)> {
        project
            .entries()
            .iter()
            .map(|e| (e.name.clone().unwrap(), e.meta["path"].clone()))
            .collect()
    };

    let project = Project::load(&root).expect("first load");
    assert_eq!(
        entries(&project),
        [("board-a".into(), "boards/board-a".into())]
    );
    let compiled = script_binaries(&root);

    write_members_project(&root, ".", &["board-a", "board-b"]);
    let project = Project::load(&root).expect("second load");
    assert_eq!(
        entries(&project),
        [
            ("board-a".into(), "boards/board-a".into()),
            ("board-b".into(), "boards/board-b".into()),
        ]
    );
    assert_eq!(script_binaries(&root), compiled, "quon.rs was recompiled");
}

/// A workspace quon cannot read fails the load with the call that asked
/// for it, instead of giving the script an empty list.
#[test]
#[ignore]
fn members_of_a_directory_without_a_workspace_is_a_config_error_that_names_the_call() {
    if !support::rustc_available(
        "members_of_a_directory_without_a_workspace_is_a_config_error_that_names_the_call",
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    write_members_project(tmp.path(), "missing", &["board-a"]);
    let err = Project::load(tmp.path()).unwrap_err().to_string();
    assert!(
        err.contains("cargo_workspace(\"missing\").members()"),
        "{err}"
    );
}
