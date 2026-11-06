use super::*;
use crate::compile::{ArtifactMap, BuildLayout, CompileCtx, CompileUnitInput, DriverKind};
use crate::manifest::platform::TargetPlatform;
use quon_config::{Package, Quon, Target};
use quon_model::{BuildModel, ResolvedConfig};
use std::fmt::Write as _;
use std::sync::Arc;

fn fixture() -> PathBuf {
    std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/cargo-build"),
    )
    .expect("fixture exists")
}

/// Fixed cfg sets, so no test here spawns rustc.
fn platforms() -> BTreeMap<Domain, TargetPlatform> {
    let host = TargetPlatform::from_cfg_lines(
        "x86_64-unknown-linux-gnu",
        ["unix", "target_os=\"linux\"", "target_arch=\"x86_64\""],
    )
    .unwrap();
    let none = TargetPlatform::from_cfg_lines(
        "x86_64-unknown-none",
        ["target_os=\"none\"", "target_arch=\"x86_64\""],
    )
    .unwrap();
    BTreeMap::from([
        (Domain::Host, host),
        (Domain::Target("x86_64-unknown-none".into()), none),
    ])
}

/// A raw model for `root` with one target, `x86_64-unknown-none`,
/// which is also the default target.
fn raw_model(build: impl FnOnce(&mut Quon, &Target)) -> RawBuildModel {
    let mut q = Quon::new();
    q.project("demo", "0.1.0");
    let t = Target::builtin("x86_64-unknown-none");
    q.default_target(&t);
    build(&mut q, &t);
    q.into_raw().expect("quon errors")
}

/// The fixture's own quon.rs, in-process.
fn fixture_raw() -> RawBuildModel {
    raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(
            Package::new("kernel")
                .target(t)
                .link_config()
                .linker_script("crates/kernel/kernel.ld"),
        );
    })
}

fn expand_at(raw: &mut RawBuildModel, root: &Path) -> Result<Vec<Diagnostic>> {
    let Some(set) = load(raw, root, None)? else {
        return Ok(Vec::new());
    };
    expand_with(raw, root, set, &platforms(), "x86_64-unknown-linux-gnu")
}

fn expand_err(raw: &mut RawBuildModel, root: &Path) -> String {
    match expand_at(raw, root) {
        Ok(_) => panic!("expansion succeeded"),
        Err(e) => e.to_string(),
    }
}

/// One line per crate: name, group, target, crate type, root source,
/// edition, features, deps, rustc name.
fn dump(raw: &RawBuildModel) -> String {
    let mut out = String::new();
    for (_, c) in raw.crates.iter() {
        let deps: Vec<String> = c
            .deps
            .iter()
            .map(|(k, d)| format!("{k}->{}", d.crate_name))
            .collect();
        let rustc_name = c.cargo.as_ref().map(|c| c.rustc_name.as_str());
        let _ = writeln!(
            out,
            "{} [{}] {} {:?} {}/{} {} f=[{}] d=[{}] rustc={}",
            c.name,
            c.group,
            c.target,
            c.crate_type,
            c.path,
            c.root.as_deref().unwrap_or("-"),
            c.edition,
            c.features.join(","),
            deps.join(","),
            rustc_name.unwrap_or("-"),
        );
    }
    out
}

#[test]
fn fixture_expands_into_units() {
    let mut raw = fixture_raw();
    expand_at(&mut raw, &fixture()).expect("expand");
    assert_eq!(
        dump(&raw),
        "\
fixture-derive [cargo:host] host ProcMacro crates/derive/src/lib.rs 2021 f=[] d=[util->util@host] rustc=fixture_derive
kernel@lib [kernel] x86_64-unknown-none Lib crates/kernel/src/lib.rs 2021 f=[default,log] d=[fixture_derive->fixture-derive,helpers->util-helpers,util->util] rustc=kernel
kernel [kernel] x86_64-unknown-none Bin crates/kernel/src/main.rs 2021 f=[default,log] d=[fixture_derive->fixture-derive,helpers->util-helpers,kernel->kernel@lib,util->util] rustc=kernel
util@host [cargo:host] host Lib crates/util/src/lib.rs 2021 f=[default,std-like] d=[] rustc=util
util [cargo:x86_64-unknown-none] x86_64-unknown-none Lib crates/util/src/lib.rs 2021 f=[default,extra,std-like] d=[] rustc=util
util-helpers [cargo:x86_64-unknown-none] x86_64-unknown-none Lib crates/helpers/src/lib.rs 2021 f=[] d=[util->util] rustc=util_helpers
"
    );

    let group = |name: &str| {
        let h = raw.groups.lookup(name).expect(name);
        raw.groups.get(h).unwrap().clone()
    };
    assert_eq!(group("kernel").crates, vec!["kernel@lib", "kernel"]);
    let host = group("cargo:host");
    assert_eq!(host.target, "host");
    assert!(!host.config);
    assert_eq!(host.crates, vec!["fixture-derive", "util@host"]);
    assert_eq!(
        group("cargo:x86_64-unknown-none").crates,
        vec!["util", "util-helpers"]
    );

    // Root units take the root's settings; others do not.
    let krate = |name: &str| {
        let h = raw.crates.lookup(name).expect(name);
        raw.crates.get(h).unwrap().clone()
    };
    let bin = krate("kernel");
    assert_eq!(
        bin.linker_script.as_deref(),
        Some("crates/kernel/kernel.ld")
    );
    assert!(krate("util").linker_script.is_none());

    let cargo = bin.cargo.expect("manifest unit");
    assert_eq!(cargo.package.name, "kernel");
    assert_eq!(cargo.package.version, "0.3.1");
    assert_eq!(cargo.package.manifest_dir, "crates/kernel");
    assert_eq!(cargo.declared_features, vec!["default", "log", "unused"]);
    assert_eq!(cargo.check_cfg, vec!["cfg(fixture_cfg)"]);
    let lints: Vec<String> = cargo
        .lints
        .iter()
        .map(|l| format!("{} {}::{} {}", l.priority, l.tool, l.name, l.level))
        .collect();
    assert_eq!(
        lints,
        vec![
            "-1 clippy::all warn",
            "-1 rust::missing_debug_implementations allow",
            "0 rust::unexpected_cfgs warn",
            "0 rust::unused_must_use deny",
        ]
    );
    let env = &cargo.env;
    assert_eq!(env["CARGO_PKG_NAME"], "kernel");
    assert_eq!(env["CARGO_PKG_VERSION"], "0.3.1");
    assert_eq!(env["CARGO_PKG_AUTHORS"], "Quon Tests:Second Author");
    assert_eq!(env["CARGO_PKG_DESCRIPTION"], "manifest build fixture");
    assert_eq!(env["CARGO_CRATE_NAME"], "kernel");
    assert_eq!(env["CARGO_BIN_NAME"], "kernel");
    assert_eq!(env["CARGO_PRIMARY_PACKAGE"], "1");
    assert_eq!(
        Path::new(&env["CARGO_MANIFEST_DIR"]),
        fixture().join("crates/kernel")
    );

    let util = krate("util").cargo.unwrap();
    assert_eq!(util.env["CARGO_PKG_VERSION_PRE"], "pre.1");
    assert!(!util.env.contains_key("CARGO_PRIMARY_PACKAGE"));
    assert!(!util.env.contains_key("CARGO_BIN_NAME"));
    // `util` has no `[lints]` table, so it does not inherit the
    // workspace lints.
    assert!(util.lints.is_empty());
}

