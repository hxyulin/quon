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
