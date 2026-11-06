//! Fixture tests for Cargo manifest loading.
//!
//! Each fixture under `tests/fixtures/cargo-*` holds a Cargo workspace
//! and `expected/*.txt` files. Set `QUON_UPDATE_EXPECT=1` to rewrite
//! the expected files from the current output, then review the diff.

use quon_core::manifest::sources::RegistrySources;
use quon_core::manifest::{dump_packages, workspace::load_workspace};
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn vendored(dir: &Path, vendor: &str) -> RegistrySources {
    RegistrySources::vendor_dir(dir, &dir.join(vendor)).expect("read Cargo.lock")
}

fn check_expected(path: &Path, actual: &str) {
    if std::env::var_os("QUON_UPDATE_EXPECT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e} (set QUON_UPDATE_EXPECT=1)", path.display()));
    assert_eq!(
        actual,
        expected,
        "output differs from {} (set QUON_UPDATE_EXPECT=1 to update)",
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

/// A member manifest that is not valid TOML fails the load with its
/// line and column. The workspace is written at test time: Cargo reads
/// every `Cargo.toml` in a git checkout when quon is a git dependency,
/// and a checked-in broken manifest makes it print an error to every
/// project that depends on quon.
#[test]
fn a_member_manifest_with_a_toml_error_names_its_line_and_column() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"a\"]\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::write(
        dir.path().join("a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = 2021\n",
    )
    .unwrap();
    let err = match load_workspace(dir.path(), &vendored(dir.path(), "vendor")) {
        Ok(_) => panic!("a manifest with a TOML error loaded"),
        Err(e) => e.to_string(),
    };
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        err.replace(&format!("{}/", canonical.display()), ""),
        "a/Cargo.toml:4:11: error: invalid manifest: \
         data did not match any variant of untagged enum Inheritable"
    );
}

// ---------------------------------------------------------------------
// Feature resolution
// ---------------------------------------------------------------------

use quon_core::RustcInfo;
use quon_core::manifest::platform::{self, ProbeTarget, TargetPlatform};
use quon_core::manifest::resolve::{Domain, Root, UnitGraph, UnitKind, dump_units, resolve};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// One resolution of a fixture: roots for one target.
struct Case {
    fixture: &'static str,
    /// Names `expected/<label>.txt`.
    label: &'static str,
    target: Target,
    roots: &'static [&'static str],
    features: &'static [&'static str],
    no_default_features: bool,
}