/// The same input expands to the same model, byte for byte.
#[test]
fn expansion_is_deterministic() {
    let mut a = fixture_raw();
    let mut b = fixture_raw();
    expand_at(&mut a, &fixture()).unwrap();
    expand_at(&mut b, &fixture()).unwrap();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

#[test]
fn model_without_workspace_is_untouched() {
    let mut raw = raw_model(|q, t| {
        q.add(quon_config::Crate::lib("kernel", "crates/kernel").target(t));
    });
    let before = serde_json::to_string(&raw).unwrap();
    // A directory without Cargo.toml: nothing may be read.
    let empty = tempfile::tempdir().unwrap();
    expand_at(&mut raw, empty.path()).unwrap();
    assert_eq!(serde_json::to_string(&raw).unwrap(), before);
}

#[test]
fn root_features_and_raw_binary_apply() {
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(
            Package::new("kernel")
                .target(t)
                .no_default_features()
                .features(["unused"])
                .raw_binary(),
        );
    });
    expand_at(&mut raw, &fixture()).unwrap();
    let h = raw.crates.lookup("kernel").unwrap();
    let bin = raw.crates.get(h).unwrap();
    assert_eq!(bin.features, vec!["unused"]);
    assert_eq!(bin.crate_type, CrateType::RawBinary);
    let h = raw.crates.lookup("kernel@lib").unwrap();
    assert_eq!(raw.crates.get(h).unwrap().crate_type, CrateType::Lib);
}

// ---------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------

/// A workspace in a temp dir: `files` are `(relative path, contents)`.
fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }
    dir
}

const WS: &str = "[workspace]\nresolver = \"2\"\nmembers = [\"a\"]\n";

fn root_a() -> RawBuildModel {
    raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("a").target(t));
    })
}

#[test]
fn registry_dependency_needs_a_lock() {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nbitflags = \"2\"\n",
        ),
        ("a/src/lib.rs", ""),
    ]);
    let err = expand_err(&mut root_a(), ws.path());
    assert!(
        err.contains("Cargo.lock is missing; run `cargo update`"),
        "{err}"
    );
}

const A_WITH_REGISTRY_DEP: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\
                                   edition = \"2021\"\n[dependencies]\nflags = \"1\"\n";

fn registry_lock(checksum: &str) -> String {
    format!(
        "version = 4\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\n\
         dependencies = [\"flags\"]\n[[package]]\nname = \"flags\"\nversion = \"1.2.0\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
         checksum = \"{checksum}\"\n"
    )
}

/// A workspace whose member `a` depends on registry package
/// `flags 1.2.0`, vendored into `vendor/` and stamped.
fn vendored_workspace(checksum: &str) -> tempfile::TempDir {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        ("Cargo.lock", &registry_lock(checksum)),
        ("a/Cargo.toml", A_WITH_REGISTRY_DEP),
        ("a/src/lib.rs", ""),
        (
            "vendor/flags-1.2.0/Cargo.toml",
            "[package]\nname = \"flags\"\nversion = \"1.2.0\"\nedition = \"2021\"\n",
        ),
        ("vendor/flags-1.2.0/src/lib.rs", ""),
    ]);
    crate::manifest::sources::write_vendor_stamp(ws.path(), &ws.path().join("vendor")).unwrap();
    ws
}

fn root_a_vendored() -> RawBuildModel {
    raw_model(|q, t| {
        q.cargo_workspace(".").vendored("vendor");
        q.add(Package::new("a").target(t));
    })
}

/// The units of `raw` expanded at `root`.
fn units_at(mut raw: RawBuildModel, root: &Path) -> crate::unit::UnitGraph {
    expand_at(&mut raw, root).expect("expand");
    let (model, diags) = quon_model::intern(raw);
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = crate::config::resolve(&model, "dev", None, root, None).expect("resolve");
    crate::unit::lower(
        &model,
        &resolved,
        &crate::vendor::cross::context::VendorBuildContext::new(),
    )
    .expect("lower")
}

fn unit_named(units: &crate::unit::UnitGraph, name: &str) -> crate::unit::BuildUnit {
    units
        .iter()
        .find(|(_, u)| u.name == name)
        .map(|(_, u)| u.clone())
        .expect(name)
}

#[test]
fn vendored_registry_dependency_is_lowered() {
    let ws = vendored_workspace("abc");
    let mut raw = root_a_vendored();
    expand_at(&mut raw, ws.path()).expect("expand");
    let flags = raw
        .crates
        .lookup("flags")
        .and_then(|h| raw.crates.get(h))
        .expect("flags unit");
    assert_eq!(flags.group, "cargo:x86_64-unknown-none");
    assert!(!flags.is_project_crate);
    let cargo = flags.cargo.as_ref().unwrap();
    assert_eq!(
        cargo.package.source,
        CargoPackageSource::Registry {
            checksum: Some("abc".into())
        }
    );
    let a = raw
        .crates
        .lookup("a")
        .and_then(|h| raw.crates.get(h))
        .unwrap();
    assert_eq!(
        a.cargo.as_ref().unwrap().package.source,
        CargoPackageSource::Path
    );

    let units = units_at(root_a_vendored(), ws.path());
    let flags = unit_named(&units, "flags");
    assert!(flags.settings.cap_lints);
    assert_eq!(
        flags.package.source,
        crate::unit::PackageSource::Registry {
            checksum: Some("abc".into())
        }
    );
    assert!(!unit_named(&units, "a").settings.cap_lints);

    // Another checksum is another package: a new metadata hash.
    let other = vendored_workspace("def");
    let other_units = units_at(root_a_vendored(), other.path());
    assert_ne!(flags.metadata, unit_named(&other_units, "flags").metadata);
}

/// `syn` 2 has `test = ["syn-test-suite/all-features"]`, where
/// `syn-test-suite` is a dev-dependency. Cargo accepts that, and so must
/// Quon, though it drops a registry package's dev-dependencies.
#[test]
fn registry_feature_may_name_a_dev_dependency() {
    let ws = vendored_workspace("abc");
    std::fs::write(
        ws.path().join("vendor/flags-1.2.0/Cargo.toml"),
        "[package]\nname = \"flags\"\nversion = \"1.2.0\"\nedition = \"2021\"\n\
         [features]\ntest = [\"suite/all-features\"]\n\
         [dev-dependencies.suite]\nversion = \"1\"\n",
    )
    .unwrap();
    crate::manifest::sources::write_vendor_stamp(ws.path(), &ws.path().join("vendor")).unwrap();
    let mut raw = root_a_vendored();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert!(raw.crates.lookup("flags").is_some());
}

#[test]
fn vendored_mode_needs_a_current_stamp() {
    let ws = vendored_workspace("abc");
    std::fs::write(ws.path().join("Cargo.lock"), registry_lock("abd")).unwrap();
    let err = expand_err(&mut root_a_vendored(), ws.path());
    assert!(
        err.contains("Cargo.lock changed since the last `quon vendor`"),
        "{err}"
    );
}

#[test]
fn vendored_mode_reports_a_missing_package_directory() {
    let ws = vendored_workspace("abc");
    std::fs::remove_dir_all(ws.path().join("vendor/flags-1.2.0")).unwrap();
    let err = expand_err(&mut root_a_vendored(), ws.path());
    assert!(
        err.contains("source of `flags 1.2.0` is missing") && err.contains("run `quon vendor`"),
        "{err}"
    );
}

/// An optional registry dependency no feature enables is never
/// activated, so it is not an error.
#[test]
fn inactive_registry_dependency_is_ignored() {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nbitflags = { version = \"2\", optional = true }\n",
        ),
        ("a/src/lib.rs", ""),
    ]);
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert!(raw.crates.lookup("a").is_some());
}

/// Member `a` (lib and bin) has a build script with build-dep `helper`
/// and `links = "foo"`.
fn scripted_workspace() -> tempfile::TempDir {
    workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"helper\"]\n",
        ),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             links = \"foo\"\n[build-dependencies]\nhelper = { path = \"../helper\" }\n",
        ),
        ("a/src/lib.rs", ""),
        ("a/src/main.rs", "fn main() {}"),
        ("a/build.rs", "fn main() {}"),
        (
            "helper/Cargo.toml",
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("helper/src/lib.rs", ""),
    ])
}

