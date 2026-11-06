//! K9 — end-to-end test for the `.kconfig` parser, loader, and resolver.
//!
//! This is the round-trip parity check between the two equivalent
//! fixtures at:
//!
//! - `tests/fixtures/kconfig-minimal/` — declares its options through a
//!   `.kconfig` file loaded via `load_kconfig()`. Also exercises menus,
//!   the `source` directive, and a boolean expression in `depends_on`.
//! - `tests/fixtures/kconfig-builder-equiv/` — declares the same options
//!   through the inline `config_*` builders, omitting the
//!   features the legacy builder cannot express (`||`/`!`).
//!
//! For the AND-of-symbols subset both forms can express, every option's
//! resolved value should match. The test loads both fixtures, runs the
//! resolver, and asserts equality option-by-option.
//!
//! # Why this lives in `quon-core/tests/`
//!
//! It's a pure in-process test of the parse → load → lower → resolve
//! pipeline. It does not spawn the `quon` binary. Stage 2: the
//! fixture-driven tests now call `load_build_model`, which prefers
//! `quon.rs` and therefore requires rustc — they are `#[ignore]`-gated
//! with a probe-or-skip guard. The inline builder test
//! (`depends_on_expr_honours_or_semantics`) constructs the model in-
//! process via `Config` and stays ungated. The CLI integration tests in
//! `quon/tests/integration.rs` are reserved for behavior that
//! actually shells out to the binary; this test would be misleading
//! there because nothing about the CLI is being exercised.

use quon_core::model::ResolvedValue;
use std::path::{Path, PathBuf};

/// Copy `tests/fixtures/<name>` into a fresh temp dir and return the
/// temp dir and the copy. Loading a model compiles and runs the
/// fixture's `quon.rs` under `<project>/build`, so a copy keeps
/// `build/` out of the source tree and keeps tests that load the same
/// fixture from linking one script binary at the same time.
fn fixture_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    let tmp = tempfile::tempdir().expect("tempdir");
    let dst = tmp.path().join(name);
    copy_dir(&src, &dst);
    (tmp, dst)
}

/// Recursive copy that skips `build/`.
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dir");
    for entry in std::fs::read_dir(src).expect("read fixture") {
        let entry = entry.expect("dir entry");
        let to = dst.join(entry.file_name());
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() && entry.file_name() != "build" {
            copy_dir(&entry.path(), &to);
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to).expect("copy file");
        }
    }
}

#[test]
#[ignore]
fn kconfig_loader_round_trips_against_builder() {
    if quon_core::RustcInfo::probe().is_err() {
        eprintln!("kconfig_loader_round_trips_against_builder: skipped — rustc probe failed");
        return;
    }
    let (_kconfig_tmp, kconfig_dir) = fixture_copy("kconfig-minimal");
    let (_builder_tmp, builder_dir) = fixture_copy("kconfig-builder-equiv");
    assert!(
        kconfig_dir.join("quon.rs").is_file(),
        "fixture missing at {kconfig_dir:?}"
    );
    assert!(
        builder_dir.join("quon.rs").is_file(),
        "fixture missing at {builder_dir:?}"
    );

    // Load both fixture models. Stage 2: load_build_model prefers .rs
    // over the legacy format — the kconfig fixture's load_kconfig() call
    // is wired through the .rs harness.
    let kconfig_model = quon_core::load_build_model(&kconfig_dir).expect("load kconfig fixture");
    let builder_model = quon_core::load_build_model(&builder_dir).expect("load builder fixture");

    // Resolve both with the default profile and no overrides.
    let kconfig_resolved =
        quon_core::resolve_config(&kconfig_model, "default", None, &kconfig_dir, None)
            .expect("resolve kconfig fixture");
    let builder_resolved =
        quon_core::resolve_config(&builder_model, "default", None, &builder_dir, None)
            .expect("resolve builder fixture");

    // For every option declared in the builder equivalent, the kconfig
    // resolved value should match. The kconfig form may declare
    // additional options (e.g. FANCY_FEATURE) that the builder form
    // cannot express, so we iterate over the builder keys and treat that
    // as the parity baseline.
    for (name, builder_value) in &builder_resolved.options {
        let kconfig_value = kconfig_resolved
            .options
            .get(name)
            .unwrap_or_else(|| panic!("kconfig fixture is missing option '{name}'"));
        assert_eq!(
            kconfig_value, builder_value,
            "option '{name}' resolved differently:\n  kconfig: {kconfig_value:?}\n  builder: {builder_value:?}"
        );
    }
}

