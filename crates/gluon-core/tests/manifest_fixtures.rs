//! Fixture tests for Cargo manifest loading.
//!
//! Each fixture under `tests/fixtures/cargo-*` holds a Cargo workspace
//! and `expected/*.txt` files. Set `GLUON_UPDATE_EXPECT=1` to rewrite
//! the expected files from the current output, then review the diff.

use gluon_core::manifest::sources::RegistrySources;
use gluon_core::manifest::{dump_packages, workspace::load_workspace};
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn vendored(dir: &Path, vendor: &str) -> RegistrySources {
    RegistrySources::vendor_dir(dir, &dir.join(vendor)).expect("read Cargo.lock")
}

fn check_expected(path: &Path, actual: &str) {
    if std::env::var_os("GLUON_UPDATE_EXPECT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e} (set GLUON_UPDATE_EXPECT=1)", path.display()));
    assert_eq!(
        actual,
        expected,
        "output differs from {} (set GLUON_UPDATE_EXPECT=1 to update)",
        path.display()
    );
}

fn packages_fixture(name: &str) {
    let dir = fixtures().join(name);
    let set = load_workspace(&dir, &vendored(&dir, "vendor")).expect("load");
    check_expected(&dir.join("expected/packages.txt"), &dump_packages(&set));
}

#[test]
fn cargo_inherit_packages() {
    packages_fixture("cargo-inherit");
}

#[test]
fn cargo_inherit_warnings() {
    let dir = fixtures().join("cargo-inherit");
    let set = load_workspace(&dir, &vendored(&dir, "vendor")).expect("load");
    let msgs: Vec<String> = set.warnings.iter().map(|w| w.message.clone()).collect();
    assert_eq!(msgs.len(), 2, "{msgs:#?}");
    assert!(
        msgs.iter()
            .any(|m| m.contains("`default-features = false` is ignored"))
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("`[profile.*]` tables are ignored"))
    );
}

#[test]
fn cargo_rename_packages() {
    packages_fixture("cargo-rename");
}

/// Missing vendor sources and a stale lock do not fail the load; the
/// edges are recorded as unresolved for the resolver to report.
#[test]
fn unresolved_registry_edges_are_recorded() {
    let dir = fixtures().join("cargo-rename");
    let set = load_workspace(&dir, &vendored(&dir, "no-such-vendor")).expect("load");
    let out = dump_packages(&set);
    assert!(
        out.contains("dep normal fake1 -> fake ^1 unresolved: source of `fake 1.0.0` is missing"),
        "{out}"
    );
}

/// Every `cargo-errors/<class>` fixture fails to load with the
/// diagnostics in its `expected.txt`.
#[test]
fn cargo_error_fixtures() {
    let root = fixtures().join("cargo-errors");
    let mut classes: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    classes.sort();
    assert!(!classes.is_empty());
    for dir in classes {
        let err = match load_workspace(&dir, &vendored(&dir, "vendor")) {
            Ok(_) => panic!("{} loaded without error", dir.display()),
            Err(e) => e.to_string(),
        };
        let canonical = std::fs::canonicalize(&dir).unwrap();
        let text = err.replace(&format!("{}/", canonical.display()), "") + "\n";
        check_expected(&dir.join("expected.txt"), &text);
    }
}