/// The script's build-dep is reachable from the run binary through
/// the script alone, so it is in the build.
fn scripted_raw() -> RawBuildModel {
    raw_model(|q, t| {
        q.cargo_workspace(".");
        let a = q.add(Package::new("a").target(t));
        q.run(&a);
    })
}

#[test]
fn build_script_is_carried_by_each_owner() {
    let ws = scripted_workspace();
    let mut raw = scripted_raw();
    expand_at(&mut raw, ws.path()).expect("expand");
    for owner in ["a", "a@lib"] {
        let def = raw
            .crates
            .lookup(owner)
            .and_then(|h| raw.crates.get(h))
            .expect(owner);
        let script = def.cargo.as_ref().unwrap().build_script.as_ref();
        assert_eq!(
            script,
            Some(&CargoBuildScriptDef {
                unit: "a@build-script".into(),
                path: "build.rs".into(),
                edition: "2021".into(),
                deps: BTreeMap::from([("helper".into(), "helper".into())]),
                links: Some("foo".into()),
            }),
            "{owner}"
        );
    }
    // The script is not a crate; its build-dep is a host crate.
    assert!(raw.crates.lookup("a@build-script").is_none());
    let helper = raw
        .crates
        .lookup("helper")
        .and_then(|h| raw.crates.get(h))
        .unwrap();
    assert_eq!(helper.group, "cargo:host");
}

#[test]
fn build_script_is_one_host_unit_with_a_run_per_owner() {
    use crate::unit::{DepTarget, Platform, UnitProfile};
    let ws = scripted_workspace();
    let units = units_at(scripted_raw(), ws.path());
    let script = unit_named(&units, "a@build-script");
    assert!(script.is_build_script);
    assert_eq!(script.crate_name, "build_script_build");
    assert_eq!(script.platform, Platform::Host);
    assert_eq!(script.profile, UnitProfile::Host);
    assert!(!script.settings.cap_lints);
    assert!(script.src.ends_with("a/build.rs"), "{:?}", script.src);
    let env = &script.settings.cargo.as_ref().unwrap().env;
    assert_eq!(env["CARGO_CRATE_NAME"], "build_script_build");
    assert_eq!(env["CARGO_PKG_NAME"], "a");

    let (helper_id, helper) = units
        .iter()
        .find(|(_, u)| u.name == "helper")
        .expect("helper is in the build");
    assert_eq!(helper.platform, Platform::Host);
    assert_eq!(helper.profile, UnitProfile::Host);
    assert_eq!(script.deps.len(), 1);
    assert_eq!(script.deps[0].extern_name, "helper");
    assert_eq!(script.deps[0].target, DepTarget::Unit(helper_id));

    let runs: Vec<_> = units.runs().map(|(_, r)| r.clone()).collect();
    let owners: BTreeSet<String> = runs
        .iter()
        .map(|r| units.get(r.owner).name.clone())
        .collect();
    assert_eq!(owners, BTreeSet::from(["a".into(), "a@lib".into()]));
    let script_ids: BTreeSet<_> = runs.iter().map(|r| r.script).collect();
    assert_eq!(script_ids.len(), 1, "owners share one compile");
    for r in &runs {
        assert_eq!(r.package.name, "a");
        assert_eq!(r.package.version, "0.1.0");
        assert_eq!(r.package.links.as_deref(), Some("foo"));
        assert!(r.package.manifest_dir.ends_with("a"));
        assert!(r.package.manifest_path.ends_with("a/Cargo.toml"));
    }
}

#[test]
fn shared_links_value_is_rejected() {
    let ws = workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"b\"]\n",
        ),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             links = \"foo\"\n[dependencies]\nb = { path = \"../b\" }\n",
        ),
        ("a/src/lib.rs", ""),
        ("a/build.rs", "fn main() {}"),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             links = \"foo\"\n",
        ),
        ("b/src/lib.rs", ""),
        ("b/build.rs", "fn main() {}"),
    ]);
    let err = expand_err(&mut root_a(), ws.path());
    assert!(
        err.contains("packages `a`, `b` all set `links = \"foo\"` for x86_64-unknown-none"),
        "{err}"
    );
}

#[test]
fn root_crate_type_needs_a_bin_target() {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("a/src/lib.rs", ""),
    ]);
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("a").target(t).raw_binary());
    });
    let err = expand_err(&mut raw, ws.path());
    assert!(
        err.contains(
            "package root 'a' has crate type RawBinary in quon.rs, but package `a` has no bin \
             target"
        ),
        "{err}"
    );
    assert!(err.contains("a/Cargo.toml"), "{err}");
}

/// A root package's `harness = false` tests become bin crates marked as
/// tests, linked to the package's lib, with the root's settings and
/// `--cfg test`. A test named like the package's bin yields the plain
/// name. Tests that need libtest are skipped with a warning.
#[test]
fn root_tests_become_test_crates() {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [[test]]\nname = \"boot\"\nharness = false\n\
             [[test]]\nname = \"a\"\nharness = false\n",
        ),
        ("a/src/lib.rs", ""),
        ("a/src/main.rs", ""),
        ("a/tests/boot.rs", ""),
        ("a/tests/a.rs", ""),
        ("a/tests/hosted.rs", ""),
    ]);
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("a").target(t).linker_script("a/link.ld"));
    });
    let warnings = expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        dump(&raw),
        "\
a@lib [a] x86_64-unknown-none Lib a/src/lib.rs 2021 f=[] d=[] rustc=a
a [a] x86_64-unknown-none Bin a/src/main.rs 2021 f=[] d=[a->a@lib] rustc=a
a@test [a] x86_64-unknown-none Bin a/tests/a.rs 2021 f=[] d=[a->a@lib] rustc=a
boot [a] x86_64-unknown-none Bin a/tests/boot.rs 2021 f=[] d=[a->a@lib] rustc=boot
"
    );
    let krate = |name: &str| {
        let h = raw.crates.lookup(name).expect(name);
        raw.crates.get(h).unwrap().clone()
    };
    let boot = krate("boot");
    assert!(boot.cargo.as_ref().unwrap().is_test);
    assert_eq!(boot.linker_script.as_deref(), Some("a/link.ld"));
    assert_eq!(boot.cfg_flags, ["test"]);
    assert!(!krate("a").cargo.as_ref().unwrap().is_test);
    assert!(krate("a").cfg_flags.is_empty());

    let messages: Vec<String> = warnings.iter().map(|w| w.to_string()).collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("test `hosted` of package `a` uses the libtest harness")),
        "{messages:?}"
    );
}

/// `quon build` leaves test crates out; check and clippy keep them.
#[test]
fn build_skips_test_crates() {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\
             [[test]]\nname = \"boot\"\nharness = false\n",
        ),
        ("a/src/main.rs", ""),
        ("a/tests/boot.rs", ""),
    ]);
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("a").target(t));
    });
    expand_at(&mut raw, ws.path()).expect("expand");
    let (model, diags) = quon_model::intern(raw);
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = crate::config::resolve(&model, "dev", None, ws.path(), None).unwrap();
    let names = |r: &ResolvedConfig| -> Vec<String> {
        r.crates
            .iter()
            .map(|c| model.crates.get(c.handle).unwrap().name.clone())
            .collect()
    };
    assert_eq!(names(&resolved), ["a", "boot"]);
    assert_eq!(names(&crate::without_tests(&model, &resolved)), ["a"]);
}

// ---------------------------------------------------------------------
// Dev-dependencies
// ---------------------------------------------------------------------

/// `[package]` for `name`, then `rest`.
fn manifest(name: &str, rest: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{rest}")
}