#[test]
#[ignore]
fn kconfig_loader_resolves_boolean_expression_form() {
    if quon_core::RustcInfo::probe().is_err() {
        eprintln!("kconfig_loader_resolves_boolean_expression_form: skipped — rustc probe failed");
        return;
    }
    // FANCY_FEATURE is declared in extras.kconfig with
    //     depends_on = LOG_ENABLED && !DEBUG
    // The defaults are LOG_ENABLED=true, DEBUG=false → expression is
    // true → FANCY_FEATURE keeps its `default = true`. This is the
    // behavior that distinguishes true semantic evaluation from
    // hadron-style `flatten_symbols()`: the flat form would treat the
    // referenced symbols as an implicit AND of two requirements
    // (`LOG_ENABLED` AND `DEBUG`), wrongly disabling FANCY_FEATURE.
    let (_tmp, kconfig_dir) = fixture_copy("kconfig-minimal");
    let model = quon_core::load_build_model(&kconfig_dir).expect("load kconfig fixture");
    let resolved =
        quon_core::resolve_config(&model, "default", None, &kconfig_dir, None).expect("resolve");
    // Pin the intermediate values explicitly so that editing the
    // fixture defaults flips this test loudly instead of letting the
    // expression silently become vacuous (see the comment on
    // FANCY_FEATURE in `extras.kconfig`).
    assert_eq!(
        resolved.options.get("LOG_ENABLED"),
        Some(&ResolvedValue::Bool(true)),
        "fixture invariant: LOG_ENABLED defaults to true"
    );
    assert_eq!(
        resolved.options.get("DEBUG"),
        Some(&ResolvedValue::Bool(false)),
        "fixture invariant: DEBUG defaults to false"
    );
    // FANCY_FEATURE only exists in extras.kconfig, which
    // options.kconfig pulls in via `source "./extras.kconfig"`, so this
    // also proves the loader follows source directives.
    assert_eq!(
        resolved.options.get("FANCY_FEATURE"),
        Some(&ResolvedValue::Bool(true)),
        "FANCY_FEATURE should be enabled because LOG_ENABLED && !DEBUG holds at default"
    );
}

#[test]
fn depends_on_expr_honours_or_semantics() {
    // Defaults: A=true, B=false. The expression `A || B` is true
    // under the Expr evaluation path (reuses the kconfig grammar)
    // so DEP keeps its default of true. If the builder were still
    // routing this through the legacy flat `Vec<String>` AND-of-
    // names path, it would treat `A` and `B` as two separate
    // required symbols, see `B` is off, and force DEP to false.
    use quon_config::Config;
    use quon_model::{ConfigValue, Expr, intern};

    let mut cfg = Config::new();
    cfg.project("expr-parity", "0.1.0");
    let t = cfg.target("x86_64-unknown-none");
    cfg.profile("default").target(t);
    cfg.config_bool("A").default_value(ConfigValue::Bool(true));
    cfg.config_bool("B").default_value(ConfigValue::Bool(false));
    cfg.config_bool("DEP")
        .default_value(ConfigValue::Bool(true))
        .depends_on(Expr::Or(vec![
            Expr::Ident("A".into()),
            Expr::Ident("B".into()),
        ]));

    let raw = cfg.into_raw();
    let (model, diags) = intern(raw);
    assert!(diags.is_empty(), "intern diags: {diags:?}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let resolved =
        quon_core::resolve_config(&model, "default", None, tmp.path(), None).expect("resolve");

    assert_eq!(
        resolved.options.get("A"),
        Some(&ResolvedValue::Bool(true)),
        "sanity: A defaults to true"
    );
    assert_eq!(
        resolved.options.get("B"),
        Some(&ResolvedValue::Bool(false)),
        "sanity: B defaults to false"
    );
    assert_eq!(
        resolved.options.get("DEP"),
        Some(&ResolvedValue::Bool(true)),
        "DEP should stay on because A || B holds (A is on)"
    );
}
