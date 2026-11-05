use super::*;
use crate::compile::{ArtifactMap, BuildLayout, CompileCtx, CompileUnitInput, DriverKind};
use crate::manifest::platform::TargetPlatform;
use gluon_config::Config;
use gluon_model::{BuildModel, Handle, ResolvedConfig, TargetDef};
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

/// A raw model for `root` with one target, `x86_64-unknown-none`.
fn raw_model(build: impl FnOnce(&mut Config, Handle<TargetDef>)) -> RawBuildModel {
    let mut cfg = Config::new();
    cfg.project("demo", "0.1.0");
    let t = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(t);
    build(&mut cfg, t);
    assert!(cfg.diagnostics().is_empty(), "{:?}", cfg.diagnostics());
    cfg.into_raw()
}

/// The fixture's own gluon.rs, in-process.
fn fixture_raw() -> RawBuildModel {
    raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("kernel")
            .target(t)
            .config(true)
            .package("kernel")
            .linker_script("crates/kernel/kernel.ld");
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
    assert_eq!(env["CARGO_PKG_AUTHORS"], "Gluon Tests:Second Author");
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
    let mut raw = raw_model(|cfg, t| {
        cfg.group("k").target(t).add("kernel", "crates/kernel");
    });
    let before = serde_json::to_string(&raw).unwrap();
    // A directory without Cargo.toml: nothing may be read.
    let empty = tempfile::tempdir().unwrap();
    expand_at(&mut raw, empty.path()).unwrap();
    assert_eq!(serde_json::to_string(&raw).unwrap(), before);
}

#[test]
fn root_features_and_raw_binary_apply() {
    let mut raw = raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("kernel")
            .target(t)
            .package("kernel")
            .no_default_features()
            .features(&["unused"])
            .crate_type(CrateType::RawBinary);
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
    raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("k").target(t).package("a");
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
    raw_model(|cfg, t| {
        cfg.cargo_workspace(".").vendored("vendor");
        cfg.group("k").target(t).package("a");
    })
}

/// The units of `raw` expanded at `root`.
fn units_at(mut raw: RawBuildModel, root: &Path) -> crate::unit::UnitGraph {
    expand_at(&mut raw, root).expect("expand");
    let (model, diags) = gluon_model::intern(raw);
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
/// Gluon, though it drops a registry package's dev-dependencies.
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
        err.contains("Cargo.lock changed since the last `gluon vendor`"),
        "{err}"
    );
}

#[test]
fn vendored_mode_reports_a_missing_package_directory() {
    let ws = vendored_workspace("abc");
    std::fs::remove_dir_all(ws.path().join("vendor/flags-1.2.0")).unwrap();
    let err = expand_err(&mut root_a_vendored(), ws.path());
    assert!(
        err.contains("source of `flags 1.2.0` is missing") && err.contains("run `gluon vendor`"),
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

/// The script's build-dep is reachable from the boot binary through
/// the script alone, so it is in the build.
fn scripted_raw() -> RawBuildModel {
    let mut cfg = Config::new();
    cfg.project("demo", "0.1.0");
    let t = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(t).boot_binary("a");
    cfg.cargo_workspace(".");
    cfg.group("k").target(t).package("a");
    assert!(cfg.diagnostics().is_empty(), "{:?}", cfg.diagnostics());
    cfg.into_raw()
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
    let mut raw = raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("k")
            .target(t)
            .package("a")
            .crate_type(CrateType::RawBinary);
    });
    let err = expand_err(&mut raw, ws.path());
    assert!(
        err.contains(
            "package root 'a' has crate type RawBinary in gluon.rs, but package `a` has no bin \
             target"
        ),
        "{err}"
    );
    assert!(err.contains("a/Cargo.toml"), "{err}");
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
        let (model, _) = gluon_model::intern(raw);
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
    let mut raw = raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("k").target(t).package("nope");
    });
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains("package `nope` is not a member of the Cargo workspace at"),
        "{err}"
    );
}

#[test]
fn package_without_workspace_is_rejected() {
    let mut raw = raw_model(|cfg, t| {
        cfg.group("k").target(t).package("kernel");
    });
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains("`package(\"kernel\")` needs a Cargo workspace"),
        "{err}"
    );
}

#[test]
fn declared_crate_overlapping_the_workspace_is_rejected() {
    let mut raw = raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        let mut g = cfg.group("k").target(t);
        g.package("kernel");
        g.add("loader", "crates/loader").dep("util", "util");
        g.add("again", "./crates/util/");
        g.add("util-helpers", "elsewhere");
    });
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains(
            "crate 'loader' (declared with `group.add`) depends on 'util' (as `util`), which \
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
            "crate 'util-helpers' is declared with `group.add`, and package `util-helpers` in \
             the Cargo workspace has a unit of the same name"
        ),
        "{err}"
    );
}

#[test]
fn no_sysroot_root_with_dependencies_is_rejected() {
    let mut raw = raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("k").target(t).no_sysroot().package("kernel");
    });
    let err = expand_err(&mut raw, &fixture());
    assert!(
        err.contains("package `kernel` is in `no_sysroot` group 'k' but has dependencies"),
        "{err}"
    );
}

// ---------------------------------------------------------------------
// Through intern, unit lowering and the command builder
// ---------------------------------------------------------------------

fn fixture_model() -> (BuildModel, ResolvedConfig) {
    let mut raw = fixture_raw();
    expand_at(&mut raw, &fixture()).expect("expand");
    let (model, diags) = gluon_model::intern(raw);
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
    let mut raw = raw_model(|cfg, t| {
        cfg.cargo_workspace(".");
        cfg.group("kernel")
            .target(t)
            .package("kernel")
            .cfg_flag("kernel_debug")
            .cfg_flag("mode=\"fast\"");
    });
    expand_at(&mut raw, &fixture()).unwrap();
    let (model, _) = gluon_model::intern(raw);
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