/// Member `a` (lib, bin and the `harness = false` test `boot`) with
/// `a_rest` appended to its manifest, plus `others`: `(dir, manifest
/// rest)` for each lib package beside it.
fn dev_workspace(a_rest: &str, others: &[(&str, &str)]) -> tempfile::TempDir {
    let a = manifest(
        "a",
        &format!("[[test]]\nname = \"boot\"\nharness = false\n{a_rest}"),
    );
    let mut files: Vec<(String, String)> = vec![
        ("Cargo.toml".into(), WS.into()),
        ("a/Cargo.toml".into(), a),
        ("a/src/lib.rs".into(), String::new()),
        ("a/src/main.rs".into(), String::new()),
        ("a/tests/boot.rs".into(), String::new()),
    ];
    for (name, rest) in others {
        files.push((format!("{name}/Cargo.toml"), manifest(name, rest)));
        files.push((format!("{name}/src/lib.rs"), String::new()));
    }
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    workspace(&files)
}

/// The test-only crates of an expanded model, in arena order.
fn test_only(raw: &RawBuildModel) -> Vec<String> {
    raw.crates
        .iter()
        .filter(|(_, c)| c.cargo.as_ref().is_some_and(|c| c.test_only))
        .map(|(_, c)| c.name.clone())
        .collect()
}

/// A dev-dependency links into the tests only, and `quon build` drops
/// it along with the tests.
#[test]
fn dev_dependency_is_linked_by_tests_only() {
    let ws = dev_workspace(
        "[dev-dependencies]\nkit = { path = \"../kit\" }\n",
        &[("kit", "")],
    );
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        dump(&raw),
        "\
a@lib [a] x86_64-unknown-none Lib a/src/lib.rs 2021 f=[] d=[] rustc=a
a [a] x86_64-unknown-none Bin a/src/main.rs 2021 f=[] d=[a->a@lib] rustc=a
boot [a] x86_64-unknown-none Bin a/tests/boot.rs 2021 f=[] d=[a->a@lib,kit->kit] rustc=boot
kit [cargo:x86_64-unknown-none] x86_64-unknown-none Lib kit/src/lib.rs 2021 f=[] d=[] rustc=kit
"
    );
    assert_eq!(test_only(&raw), ["boot", "kit"]);

    let (model, diags) = quon_model::intern(raw);
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = crate::config::resolve(&model, "dev", None, ws.path(), None).unwrap();
    let names: BTreeSet<String> = crate::without_tests(&model, &resolved)
        .crates
        .iter()
        .map(|c| model.crates.get(c.handle).unwrap().name.clone())
        .collect();
    assert_eq!(names, BTreeSet::from(["a".into(), "a@lib".into()]));
}

/// A dev-dependency that turns on a feature of `util`, which the kernel
/// also uses, gives the tests their own `util` and `a` lib. The
/// kernel's units are the same as without the dev-dependency.
#[test]
fn dev_dependency_feature_copies_the_shared_dependency() {
    let util = ("util", "[features]\nextra = []\n");
    let kit = (
        "kit",
        "[dependencies]\nutil = { path = \"../util\", features = [\"extra\"] }\n",
    );
    let normal = "[dependencies]\nutil = { path = \"../util\" }\n";
    let ws = dev_workspace(
        &format!("{normal}[dev-dependencies]\nkit = {{ path = \"../kit\" }}\n"),
        &[util, kit],
    );
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        dump(&raw),
        "\
a@lib [a] x86_64-unknown-none Lib a/src/lib.rs 2021 f=[] d=[util->util] rustc=a
a@test-pass [a] x86_64-unknown-none Lib a/src/lib.rs 2021 f=[] d=[util->util@test-pass] rustc=a
a [a] x86_64-unknown-none Bin a/src/main.rs 2021 f=[] d=[a->a@lib,util->util] rustc=a
boot [a] x86_64-unknown-none Bin a/tests/boot.rs 2021 f=[] d=[a->a@test-pass,kit->kit,util->util@test-pass] rustc=boot
kit [cargo:x86_64-unknown-none] x86_64-unknown-none Lib kit/src/lib.rs 2021 f=[] d=[util->util@test-pass] rustc=kit
util [cargo:x86_64-unknown-none] x86_64-unknown-none Lib util/src/lib.rs 2021 f=[] d=[] rustc=util
util@test-pass [cargo:x86_64-unknown-none] x86_64-unknown-none Lib util/src/lib.rs 2021 f=[extra] d=[] rustc=util
"
    );
    assert_eq!(
        test_only(&raw),
        ["a@test-pass", "boot", "kit", "util@test-pass"]
    );

    let with_tests = units_at(root_a(), ws.path());
    assert_ne!(
        unit_named(&with_tests, "util").metadata,
        unit_named(&with_tests, "util@test-pass").metadata
    );
    // The same workspace without the test and the dev-dependency.
    std::fs::write(ws.path().join("a/Cargo.toml"), manifest("a", normal)).unwrap();
    std::fs::remove_dir_all(ws.path().join("a/tests")).unwrap();
    let without = units_at(root_a(), ws.path());
    for name in ["a", "a@lib", "util"] {
        assert_eq!(
            unit_named(&with_tests, name).metadata,
            unit_named(&without, name).metadata,
            "{name}"
        );
    }
}

/// A `[target.'cfg(..)'.dev-dependencies]` key is matched against the
/// root's target, as for normal dependencies.
#[test]
fn dev_dependency_platform_key_matches_the_root_target() {
    let ws = dev_workspace(
        "[target.'cfg(target_os = \"none\")'.dev-dependencies]\nkit = { path = \"../kit\" }\n\
         [target.'cfg(unix)'.dev-dependencies]\nhosted = { path = \"../hosted\" }\n",
        &[("kit", ""), ("hosted", "")],
    );
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(test_only(&raw), ["boot", "kit"]);
    let boot = raw.crates.get(raw.crates.lookup("boot").unwrap()).unwrap();
    let deps: Vec<&str> = boot.deps.keys().map(String::as_str).collect();
    assert_eq!(deps, ["a", "kit"]);
}

/// A proc-macro dev-dependency builds for the host.
#[test]
fn proc_macro_dev_dependency_builds_for_the_host() {
    let ws = dev_workspace(
        "[dev-dependencies]\nmac = { path = \"../mac\" }\n",
        &[("mac", "[lib]\nproc-macro = true\n")],
    );
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("expand");
    let out = dump(&raw);
    assert!(
        out.contains("mac [cargo:host] host ProcMacro mac/src/lib.rs"),
        "{out}"
    );
    assert!(
        out.contains(
            "boot [a] x86_64-unknown-none Bin a/tests/boot.rs 2021 f=[] d=[a->a@lib,mac->mac]"
        ),
        "{out}"
    );
    assert_eq!(test_only(&raw), ["boot", "mac"]);
}

/// Only the dev-dependencies of a root with tests apply: not those of
/// another member it depends on, and not a registry package's, which
/// loading drops.
#[test]
fn dev_dependencies_of_other_packages_never_apply() {
    let ws = dev_workspace(
        "[dependencies]\nutil = { path = \"../util\" }\n",
        &[
            (
                "util",
                "[dev-dependencies]\nghost = { path = \"../ghost\" }\n",
            ),
            ("ghost", ""),
        ],
    );
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert!(raw.crates.lookup("ghost").is_none());
    assert_eq!(test_only(&raw), ["boot"]);

    // `suite` is in no lock or vendor directory, so following the edge
    // would fail.
    let ws = vendored_workspace("abc");
    let files = [
        (
            "a/Cargo.toml",
            format!("{A_WITH_REGISTRY_DEP}[[test]]\nname = \"boot\"\nharness = false\n"),
        ),
        ("a/tests/boot.rs", String::new()),
        (
            "vendor/flags-1.2.0/Cargo.toml",
            manifest("flags", "[dev-dependencies]\nsuite = \"1\"\n").replace("0.1.0", "1.2.0"),
        ),
    ];
    for (path, contents) in files {
        let p = ws.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }
    crate::manifest::sources::write_vendor_stamp(ws.path(), &ws.path().join("vendor")).unwrap();
    let mut raw = root_a_vendored();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(test_only(&raw), ["boot"]);
}

