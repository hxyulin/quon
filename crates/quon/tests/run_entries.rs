//! End-to-end CLI tests for several named run entries, against the
//! `run-entries` fixture: two host boards whose chip features of one
//! library exclude each other, so they build only if features resolve
//! per entry, and a `common` library both use the same way.

mod support;

use std::fs;
use std::path::Path;
use std::process::Output;

fn quon_out(fixture: &Path, args: &[&str]) -> Output {
    support::quon(fixture)
        .args(args)
        .env_remove("QUON_RUN")
        .output()
        .expect("spawn quon")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
#[ignore]
fn each_entry_resolves_its_own_features() {
    if !support::rustc_available("each_entry_resolves_its_own_features") {
        return;
    }
    let (_tmp, fixture) = support::setup_fixture("run-entries");

    // Step 1: `quon build` builds every entry in one schedule. `chip`
    // compiles once per feature set, `common`, which both boards use
    // the same way, once. A second build finds everything cached.
    let out = quon_out(&fixture, &["build"]);
    let err = stderr(&out);
    assert!(out.status.success(), "step 1: build failed:\n{err}");
    assert!(
        err.contains("Building 2 run entries"),
        "step 1: no entries line:\n{err}"
    );
    let compiles = |name: &str| {
        err.lines()
            .filter(|l| l.trim_start().starts_with(&format!("Compiling {name} ")))
            .count()
    };
    assert_eq!(compiles("chip"), 2, "step 1:\n{err}");
    assert_eq!(compiles("common"), 1, "step 1:\n{err}");
    let (built, cached) = support::summary(&err);
    assert_eq!(cached, 0, "step 1:\n{err}");
    let out = quon_out(&fixture, &["build"]);
    let again = stderr(&out);
    assert_eq!(
        support::summary(&again),
        (0, built),
        "step 1 (again):\n{again}"
    );

    // Step 2: `quon run <entry>` starts that entry's binary.
    for (entry, expected) in [("a", "chip a"), ("b", "chip b")] {
        let out = quon_out(&fixture, &["run", entry]);
        assert!(
            out.status.success(),
            "step 2: run {entry}:\n{}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), expected, "step 2: run {entry}");
    }

    // Step 3: without a name, `quon run` lists the entries.
    let out = quon_out(&fixture, &["run"]);
    assert!(!out.status.success(), "step 3: run without a name passed");
    assert!(
        stderr(&out).contains("quon.rs declares 2 run entries (a, b)"),
        "step 3:\n{}",
        stderr(&out)
    );

    // Step 4: an unknown name names the entries.
    let out = quon_out(&fixture, &["build", "c"]);
    assert!(!out.status.success(), "step 4: build c passed");
    assert!(
        stderr(&out).contains("no run entry named 'c'; the entries are a, b"),
        "step 4:\n{}",
        stderr(&out)
    );

    // Step 5: `RUN` in .quon-config picks the default entry, and
    // QUON_RUN overrides it.
    fs::write(fixture.join(".quon-config"), "RUN = \"b\"\n").expect("write .quon-config");
    let out = quon_out(&fixture, &["run"]);
    assert_eq!(stdout(&out), "chip b", "step 5:\n{}", stderr(&out));
    let out = support::quon(&fixture)
        .args(["run"])
        .env("QUON_RUN", "a")
        .output()
        .expect("spawn quon");
    assert_eq!(stdout(&out), "chip a", "step 5 (env):\n{}", stderr(&out));
    fs::remove_file(fixture.join(".quon-config")).expect("remove .quon-config");

    // Step 6: without a name, `quon test` runs the host tests alone.
    let out = quon_out(&fixture, &["test"]);
    let err = stderr(&out);
    assert!(out.status.success(), "step 6: test failed:\n{err}");
    assert!(
        err.contains("only the host tests run"),
        "step 6: no note:\n{err}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("has_a_name ... ok"),
        "step 6: the host test did not run:\n{err}"
    );
}