#[derive(Clone, Copy)]
enum Target {
    Builtin(&'static str),
    /// A custom spec named `<stem>.json`, generated from
    /// x86_64-unknown-none because the spec schema changes between
    /// nightlies.
    GeneratedSpec(&'static str),
}

const fn case(fixture: &'static str, roots: &'static [&'static str]) -> Case {
    Case {
        fixture,
        label: "x86_64-unknown-none",
        target: Target::Builtin("x86_64-unknown-none"),
        roots,
        features: &[],
        no_default_features: false,
    }
}

const CASES: &[Case] = &[
    case("cargo-inherit", &["app"]),
    case("cargo-rename", &["app"]),
    case("cargo-host-split", &["app"]),
    case("cargo-platform", &["plat"]),
    Case {
        label: "x86_64-custom",
        target: Target::GeneratedSpec("x86_64-custom"),
        ..case("cargo-platform", &["plat"])
    },
    Case {
        label: "default",
        ..case("cargo-features", &["root"])
    },
    Case {
        label: "extra",
        features: &["viaslash", "weak", "cyc-a", "use-hidden"],
        ..case("cargo-features", &["root"])
    },
    Case {
        label: "no-default",
        no_default_features: true,
        ..case("cargo-features", &["root"])
    },
];

/// A case resolved, with what the oracle needs to rerun it in Cargo.
struct Resolved {
    graph: UnitGraph,
    /// `--target` argument for Cargo.
    target_arg: PathBuf,
    _spec_dir: Option<tempfile::TempDir>,
}

/// Whether `c` can run with this rustc: a generated spec needs
/// `--print target-spec-json`, which only nightly accepts.
fn runs_here(info: &RustcInfo, c: &Case) -> bool {
    let nightly = info.release.contains("nightly") || info.release.contains("dev");
    if !nightly && matches!(c.target, Target::GeneratedSpec(_)) {
        eprintln!(
            "skipping {} {}: custom target specs need nightly",
            c.fixture, c.label
        );
        return false;
    }
    true
}

fn resolve_case(info: &RustcInfo, c: &Case) -> Resolved {
    let dir = fixtures().join(c.fixture);
    let set = load_workspace(&dir, &vendored(&dir, "vendor")).expect("load");
    let (probe, target_arg, spec_dir) = match c.target {
        Target::Builtin(t) => (ProbeTarget::Builtin(t.into()), PathBuf::from(t), None),
        Target::GeneratedSpec(stem) => {
            let tmp = tempfile::tempdir().unwrap();
            let out = Command::new(&info.rustc_path)
                .args(["-Zunstable-options", "--print", "target-spec-json"])
                .arg("--target=x86_64-unknown-none")
                .output()
                .unwrap();
            assert!(out.status.success(), "target-spec-json needs nightly rustc");
            let spec = tmp.path().join(format!("{stem}.json"));
            std::fs::write(&spec, &out.stdout).unwrap();
            (ProbeTarget::Spec(spec.clone()), spec, Some(tmp))
        }
    };
    let target = platform::probe(info, &probe, None).expect("probe target");
    let host = platform::probe(info, &ProbeTarget::Host, None).expect("probe host");
    let domain = Domain::Target(target.name.clone());
    let platforms: BTreeMap<Domain, TargetPlatform> =
        [(Domain::Host, host), (domain.clone(), target)].into();
    let roots: Vec<Root> = c
        .roots
        .iter()
        .map(|r| Root {
            package: set.member(r).unwrap_or_else(|| panic!("no member {r}")),
            domain: domain.clone(),
            features: c.features.iter().map(|s| s.to_string()).collect(),
            default_features: !c.no_default_features,
        })
        .collect();
    let graph = resolve(&set, &roots, &[], &platforms, &info.host_triple).expect("resolve");
    Resolved {
        graph,
        target_arg,
        _spec_dir: spec_dir,
    }
}

#[test]
fn resolver_fixtures() {
    let Ok(info) = RustcInfo::probe() else {
        eprintln!("skipping: no rustc");
        return;
    };
    for c in CASES.iter().filter(|c| runs_here(&info, c)) {
        let r = resolve_case(&info, c);
        let path = fixtures()
            .join(c.fixture)
            .join(format!("expected/{}.txt", c.label));
        check_expected(&path, &dump_units(&r.graph));
    }
}

#[test]
fn resolution_is_deterministic() {
    let Ok(info) = RustcInfo::probe() else {
        return;
    };
    for c in CASES.iter().filter(|c| runs_here(&info, c)) {
        let a = dump_units(&resolve_case(&info, c).graph);
        let b = dump_units(&resolve_case(&info, c).graph);
        assert_eq!(a, b, "{} {}", c.fixture, c.label);
        let hashes = |g: &UnitGraph| -> Vec<String> {
            g.units.iter().map(|u| u.unit_hash.clone()).collect()
        };
        assert_eq!(
            hashes(&resolve_case(&info, c).graph),
            hashes(&resolve_case(&info, c).graph)
        );
    }
}

#[test]
fn unknown_root_feature_is_reported() {
    let Ok(info) = RustcInfo::probe() else {
        return;
    };
    let dir = fixtures().join("cargo-features");
    let set = load_workspace(&dir, &vendored(&dir, "vendor")).unwrap();
    let host = platform::probe(&info, &ProbeTarget::Host, None).unwrap();
    let platforms: BTreeMap<Domain, TargetPlatform> = [(Domain::Host, host)].into();
    let root = Root {
        package: set.member("root").unwrap(),
        domain: Domain::Host,
        features: vec!["nope".into(), "lib/missing".into()],
        default_features: true,
    };
    let err = resolve(&set, &[root], &[], &platforms, &info.host_triple)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("package `root` has no feature `nope`"),
        "{err}"
    );
    assert!(
        err.contains("package `lib` has no feature `missing`"),
        "{err}"
    );
    assert!(err.contains("required by root -> lib in the host"), "{err}");
}

/// An edge that the resolver activates and that has no package is an
/// error; Cargo.lock and the sources are only needed for active edges.
#[test]
fn activated_unresolved_edge_is_reported() {
    let Ok(info) = RustcInfo::probe() else {
        return;
    };
    let dir = fixtures().join("cargo-rename");
    let set = load_workspace(&dir, &vendored(&dir, "no-such-vendor")).unwrap();
    let host = platform::probe(&info, &ProbeTarget::Host, None).unwrap();
    let platforms: BTreeMap<Domain, TargetPlatform> = [(Domain::Host, host)].into();
    let root = Root {
        package: set.member("app").unwrap(),
        domain: Domain::Host,
        features: vec![],
        default_features: true,
    };
    let err = resolve(&set, &[root], &[], &platforms, &info.host_triple)
        .unwrap_err()
        .to_string();
    assert!(err.contains("dependency `fake1` of `app`"), "{err}");
}

// ---------------------------------------------------------------------
// Differential oracle against `cargo build --unit-graph`
// ---------------------------------------------------------------------

/// (package, version, host or target, unit kind) → features and
/// `extern -> dep key` edges. Build-script run units are left out: they
/// have no Quon counterpart in the graph (they become a `BuildScriptRun`
/// on the consuming unit).
type UnitKey = (String, String, &'static str, String);
type Comparable = BTreeSet<(UnitKey, Vec<String>, Vec<(String, UnitKey)>)>;

fn quon_comparable(g: &UnitGraph) -> Comparable {
    let key = |u: &quon_core::manifest::Unit| -> UnitKey {
        let p = &g.packages[u.package];
        let kind = match &u.kind {
            UnitKind::Lib => "lib".to_string(),
            UnitKind::ProcMacro => "proc-macro".to_string(),
            UnitKind::Bin(n) => format!("bin:{n}"),
            UnitKind::BuildScript => "build-script".to_string(),
            other => other.to_string(),
        };
        let place = if u.domain == Domain::Host {
            "host"
        } else {
            "target"
        };
        (p.id.name.clone(), p.id.version.to_string(), place, kind)
    };
    g.units
        .iter()
        .map(|u| {
            let deps = u
                .deps
                .iter()
                .map(|d| (d.extern_name.clone(), key(&g.units[d.unit])))
                .collect();
            (key(u), u.features.iter().cloned().collect(), deps)
        })
        .collect()
}

fn cargo_comparable(json: &str) -> Comparable {
    let v: serde_json::Value = serde_json::from_str(json).expect("unit graph json");
    let units = v["units"].as_array().expect("units");
    let key = |u: &serde_json::Value| -> Option<UnitKey> {
        if u["mode"] != "build" {
            return None;
        }
        // pkg_id: "path+file:///...#name@1.0.0", "path+file:///.../name#1.0.0"
        // or "registry+...#name@1.0.0".
        let pkg_id = u["pkg_id"].as_str()?;
        let frag = pkg_id.rsplit('#').next()?;
        let (name, version) = match frag.split_once('@') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => {
                let path = pkg_id.split('#').next()?;
                (path.rsplit('/').next()?.to_string(), frag.to_string())
            }
        };
        let kinds: Vec<&str> = u["target"]["kind"]
            .as_array()?
            .iter()
            .filter_map(|k| k.as_str())
            .collect();
        let kind = if kinds.contains(&"proc-macro") {
            "proc-macro".to_string()
        } else if kinds.contains(&"custom-build") {
            "build-script".to_string()
        } else if kinds.contains(&"bin") {
            format!("bin:{}", u["target"]["name"].as_str()?)
        } else {
            "lib".to_string()
        };
        let place = if u["platform"].is_null() {
            "host"
        } else {
            "target"
        };
        Some((name, version, place, kind))
    };
    units
        .iter()
        .filter_map(|u| {
            let k = key(u)?;
            let mut features: Vec<String> = u["features"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_string())
                .collect();
            features.sort();
            let mut deps: Vec<(String, UnitKey)> = u["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|d| {
                    let target = &units[d["index"].as_u64()? as usize];
                    Some((d["extern_crate_name"].as_str()?.to_string(), key(target)?))
                })
                .collect();
            deps.sort();
            Some((k, features, deps))
        })
        .collect()
}

fn copy_tree(src: &Path, dst: &Path) {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.unwrap();
        let rel = entry.path().strip_prefix(src).unwrap();
        let out = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&out).unwrap();
        } else {
            std::fs::copy(entry.path(), &out).unwrap();
        }
    }
}