/// A dev-dependency that cannot be resolved fails the expansion when a
/// test needs it, naming the package and the dev-dependency, and is
/// ignored when the package has no tests.
#[test]
fn unresolved_dev_dependency_names_the_package() {
    let dev = "[dev-dependencies]\nbitflags = \"2\"\n";
    let ws = dev_workspace(dev, &[]);
    let err = expand_err(&mut root_a(), ws.path());
    assert!(err.contains("dev-dependency `bitflags` of `a`"), "{err}");

    std::fs::write(ws.path().join("a/Cargo.toml"), manifest("a", dev)).unwrap();
    std::fs::remove_dir_all(ws.path().join("a/tests")).unwrap();
    expand_at(&mut root_a(), ws.path()).expect("expand without tests");
}

/// A path lib used by the kernel and by a proc macro is built twice:
/// once per platform, with its own hash and paths. The unit table
/// needs nothing special for it.
#[test]
fn path_crate_on_host_and_target_is_two_units() {
    use crate::unit::Platform;
    let ws = workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"pm\", \"shared\"]\n",
        ),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\npm = { path = \"../pm\" }\nshared = { path = \"../shared\" }\n",
        ),
        ("a/src/lib.rs", ""),
        (
            "pm/Cargo.toml",
            "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [lib]\nproc-macro = true\n[dependencies]\nshared = { path = \"../shared\" }\n",
        ),
        ("pm/src/lib.rs", ""),
        (
            "shared/Cargo.toml",
            "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("shared/src/lib.rs", ""),
    ]);
    let units = units_at(root_a(), ws.path());
    let target = unit_named(&units, "shared");
    let host = unit_named(&units, "shared@host");
    assert!(matches!(target.platform, Platform::Cross(_)));
    assert_eq!(host.platform, Platform::Host);
    assert_ne!(target.metadata, host.metadata);

    let (model, resolved) = {
        let mut raw = root_a();
        expand_at(&mut raw, ws.path()).unwrap();
        let (model, _) = quon_model::intern(raw);
        let resolved = crate::config::resolve(&model, "dev", None, ws.path(), None).unwrap();
        (model, resolved)
    };
    let layout = BuildLayout::new(ws.path().join("build"), "demo");
    let id = |name: &str| {
        units
            .iter()
            .find(|(_, u)| u.name == name)
            .map(|(id, _)| id)
            .unwrap()
    };
    let path = |name: &str| {
        units
            .paths(id(name), &layout, &model, &resolved)
            .unwrap()
            .output_path
    };
    assert_ne!(path("shared"), path("shared@host"));
    let key = |name: &str| {
        units
            .cache_key(id(name), &layout, &model, &resolved)
            .unwrap()
    };
    assert_ne!(key("shared"), key("shared@host"));
    // The proc macro links the host copy.
    let pm = unit_named(&units, "pm");
    assert_eq!(
        pm.deps[0].target,
        crate::unit::DepTarget::Unit(id("shared@host"))
    );
}

#[test]
fn unsupported_lib_crate_type_is_rejected() {
    let ws = workspace(&[
        ("Cargo.toml", WS),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [lib]\ncrate-type = [\"rlib\", \"staticlib\"]\n",
        ),
        ("a/src/lib.rs", ""),
    ]);
    let err = expand_err(&mut root_a(), ws.path());
    assert!(
        err.contains("package `a`: lib crate type `rlib, staticlib` is not supported"),
        "{err}"
    );
}

#[test]
fn unknown_package_is_rejected() {
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("nope").target(t));
    });
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains("package `nope` is not a member of the Cargo workspace at"),
        "{err}"
    );
}

#[test]
fn package_without_workspace_is_rejected() {
    // `Quon` reports a package without a workspace itself, so the
    // workspace is removed from the raw model to reach the check in
    // manifest expansion.
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("kernel").target(t));
    });
    raw.cargo_workspace = None;
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains("`Package::new(\"kernel\")` needs a Cargo workspace"),
        "{err}"
    );
}

#[test]
fn declared_crate_overlapping_the_workspace_is_rejected() {
    use quon_config::Crate;
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("kernel").target(t));
        q.add(Crate::lib("loader", "crates/loader").target(t));
        q.add(Crate::lib("again", "./crates/util/").target(t));
        q.add(Crate::lib("util-helpers", "elsewhere").target(t));
    });
    // `util` is a workspace package, which `Quon` has no handle for, so
    // the dependency on it is added to the raw model.
    let loader = raw.crates.lookup("loader").unwrap();
    raw.crates.get_mut(loader).unwrap().deps.insert(
        "util".into(),
        quon_model::RawDepDef {
            crate_name: "util".into(),
            features: Vec::new(),
            version: None,
            optional: false,
            default_features: true,
            span: None,
        },
    );
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains(
            "crate 'loader' (declared with `Crate`) depends on 'util' (as `util`), which \
             comes from the Cargo workspace"
        ),
        "{err}"
    );
    assert!(
        err.contains("which is the directory of workspace member `util`"),
        "{err}"
    );
    assert!(
        err.contains(
            "crate 'util-helpers' is declared with `Crate`, and package `util-helpers` in \
             the Cargo workspace has a unit of the same name"
        ),
        "{err}"
    );
}

#[test]
fn no_sysroot_root_with_dependencies_is_rejected() {
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("kernel").target(t).no_sysroot());
    });
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains("package `kernel` is built with `no_sysroot` but has dependencies"),
        "{err}"
    );
}

// ---------------------------------------------------------------------
// Through intern, unit lowering and the command builder
// ---------------------------------------------------------------------

fn fixture_model() -> (BuildModel, ResolvedConfig) {
    let mut raw = fixture_raw();
    expand_at(&mut raw, &fixture()).expect("expand");
    let (model, diags) = quon_model::intern(raw);
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = crate::config::resolve(&model, "dev", None, &fixture(), None).expect("resolve");
    (model, resolved)
}

fn fake_rustc() -> crate::compile::RustcInfo {
    crate::compile::RustcInfo {
        rustc_path: "/fake/rustc".into(),
        rustc_arg: "rustc".into(),
        version: "rustc 0.0.0 (test 2020-01-01)".into(),
        host_triple: "x86_64-unknown-linux-gnu".into(),
        commit_hash: None,
        release: "0.0.0".into(),
        sysroot: "/fake-sysroot".into(),
        rust_src: None,
        mtime_ns: 0,
    }
}

/// The argv and env of `unit`'s rustc command, with every dep's
/// artifact at a fake path.
fn command_of(unit: &str, driver: DriverKind) -> (Vec<String>, BTreeMap<String, String>) {
    let (model, resolved) = fixture_model();
    let units = crate::unit::lower(
        &model,
        &resolved,
        &crate::vendor::cross::context::VendorBuildContext::new(),
    )
    .expect("lower");
    let tmp = tempfile::tempdir().unwrap();
    let layout = BuildLayout::with_driver(tmp.path().join("build"), "demo", driver);
    let ctx = CompileCtx::new(layout, Arc::new(fake_rustc()));
    let mut artifacts = ArtifactMap::new();
    for (id, u) in units.iter() {
        artifacts.insert_unit(id, PathBuf::from(format!("/artifacts/{}", u.name)));
    }
    for (t, _) in model.targets.iter() {
        artifacts.set_config_crate(t, PathBuf::from("/artifacts/config"));
    }
    let id = units
        .iter()
        .find(|(_, u)| u.name == unit)
        .map(|(id, _)| id)
        .expect(unit);
    let cmd = crate::compile::command_builder::build_unit_command(
        &ctx,
        &CompileUnitInput {
            model: &model,
            resolved: &resolved,
            units: &units,
            unit: id,
            artifacts: &artifacts,
            sysroot_dir: Some(Path::new("/sysroot")),
        },
    )
    .expect("command");
    let args = cmd
        .builder
        .args()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let env = cmd
        .builder
        .envs()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    (args, env)
}

