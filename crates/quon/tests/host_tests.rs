//! End-to-end CLI tests for `quon test`'s host tests against the
//! `host-tests` fixture. They compile and run real test executables, so
//! they are `#[ignore]`d like the other build tests.

mod support;

use std::path::Path;

/// Whether any file under `dir` has `needle` in its name.
fn has_file_named(dir: &Path, needle: &str) -> bool {
    std::fs::read_dir(dir).unwrap().any(|e| {
        let e = e.unwrap();
        e.file_name().to_string_lossy().contains(needle)
            || (e.file_type().unwrap().is_dir() && has_file_named(&e.path(), needle))
    })
}

fn run(fixture: &Path, args: &[&str], fail: bool) -> (bool, String, String) {
    let mut cmd = support::quon(fixture);
    cmd.args(args);
    if fail {
        cmd.env("CALC_FAIL", "1");
    } else {
        cmd.env_remove("CALC_FAIL");
    }
    let out = cmd.output().expect("spawn quon");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[ignore]
fn host_tests_build_and_run_on_the_host() {
    if !support::rustc_available("host_tests_build_and_run_on_the_host") {
        return;
    }
    let (_tmp, fixture) = support::setup_fixture("host-tests");
    support::seed_sysroot(&fixture);

    // `quon build` builds none of the test executables or the
    // dev-dependency.
    support::quon_ok(&fixture, &["build"]);
    let build = fixture.join("build");
    assert!(!has_file_named(&build, "helper"), "quon build built helper");

    let (ok, stdout, stderr) = run(&fixture, &["test"], false);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("Running host test calc (src/lib.rs)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Running host test calc (tests/api.rs)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Running host test calc (doctests)"),
        "{stderr}"
    );
    for t in [
        "tests::adds",
        "tests::runs_in_the_package_directory",
        "api_adds",
        // Named relative to the package directory.
        "src/lib.rs - add (line 6)",
    ] {
        assert!(stdout.contains(&format!("test {t} ... ok")), "{stdout}");
    }
    assert!(
        stderr.contains("test result: ok. passed: host 3 of 3"),
        "{stderr}"
    );

    // The filter goes to libtest.
    let (ok, stdout, stderr) = run(&fixture, &["test", "adds"], false);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("test tests::adds ... ok"), "{stdout}");
    assert!(
        !stdout.contains("runs_in_the_package_directory"),
        "{stdout}"
    );

    // A failing executable fails the run, after the others ran. The
    // unit tests and the doctest both check CALC_FAIL, and the doctests
    // run from the command the earlier build stored.
    let (ok, stdout, stderr) = run(&fixture, &["test", "--no-build"], true);
    assert!(!ok, "{stderr}");
    assert!(stdout.contains("test api_adds ... ok"), "{stdout}");
    assert!(stderr.contains("host test calc (src/lib.rs)"), "{stderr}");
    assert!(stderr.contains("host test calc (doctests)"), "{stderr}");
    assert!(
        stderr.contains("2 of 3 test runs failed (passed: host 1 of 3)"),
        "{stderr}"
    );
}

#[test]
fn dry_run_prints_each_host_test() {
    let (_tmp, fixture) = support::setup_fixture("host-tests");
    let (ok, stdout, stderr) = run(&fixture, &["test", "--dry-run", "adds"], false);
    assert!(ok, "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(
        lines[0].contains("/host/profile/dev/deps/calc-"),
        "{}",
        lines[0]
    );
    assert!(lines[0].ends_with(" adds"), "{}", lines[0]);
    assert!(
        lines[1].contains("/host/profile/dev/deps/api-"),
        "{}",
        lines[1]
    );
    // Nothing is built, so the doctest command is not known yet.
    assert!(
        lines[2].starts_with("# calc (doctests): ") && lines[2].ends_with(".doctest.json"),
        "{}",
        lines[2]
    );
}
