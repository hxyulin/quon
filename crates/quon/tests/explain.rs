//! `quon explain copies` and the note on the `Finished` line. They
//! build, so they are `#[ignore]`d like the other build tests.

mod support;

use std::fs;
use std::path::Path;

/// Two run entries, `a` and `b`, whose binaries both reach `leaf`
/// through `mid`. `b` also depends on `leaf` directly with
/// `b_features`.
fn write_project(root: &Path, b_features: &str) {
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    let package = |name: &str, rest: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{rest}")
    };
    write(
        "Cargo.toml",
        "[workspace]\nresolver = \"3\"\nmembers = [\"leaf\", \"mid\", \"app-a\", \"app-b\"]\n",
    );
    write(
        "leaf/Cargo.toml",
        &package("leaf", "[features]\nextra = []\n"),
    );
    write("leaf/src/lib.rs", "");
    write(
        "mid/Cargo.toml",
        &package("mid", "[dependencies]\nleaf = { path = \"../leaf\" }\n"),
    );
    write("mid/src/lib.rs", "");
    write(
        "app-a/Cargo.toml",
        &package("app-a", "[dependencies]\nmid = { path = \"../mid\" }\n"),
    );
    write(
        "app-b/Cargo.toml",
        &package(
            "app-b",
            &format!(
                "[dependencies]\nmid = {{ path = \"../mid\" }}\n\
                 leaf = {{ path = \"../leaf\", features = [{b_features}] }}\n"
            ),
        ),
    );
    for app in ["app-a", "app-b"] {
        write(&format!("{app}/src/main.rs"), "fn main() {}\n");
    }
    write(
        "quon.rs",
        r#"use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("copies", "0.1.0");
    q.cargo_workspace(".");
    let a = q.add(Package::new("app-a"));
    q.run(&a).name("a");
    let b = q.add(Package::new("app-b"));
    q.run(&b).name("b");
    Ok(())
}
"#,
    );
}

/// One feature on `leaf` in one entry forks `leaf` and `mid`, which
/// depends on it. The report names both copies' entries and traces
/// `mid`'s copy to `leaf`'s feature; the build's summary counts them.
#[test]
#[ignore]
fn explain_copies_traces_a_forked_crate_to_the_feature_that_forked_it() {
    if !support::rustc_available(
        "explain_copies_traces_a_forked_crate_to_the_feature_that_forked_it",
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root, "\"extra\"");

    let out = support::quon(root)
        .args(["explain", "copies"])
        .output()
        .expect("spawn quon");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "leaf 0.1.0 (host): 2 copies\n  \
           1 entry: a\n  \
           1 entry: b\n    \
             leaf: features +extra\n\
         mid 0.1.0 (host): 2 copies\n  \
           1 entry: a\n  \
           1 entry: b\n    \
             leaf: features +extra\n"
    );

    let stderr = support::quon_ok(root, &["build"]);
    assert!(
        stderr.contains("2 crates have more than one copy (quon explain copies)"),
        "{stderr}"
    );

    // Without the feature, both entries share every crate.
    write_project(root, "");
    let out = support::quon(root)
        .args(["explain", "copies"])
        .output()
        .expect("spawn quon");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "every crate compiles once across 2 run entries\n"
    );
    let stderr = support::quon_ok(root, &["build"]);
    assert!(!stderr.contains("more than one copy"), "{stderr}");
}