/// `flag value` pairs of `args`, in order.
fn pairs(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].clone())
        .collect()
}

#[test]
fn units_carry_manifest_identity() {
    let (model, resolved) = fixture_model();
    let units = crate::unit::lower(
        &model,
        &resolved,
        &crate::vendor::cross::context::VendorBuildContext::new(),
    )
    .unwrap();
    let unit = |name: &str| {
        units
            .iter()
            .find(|(_, u)| u.name == name)
            .map(|(_, u)| u.clone())
            .expect(name)
    };
    let bin = unit("kernel");
    let lib = unit("kernel@lib");
    assert_eq!(bin.crate_name, "kernel");
    assert_eq!(lib.crate_name, "kernel");
    assert_eq!(bin.package.name, "kernel");
    assert_eq!(bin.package.version, "0.3.1");
    assert_eq!(
        bin.package.source,
        crate::unit::PackageSource::Path("crates/kernel".into())
    );
    assert_ne!(bin.metadata, lib.metadata);
    // The root's group asks for the config crate; the synthetic group
    // of dependencies does not.
    assert!(bin.config_crate);
    assert!(unit("util").sysroot);
    assert!(!unit("util").config_crate);
    // Host and target copies of one package are different artifacts.
    assert_ne!(unit("util").metadata, unit("util@host").metadata);
    assert_eq!(unit("util@host").platform, crate::unit::Platform::Host);
    assert_eq!(unit("util@host").profile, crate::unit::UnitProfile::Host);
}

#[test]
fn command_has_cargo_flags_and_env() {
    let (args, env) = command_of("kernel", DriverKind::Rustc);
    assert_eq!(pairs(&args, "--crate-name"), vec!["kernel"]);
    assert_eq!(
        pairs(&args, "--cfg"),
        vec!["feature=\"default\"", "feature=\"log\""]
    );
    assert_eq!(
        pairs(&args, "--check-cfg"),
        vec![
            "cfg(docsrs,test)",
            "cfg(feature, values(\"default\", \"log\", \"unused\"))",
            "cfg(fixture_cfg)",
        ]
    );
    // Clippy lints only under clippy-driver.
    assert_eq!(pairs(&args, "-A"), vec!["missing_debug_implementations"]);
    assert_eq!(pairs(&args, "-W"), vec!["unexpected_cfgs"]);
    assert_eq!(pairs(&args, "-D"), vec!["unused_must_use"]);
    // Renamed dep under its key, the bin's own lib under its lib name.
    let externs = pairs(&args, "--extern");
    for e in [
        "helpers=/artifacts/util-helpers",
        "util=/artifacts/util",
        "kernel=/artifacts/kernel@lib",
        "fixture_derive=/artifacts/fixture-derive",
    ] {
        assert!(externs.iter().any(|x| x == e), "{e} not in {externs:?}");
    }
    assert!(args.iter().any(|a| a.starts_with("metadata=")), "{args:?}");
    assert_eq!(env["CARGO_PKG_VERSION"], "0.3.1");
    assert_eq!(env["CARGO_CRATE_NAME"], "kernel");
    assert_eq!(env["CARGO_BIN_NAME"], "kernel");
    assert_eq!(env["CARGO_PRIMARY_PACKAGE"], "1");
}

#[test]
fn clippy_lints_reach_clippy_driver_only() {
    let (args, _) = command_of("kernel", DriverKind::Clippy);
    assert_eq!(pairs(&args, "-W"), vec!["clippy::all", "unexpected_cfgs"]);
}

#[test]
fn cfg_flags_are_declared_to_check_cfg() {
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(
            Package::new("kernel")
                .target(t)
                .cfg("kernel_debug")
                .cfg("mode=\"fast\""),
        );
    });
    expand_at(&mut raw, &fixture()).unwrap();
    let (model, _) = quon_model::intern(raw);
    let resolved = crate::config::resolve(&model, "dev", None, &fixture(), None).unwrap();
    let units = crate::unit::lower(
        &model,
        &resolved,
        &crate::vendor::cross::context::VendorBuildContext::new(),
    )
    .unwrap();
    let (_, bin) = units.iter().find(|(_, u)| u.name == "kernel").unwrap();
    let decls = &bin.settings.cargo.as_ref().unwrap().check_cfg;
    assert!(
        decls.contains(&"cfg(kernel_debug)".to_string()),
        "{decls:?}"
    );
    assert!(
        decls.contains(&"cfg(mode, values(\"fast\"))".to_string()),
        "{decls:?}"
    );
}

/// Member `a` (lib, bin, the `harness = false` test `boot` and the
/// libtest test `api`) depending on `util`, with the dev-dependency
/// `kit`. `util` has the feature `extra`, which `a`'s build script
/// turns on through its build-dependency.
fn host_test_workspace() -> tempfile::TempDir {
    let a = manifest(
        "a",
        "build = \"build.rs\"\n\
         [[test]]\nname = \"boot\"\nharness = false\n\
         [dependencies]\nutil = { path = \"../util\" }\n\
         [build-dependencies]\nutil = { path = \"../util\", features = [\"extra\"] }\n\
         [dev-dependencies]\nkit = { path = \"../kit\" }\n",
    );
    let files = [
        ("Cargo.toml", WS.to_string()),
        ("a/Cargo.toml", a),
        ("a/build.rs", "fn main() {}".into()),
        ("a/src/lib.rs", String::new()),
        ("a/src/main.rs", String::new()),
        ("a/tests/boot.rs", String::new()),
        ("a/tests/api.rs", String::new()),
        (
            "util/Cargo.toml",
            manifest("util", "[features]\nextra = []\n"),
        ),
        ("util/src/lib.rs", String::new()),
        ("kit/Cargo.toml", manifest("kit", "")),
        ("kit/src/lib.rs", String::new()),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    workspace(&files)
}

fn root_a_with_host_tests() -> RawBuildModel {
    raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("a").target(t));
        q.host_tests().package("a");
    })
}