/// Runs each case through `cargo build --unit-graph` and compares
/// features and extern edges per unit with Quon's graph. Needs nightly
/// cargo, so it runs only with `QUON_CARGO_ORACLE=1`. No network: the
/// fixtures are path-only or carry their vendored sources.
#[test]
fn cargo_unit_graph_oracle() {
    if std::env::var_os("QUON_CARGO_ORACLE").is_none() {
        eprintln!("skipping: set QUON_CARGO_ORACLE=1 to compare with cargo --unit-graph");
        return;
    }
    let info = RustcInfo::probe().expect("rustc");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    for c in CASES {
        let r = resolve_case(&info, c);
        // Cargo writes Cargo.lock and target/ next to the workspace, so
        // run it on a copy.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        copy_tree(&fixtures().join(c.fixture), &ws);
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&ws)
            .args(["build", "--unit-graph", "-Zunstable-options", "--offline"])
            .arg("--target")
            .arg(&r.target_arg);
        if matches!(c.target, Target::GeneratedSpec(_)) {
            cmd.arg("-Zjson-target-spec");
        }
        for root in c.roots {
            cmd.args(["-p", root]);
        }
        if !c.features.is_empty() {
            cmd.arg("--features").arg(c.features.join(","));
        }
        if c.no_default_features {
            cmd.arg("--no-default-features");
        }
        if ws.join("Cargo.lock").exists() {
            cmd.arg("--locked");
        }
        if ws.join("vendor").is_dir() {
            cmd.args([
                "--config",
                "source.crates-io.replace-with=\"fixture-vendor\"",
                "--config",
                "source.fixture-vendor.directory=\"vendor\"",
            ]);
        }
        let out = cmd.output().expect("spawn cargo");
        assert!(
            out.status.success(),
            "{} {}: cargo failed: {}",
            c.fixture,
            c.label,
            String::from_utf8_lossy(&out.stderr)
        );
        let cargo_units = cargo_comparable(&String::from_utf8(out.stdout).unwrap());
        assert!(
            !cargo_units.is_empty(),
            "{} {}: empty unit graph",
            c.fixture,
            c.label
        );
        let quon_units = quon_comparable(&r.graph);
        let only_cargo: Vec<_> = cargo_units.difference(&quon_units).collect();
        let only_quon: Vec<_> = quon_units.difference(&cargo_units).collect();
        assert!(
            only_cargo.is_empty() && only_quon.is_empty(),
            "{} {}: resolution differs from cargo\nonly in cargo: {only_cargo:#?}\nonly in quon: {only_quon:#?}",
            c.fixture,
            c.label
        );
    }
}

#[test]
fn build_script_env_carries_target_cfgs_and_features() {
    let Ok(info) = RustcInfo::probe() else {
        return;
    };
    let r = resolve_case(&info, &case("cargo-host-split", &["app"]));
    let lib = r
        .graph
        .units
        .iter()
        .find(|u| u.kind == UnitKind::Lib && r.graph.packages[u.package].name() == "app")
        .unwrap();
    let run = lib.build_script.as_ref().expect("app has a build script");
    assert_eq!(r.graph.units[run.script].kind, UnitKind::BuildScript);
    assert_eq!(r.graph.units[run.script].domain, Domain::Host);
    let env = &run.env;
    assert_eq!(env["TARGET"], "x86_64-unknown-none");
    assert_eq!(env["HOST"], info.host_triple);
    assert_eq!(env["CARGO_CFG_TARGET_OS"], "none");
    assert_eq!(env["CARGO_PKG_NAME"], "app");
    assert_eq!(env["CARGO_PKG_VERSION_MINOR"], "1");
    assert!(!env.contains_key("CARGO_CFG_DEBUG_ASSERTIONS"));
    assert!(!env.keys().any(|k| k.starts_with("CARGO_FEATURE_")));
}