/// A host-test package's lib, bin and libtest tests build for the host
/// with `--test`, with its dev-dependencies, in the `cargo:host-test`
/// group. Because `tests/api.rs` exists, the bin builds too, and the
/// test gets its path as `CARGO_BIN_EXE_a`. Their `util` is not the build script's: the host-test
/// domain does not take the build-dependency's `extra` feature. Every
/// host-test unit is test-only, and the target units are unchanged.
#[test]
fn host_tests_build_for_the_host_in_their_own_domain() {
    let ws = host_test_workspace();
    let mut raw = root_a_with_host_tests();
    let warnings = expand_at(&mut raw, ws.path()).expect("expand");
    assert!(
        warnings.iter().all(|w| !w.message.contains("libtest")),
        "{warnings:?}"
    );
    assert_eq!(
        dump(&raw),
        "\
a@host-test@lib [cargo:host-test] host Lib a/src/lib.rs 2021 f=[] d=[util->util@host-test] rustc=a
a@host-test [cargo:host-test] host Bin a/src/main.rs 2021 f=[] d=[a->a@host-test@lib,util->util@host-test] rustc=a
a@test [cargo:host-test] host Test a/src/lib.rs 2021 f=[] d=[kit->kit@host-test,util->util@host-test] rustc=a
a@bin-test [cargo:host-test] host Test a/src/main.rs 2021 f=[] d=[a->a@host-test@lib,kit->kit@host-test,util->util@host-test] rustc=a
api [cargo:host-test] host Test a/tests/api.rs 2021 f=[] d=[a->a@host-test@lib,kit->kit@host-test,util->util@host-test] rustc=api
a@lib [a] x86_64-unknown-none Lib a/src/lib.rs 2021 f=[] d=[util->util] rustc=a
a [a] x86_64-unknown-none Bin a/src/main.rs 2021 f=[] d=[a->a@lib,util->util] rustc=a
boot [a] x86_64-unknown-none Bin a/tests/boot.rs 2021 f=[] d=[a->a@lib,kit->kit,util->util] rustc=boot
kit@host-test [cargo:host-test] host Lib kit/src/lib.rs 2021 f=[] d=[] rustc=kit
kit [cargo:x86_64-unknown-none] x86_64-unknown-none Lib kit/src/lib.rs 2021 f=[] d=[] rustc=kit
util@host [cargo:host] host Lib util/src/lib.rs 2021 f=[extra] d=[] rustc=util
util@host-test [cargo:host-test] host Lib util/src/lib.rs 2021 f=[] d=[] rustc=util
util [cargo:x86_64-unknown-none] x86_64-unknown-none Lib util/src/lib.rs 2021 f=[] d=[] rustc=util
"
    );
    assert_eq!(
        test_only(&raw),
        [
            "a@host-test@lib",
            "a@host-test",
            "a@test",
            "a@bin-test",
            "api",
            "boot",
            "kit@host-test",
            "kit",
            "util@host-test"
        ]
    );
    // The build script runs for the unit tests too, as under `cargo test`.
    let lib_test = raw
        .crates
        .iter()
        .find(|(_, c)| c.name == "a@test")
        .unwrap()
        .1;
    let cargo = lib_test.cargo.as_ref().unwrap();
    assert!(cargo.build_script.is_some());
    assert_eq!(
        cargo.env.get("CARGO_PRIMARY_PACKAGE").map(String::as_str),
        Some("1")
    );
    assert!(!cargo.is_test, "host tests do not boot");
    let api = raw.crates.iter().find(|(_, c)| c.name == "api").unwrap().1;
    assert_eq!(
        api.artifact_env.get("CARGO_BIN_EXE_a").map(String::as_str),
        Some("a@host-test")
    );
    assert_eq!(api.artifact_deps, ["a@host-test"]);
    assert!(
        api.cargo
            .as_ref()
            .unwrap()
            .env
            .contains_key("CARGO_TARGET_TMPDIR"),
        "tests in tests/ get a scratch directory"
    );

    // Listing the package changes none of the target units.
    let with = units_at(root_a_with_host_tests(), ws.path());
    let without = units_at(root_a(), ws.path());
    for name in ["a", "a@lib", "util", "boot", "kit", "util@host"] {
        assert_eq!(
            unit_named(&with, name).metadata,
            unit_named(&without, name).metadata,
            "{name}"
        );
    }
    let lib_test = unit_named(&with, "a@test");
    assert_eq!(lib_test.kind, CrateType::Test);
    assert_eq!(lib_test.platform, crate::unit::Platform::Host);
    assert!(!lib_test.sysroot && !lib_test.config_crate);
}

/// A package needs no target group to have host tests.
#[test]
fn host_tests_need_no_group() {
    let ws = host_test_workspace();
    let mut raw = raw_model(|q, _| {
        q.cargo_workspace(".");
        q.host_tests().package("util");
    });
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        dump(&raw),
        "\
util [cargo:host-test] host Lib util/src/lib.rs 2021 f=[] d=[] rustc=util
util@test [cargo:host-test] host Test util/src/lib.rs 2021 f=[] d=[] rustc=util
"
    );
}

#[test]
fn host_tests_reject_unknown_and_proc_macro_packages() {
    let ws = workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"m\"]\n",
        ),
        ("m/Cargo.toml", &manifest("m", "[lib]\nproc-macro = true\n")),
        ("m/src/lib.rs", ""),
    ]);
    let mut raw = raw_model(|q, _| {
        q.cargo_workspace(".");
        q.host_tests().package("m").package("nope");
    });
    let err = expand_err(&mut raw, ws.path());
    assert!(
        err.contains("cannot run the tests of a proc-macro package"),
        "{err}"
    );
    assert!(err.contains("`nope` is not a member"), "{err}");
}

/// Member `fw` with a lib, the MCU-only bin `fw` and the test `api`,
/// both with `test = false`, the way firmware crates opt their bins out
/// of `cargo test`.
fn firmware_workspace(lib: &str) -> tempfile::TempDir {
    let fw = manifest(
        "fw",
        &format!(
            "{lib}[[bin]]\nname = \"fw\"\ntest = false\n[[test]]\nname = \"api\"\ntest = false\n"
        ),
    );
    workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"fw\"]\n",
        ),
        ("fw/Cargo.toml", &fw),
        ("fw/src/lib.rs", ""),
        ("fw/src/main.rs", ""),
        ("fw/tests/api.rs", ""),
    ])
}

fn host_test_fw() -> RawBuildModel {
    raw_model(|q, _| {
        q.cargo_workspace(".");
        q.host_tests().package("fw");
    })
}

/// `test = false` leaves a target out of the host tests, as `cargo
/// test` leaves it out: only the lib's unit tests build.
#[test]
fn host_tests_leave_out_targets_with_test_false() {
    let ws = firmware_workspace("");
    let mut raw = host_test_fw();
    let warnings = expand_at(&mut raw, ws.path()).expect("expand");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        dump(&raw),
        "\
fw [cargo:host-test] host Lib fw/src/lib.rs 2021 f=[] d=[] rustc=fw
fw@test [cargo:host-test] host Test fw/src/lib.rs 2021 f=[] d=[] rustc=fw
"
    );

    // `[lib] test = false` drops the lib test, and with it the
    // doctests, which Quon says unless `doctest = false` agrees.
    let ws = firmware_workspace("[lib]\ntest = false\n");
    let mut raw = host_test_fw();
    let warnings = expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(dump(&raw), "");
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("so Quon skips its doctests too")),
        "{warnings:?}"
    );
    let ws = firmware_workspace("[lib]\ntest = false\ndoctest = false\n");
    let mut raw = host_test_fw();
    let warnings = expand_at(&mut raw, ws.path()).expect("expand");
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// A `harness = false` lib or bin under test would build with libtest
/// anyway, so it is an error that names the way out.
#[test]
fn host_tests_reject_a_lib_or_bin_without_a_harness() {
    let ws = workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\"]\n",
        ),
        (
            "a/Cargo.toml",
            &manifest(
                "a",
                "[lib]\nharness = false\n[[bin]]\nname = \"tool\"\npath = \"src/main.rs\"\n\
                 harness = false\n",
            ),
        ),
        ("a/src/lib.rs", ""),
        ("a/src/main.rs", ""),
    ]);
    let mut raw = raw_model(|q, _| {
        q.cargo_workspace(".");
        q.host_tests().package("a");
    });
    let err = expand_err(&mut raw, ws.path());
    assert!(
        err.contains("the lib of package `a` sets `harness = false`"),
        "{err}"
    );
    assert!(
        err.contains("bin `tool` of package `a` sets `harness = false`"),
        "{err}"
    );
    assert!(err.contains("set `test = false` or remove"), "{err}");
}

/// Boards `ba` and `bb` turn on chip features of `hal` that exclude
/// each other, through their `host` features. `proto` depends on `hal`
/// without a chip, and `util` is shared by all three.
fn chip_workspace() -> tempfile::TempDir {
    let board = |chip: &str| {
        format!(
            "[features]\nhost = [\"hal/{chip}\"]\n\
             [dependencies]\nhal = {{ path = \"../hal\" }}\nutil = {{ path = \"../util\" }}\n"
        )
    };
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"ba\", \"bb\", \"proto\"]\n".to_string(),
        ),
        ("ba/Cargo.toml", manifest("ba", &board("chip-a"))),
        ("bb/Cargo.toml", manifest("bb", &board("chip-b"))),
        (
            "proto/Cargo.toml",
            manifest(
                "proto",
                "[dependencies]\nhal = { path = \"../hal\" }\nutil = { path = \"../util\" }\n",
            ),
        ),
        (
            "hal/Cargo.toml",
            manifest("hal", "[features]\nchip-a = []\nchip-b = []\n"),
        ),
        ("util/Cargo.toml", manifest("util", "")),
    ];
    let mut files: Vec<(String, String)> = files
        .iter()
        .map(|(p, c)| (p.to_string(), c.clone()))
        .collect();
    for p in ["ba", "bb", "proto", "hal", "util"] {
        files.push((format!("{p}/src/lib.rs"), String::new()));
    }
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    workspace(&files)
}

/// Each package given features resolves on its own, so `hal` builds
/// once per chip and once for `proto`, without a chip. `util` has the
/// same features and deps in all three resolutions, so it builds once.
#[test]
fn host_test_packages_with_features_resolve_on_their_own() {
    let ws = chip_workspace();
    let mut raw = raw_model(|q, _| {
        q.cargo_workspace(".");
        q.host_tests()
            .package("proto")
            .package_with_features("ba", ["host"])
            .package_with_features("bb", ["host"]);
    });
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        dump(&raw),
        "\
ba [cargo:host-test] host Lib ba/src/lib.rs 2021 f=[host] d=[hal->hal@host-test#1,util->util] rustc=ba
ba@test [cargo:host-test] host Test ba/src/lib.rs 2021 f=[host] d=[hal->hal@host-test#1,util->util] rustc=ba
bb [cargo:host-test] host Lib bb/src/lib.rs 2021 f=[host] d=[hal->hal@host-test#2,util->util] rustc=bb
bb@test [cargo:host-test] host Test bb/src/lib.rs 2021 f=[host] d=[hal->hal@host-test#2,util->util] rustc=bb
hal@host-test [cargo:host-test] host Lib hal/src/lib.rs 2021 f=[] d=[] rustc=hal
hal@host-test#1 [cargo:host-test] host Lib hal/src/lib.rs 2021 f=[chip-a] d=[] rustc=hal
hal@host-test#2 [cargo:host-test] host Lib hal/src/lib.rs 2021 f=[chip-b] d=[] rustc=hal
proto [cargo:host-test] host Lib proto/src/lib.rs 2021 f=[] d=[hal->hal@host-test,util->util] rustc=proto
proto@test [cargo:host-test] host Test proto/src/lib.rs 2021 f=[] d=[hal->hal@host-test,util->util] rustc=proto
util [cargo:host-test] host Lib util/src/lib.rs 2021 f=[] d=[] rustc=util
"
    );
}

/// A git dependency, here of the workspace's `tool` (the way a repository's
/// `xtask` depends on its build tools), does not stop Quon building the other
/// members. Building `tool` itself reports it.
#[test]
fn git_dependencies_matter_only_to_the_packages_that_are_built() {
    let ws = workspace(&[
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"tool\"]\n\
             [workspace.dependencies]\ngen = { git = \"https://example.com/gen\" }\n",
        ),
        ("a/Cargo.toml", &manifest("a", "")),
        ("a/src/lib.rs", ""),
        ("a/src/main.rs", ""),
        (
            "tool/Cargo.toml",
            &manifest("tool", "[dependencies]\ngen = { workspace = true }\n"),
        ),
        ("tool/src/main.rs", ""),
    ]);
    let mut raw = root_a();
    expand_at(&mut raw, ws.path()).expect("a does not use the git dependency");

    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("tool").target(t));
    });
    let err = expand_err(&mut raw, ws.path());
    assert!(
        err.contains("dependency `gen` of `tool`: git dependency `gen` is not supported yet"),
        "{err}"
    );
}

#[test]
fn host_tests_need_a_workspace() {
    let mut raw = raw_model(|q, _| {
        q.host_tests().package("a");
    });
    let err = expand_err(&mut raw, Path::new("/nonexistent"));
    assert!(
        err.contains("`host_tests().package(\"a\")` needs a Cargo workspace"),
        "{err}"
    );
}

/// The build script's `util` and the host tests' `util` have the same
/// features and deps here, so only the domain tells them apart. They
/// must still get different hashes, or the scheduler builds both into
/// one file in parallel.
#[test]
fn host_test_copies_never_share_an_output_with_the_build() {
    let a = manifest(
        "a",
        "build = \"build.rs\"\n\
         [dependencies]\nutil = { path = \"../util\" }\n\
         [build-dependencies]\nutil = { path = \"../util\" }\n",
    );
    let files = [
        ("Cargo.toml", WS.to_string()),
        ("a/Cargo.toml", a),
        ("a/build.rs", "fn main() {}".into()),
        ("a/src/lib.rs", String::new()),
        ("util/Cargo.toml", manifest("util", "")),
        ("util/src/lib.rs", String::new()),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    let ws = workspace(&files);
    let raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        q.add(Package::new("a").target(t));
        q.host_tests().package("a");
    });
    let units = units_at(raw, ws.path());
    let build = unit_named(&units, "util@host");
    let test = unit_named(&units, "util@host-test");
    assert_eq!(build.features, test.features);
    assert_ne!(build.metadata, test.metadata);
}

/// A host-test lib test names the plain lib its doctests link, in the
/// host-test domain, unless the lib says `doctest = false`. The plain
/// lib is lowered even when no `tests/` file links it.
#[test]
fn host_test_lib_tests_name_their_doctest_lib() {
    let doctest_lib = |raw: &RawBuildModel| -> Vec<(String, Option<String>)> {
        raw.crates
            .iter()
            .filter_map(|(_, c)| {
                let cargo = c.cargo.as_ref()?;
                (c.crate_type == CrateType::Test)
                    .then(|| (c.name.clone(), cargo.doctest_lib.clone()))
            })
            .collect()
    };
    let ws = host_test_workspace();
    let mut raw = root_a_with_host_tests();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        doctest_lib(&raw),
        [
            ("a@test".to_string(), Some("a@host-test@lib".to_string())),
            ("a@bin-test".to_string(), None),
            ("api".to_string(), None),
        ]
    );

    std::fs::remove_file(ws.path().join("a/tests/api.rs")).unwrap();
    let mut raw = root_a_with_host_tests();
    expand_at(&mut raw, ws.path()).expect("expand");
    // Without `tests/` files no bin builds, so the lib keeps the plain
    // host-test name.
    assert_eq!(
        doctest_lib(&raw),
        [
            ("a@test".to_string(), Some("a@host-test".to_string())),
            ("a@bin-test".to_string(), None),
        ]
    );
    assert!(raw.crates.iter().any(|(_, c)| c.name == "a@host-test"));

    let manifest_path = ws.path().join("a/Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(&manifest_path, format!("{text}[lib]\ndoctest = false\n")).unwrap();
    let mut raw = root_a_with_host_tests();
    expand_at(&mut raw, ws.path()).expect("expand");
    assert_eq!(
        doctest_lib(&raw),
        [
            ("a@test".to_string(), None),
            ("a@bin-test".to_string(), None)
        ]
    );
}

/// With a run binary, a profile builds what the target's crates reach,
/// and the host tests too, which nothing reaches.
#[test]
fn host_tests_build_under_a_run_binary() {
    let ws = host_test_workspace();
    let mut raw = raw_model(|q, t| {
        q.cargo_workspace(".");
        let a = q.add(Package::new("a").target(t));
        q.run(&a);
        q.host_tests().package("a");
    });
    expand_at(&mut raw, ws.path()).expect("expand");
    let (model, diags) = quon_model::intern(raw);
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = crate::config::resolve(&model, "dev", None, ws.path(), None).unwrap();
    let names: BTreeSet<String> = resolved
        .crates
        .iter()
        .map(|c| model.crates.get(c.handle).unwrap().name.clone())
        .collect();
    for name in [
        "a@test",
        "a@bin-test",
        "api",
        "a@host-test",
        "a@host-test@lib",
        "kit@host-test",
        "util@host-test",
    ] {
        assert!(names.contains(name), "{name} missing from {names:?}");
    }
}
