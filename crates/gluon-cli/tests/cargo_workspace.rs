//! End-to-end builds of projects whose crates come from a Cargo
//! workspace (`cfg.cargo_workspace` and `group.package`): the
//! `cargo-build` and `cargo-registry` fixtures and every example under
//! `examples/`.
//!
//! Each test copies its project into a temp dir, seeds its sysroot (see
//! `support::seed_sysroot`) and runs the real `gluon` binary there.
//! They spawn rustc, so they are `#[ignore]`-gated like the other build
//! tests in `integration.rs`.

mod support;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{copy_dir_all, rustc_available, seed_sysroot, setup, summary};

/// Run `gluon build` in `project`; panic on failure, return stderr.
fn build_ok(project: &Path) -> String {
    gluon_ok(project, &["build"], &[]).1
}

/// Builds, then caches everything. A feature change in one manifest
/// then rebuilds that package's units and their dependents, and nothing
/// else: the feature reaches the argv, and the lib's new metadata hash
/// changes the bin's `--extern`.
#[test]
#[ignore]
fn cargo_build_fixture_builds_then_caches() {
    if !rustc_available("cargo_build_fixture_builds_then_caches") {
        return;
    }
    let (_tmp, project) = setup("tests/fixtures/cargo-build");

    let seeded = seed_sysroot(&project);
    let first = build_ok(&project);
    let (built, cached) = summary(&first);
    assert!(built > 0, "{first}");
    assert_eq!(cached, seeded, "{first}");
    assert!(
        project
            .join("build/cross/x86_64-unknown-none/dev/final/kernel")
            .is_file(),
        "kernel binary missing"
    );

    let second = build_ok(&project);
    assert_eq!(summary(&second), (0, built + seeded), "{second}");

    let manifest = project.join("crates/kernel/Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let edited = text.replace("default = [\"log\"]", "default = []");
    assert_ne!(text, edited, "fixture manifest changed shape");
    fs::write(&manifest, edited).unwrap();

    let stderr = build_ok(&project);
    // kernel@lib and the kernel bin.
    assert_eq!(summary(&stderr).0, 2, "{stderr}");
}

/// `example` builds from a clean tree. One test per example, so they
/// build in parallel.
fn example_builds(example: &str) {
    if !rustc_available(example) {
        return;
    }
    let (_tmp, project) = setup(&format!("examples/{example}"));
    let seeded = seed_sysroot(&project);
    let stderr = build_ok(&project);
    assert_eq!(summary(&stderr).1, seeded, "{example}: {stderr}");
}

#[test]
#[ignore]
fn example_basic_kernel_builds() {
    example_builds("basic-kernel");
}

#[test]
#[ignore]
fn example_with_config_builds() {
    example_builds("with-config");
}

#[test]
#[ignore]
fn example_uefi_with_bootloader_builds() {
    example_builds("uefi-with-bootloader");
}

// ---------------------------------------------------------------------
// Registry dependencies (`tests/fixtures/cargo-registry`)
// ---------------------------------------------------------------------

/// Run `gluon <args>` in `project` with `env` set on the child only.
fn gluon(project: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gluon"));
    cmd.args(args).current_dir(project);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn gluon")
}

/// [`gluon`] that must succeed; returns stdout and stderr.
fn gluon_ok(project: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> (String, String) {
    let out = gluon(project, args, env);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "gluon {args:?} failed:\n{stdout}\n{stderr}"
    );
    (stdout, stderr)
}

/// `$CARGO` for a child that must not run Cargo: any spawn fails.
const NO_CARGO: &str = "/nonexistent/cargo";

/// Switch the fixture's gluon.rs to read registry sources from
/// `vendor/`.
fn use_vendor_dir(project: &Path) {
    let path = project.join("gluon.rs");
    let text = fs::read_to_string(&path).unwrap();
    let edited = text.replace(
        "cfg.cargo_workspace(\".\");",
        "cfg.cargo_workspace(\".\").vendored(\"vendor\");",
    );
    assert_ne!(text, edited, "fixture gluon.rs changed shape");
    fs::write(path, edited).unwrap();
}

fn append(path: &Path, text: &str) {
    let mut s = fs::read_to_string(path).unwrap();
    s.push_str(text);
    fs::write(path, s).unwrap();
}

/// Point Cargo at the fixture's `registry/` directory source in place of
/// crates.io, through a `.cargo/config.toml` in the project.
fn use_fixture_registry(project: &Path) {
    fs::create_dir_all(project.join(".cargo")).unwrap();
    fs::write(
        project.join(".cargo/config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n\
             [source.fixture]\ndirectory = '{}'\n",
            project.join("registry").display()
        ),
    )
    .unwrap();
}

/// `gluon manifest` reads registry sources where a build does: from
/// Cargo's registry cache when no vendor directory is named.
#[test]
#[ignore]
fn manifest_units_reads_registry_sources_like_a_build() {
    if !rustc_available("manifest_units_reads_registry_sources_like_a_build") {
        return;
    }
    // `gluon manifest` builds nothing, so it needs no sysroot.
    let (_tmp, project) = support::setup("tests/fixtures/cargo-registry");
    use_fixture_registry(&project);
    let (units, _) = gluon_ok(&project, &["manifest", "units"], &[]);
    assert!(units.contains("fakeflags"), "{units}");
}

/// Offline: a `.cargo/config.toml` in the project replaces crates.io
/// with the fixture's `registry/` directory source, so `cargo fetch` and
/// `cargo metadata` work without the network. (A Cargo home config
/// would also redirect the cargo build of `gluon-config` that compiles
/// `gluon.rs`.) Covers both source modes:
/// registry (Cargo asked once, then the cached map) and a vendor
/// directory (no Cargo at all, sources stat'ed, stamp checked).
#[test]
#[ignore]
fn registry_dependency_builds_from_cargo_sources_then_vendor_dir() {
    if !rustc_available("registry_dependency_builds_from_cargo_sources") {
        return;
    }
    let (_tmp, project) = setup("tests/fixtures/cargo-registry");
    use_fixture_registry(&project);
    let without_cargo = [("CARGO", OsStr::new(NO_CARGO))];

    // Seed once Cargo can resolve the workspace: seeding loads the
    // model, which runs `cargo fetch`.
    let seeded = seed_sysroot(&project);
    let (_, first) = gluon_ok(&project, &["build"], &[]);
    let (built, cached) = summary(&first);
    assert!(built >= 2, "{first}");
    assert_eq!(cached, seeded, "{first}");
    // `--cap-lints allow` silences the registry package's warning.
    assert!(!first.contains("unused variable"), "{first}");
    let map = fs::read_to_string(project.join("build/cargo-sources.json")).unwrap();
    assert!(map.contains("fakeflags-1.0.0"), "{map}");

    // The cached source map answers without Cargo.
    let (_, second) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&second), (0, built + seeded), "{second}");

    // Vendor mode, with the directory `gluon vendor` would fill.
    copy_dir_all(
        &project.join("registry/fakeflags-1.0.0"),
        &project.join("vendor/fakeflags-1.0.0"),
    )
    .unwrap();
    gluon_core::manifest::sources::write_vendor_stamp(&project, &project.join("vendor")).unwrap();
    use_vendor_dir(&project);
    // The source path moved: fakeflags and the kernel rebuild.
    let (_, third) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&third).0, 2, "{third}");

    // Vendored sources are stat'ed: an edit rebuilds.
    append(
        &project.join("vendor/fakeflags-1.0.0/src/lib.rs"),
        "\npub const MORE: u32 = 1;\n",
    );
    let (_, fourth) = gluon_ok(&project, &["build"], &without_cargo);
    assert!(summary(&fourth).0 >= 1, "{fourth}");

    append(&project.join("Cargo.lock"), "\n");
    let out = gluon(&project, &["build"], &without_cargo);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("Cargo.lock changed since the last `gluon vendor`"),
        "{stderr}"
    );
}

/// True when crates.io's sparse index answers a TCP connect.
fn crates_io_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(mut addrs) = ("index.crates.io", 443).to_socket_addrs() else {
        return false;
    };
    addrs.any(|a| TcpStream::connect_timeout(&a, std::time::Duration::from_secs(3)).is_ok())
}

/// Network: the fixture with `fakeflags` swapped for crates.io
/// `bitflags`. Builds from Cargo's registry cache, then `gluon vendor`
/// fills `vendor/`, prints the line to add, and a vendor-mode build runs
/// no Cargo. Skips when crates.io is unreachable.
///
/// It uses the default Cargo home: another one would also change the
/// cargo build of `gluon-config` in the shared target directory, which
/// tests running in parallel link against.
#[test]
#[ignore]
fn registry_dependency_from_crates_io_and_gluon_vendor() {
    let name = "registry_dependency_from_crates_io_and_gluon_vendor";
    if !rustc_available(name) {
        return;
    }
    if !crates_io_reachable() {
        eprintln!("{name}: skipped — crates.io unreachable");
        return;
    }
    let (_tmp, project) = setup("tests/fixtures/cargo-registry");
    let manifest = project.join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let edited = text.replace(
        "fakeflags = \"1\"",
        "fakeflags = { package = \"bitflags\", version = \"2\" }",
    );
    assert_ne!(text, edited, "fixture manifest changed shape");
    fs::write(&manifest, edited).unwrap();
    fs::remove_file(project.join("Cargo.lock")).unwrap();
    let without_cargo = [("CARGO", OsStr::new(NO_CARGO))];
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let lock = Command::new(cargo)
        .arg("generate-lockfile")
        .current_dir(&project)
        .output()
        .expect("spawn cargo");
    if !lock.status.success() {
        eprintln!(
            "{name}: skipped — cargo generate-lockfile failed:\n{}",
            String::from_utf8_lossy(&lock.stderr)
        );
        return;
    }

    // Seed once Cargo can resolve the workspace: seeding loads the
    // model, which runs `cargo fetch`.
    let seeded = seed_sysroot(&project);
    let (_, first) = gluon_ok(&project, &["build"], &[]);
    let (built, _) = summary(&first);
    let map = fs::read_to_string(project.join("build/cargo-sources.json")).unwrap();
    assert!(map.contains("registry/src"), "{map}");
    let (_, second) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&second), (0, built + seeded), "{second}");

    let (stdout, _) = gluon_ok(&project, &["vendor"], &[]);
    assert!(
        stdout.contains("Vendored 1 packages into vendor/."),
        "{stdout}"
    );
    assert!(
        stdout.contains("cfg.cargo_workspace(\".\").vendored(\"vendor\");"),
        "{stdout}"
    );
    assert!(project.join("vendor/.gluon-vendor-stamp").is_file());
    let (stdout, _) = gluon_ok(&project, &["vendor", "--check"], &without_cargo);
    assert!(stdout.contains("matches Cargo.lock"), "{stdout}");

    use_vendor_dir(&project);
    let (_, third) = gluon_ok(&project, &["build"], &without_cargo);
    assert_eq!(summary(&third).0, 2, "{third}");
}

// ---------------------------------------------------------------------
// Build scripts of path packages (`tests/fixtures/build-script-cfg`)
// ---------------------------------------------------------------------

/// Labels of the build-script runs a `-v` build reran.
fn reran_scripts(stderr: &str) -> Vec<&str> {
    let mut runs: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("rebuilding build script of "))
        .map(|l| l.split_once(": ").map_or(l, |(what, _)| what))
        // Script compiles are labelled `(compile)`; keep the runs.
        .filter(|what| !what.ends_with("(compile)"))
        .collect();
    runs.sort_unstable();
    runs
}

/// `e_entry` of a 64-bit little-endian ELF file.
fn elf_entry(path: &Path) -> u64 {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"\x7fELF", "{} is not ELF", path.display());
    u64::from_le_bytes(bytes[24..32].try_into().unwrap())
}

/// Offline. Every script runs once per platform on the first build and
/// its directives reach rustc (the fixture's sources fail to compile
/// otherwise); a second build runs nothing; editing a
/// `rerun-if-changed` input reruns only that package's runs; editing a
/// source no script reads reruns no script.
#[test]
#[ignore]
fn build_scripts_of_path_packages_run_and_rerun_on_their_inputs() {
    if !rustc_available("build_scripts_of_path_packages") {
        return;
    }
    let (_tmp, project) = setup("tests/fixtures/build-script-cfg");

    let seeded = seed_sysroot(&project);
    let (_, first) = gluon_ok(&project, &["-v", "build"], &[]);
    let (built, cached) = summary(&first);
    assert_eq!(cached, seeded, "{first}");
    // Path packages show their scripts' warnings.
    assert!(
        first.contains("probe ran for x86_64-unknown-none"),
        "{first}"
    );
    let host = gluon_core::RustcInfo::probe().unwrap().host_triple;
    assert!(first.contains(&format!("probe ran for {host}")), "{first}");
    assert_eq!(reran_scripts(&first).len(), 5, "{first}");
    // `rustc-link-arg=-T<kernel.ld>` placed the kernel at 1 MiB.
    let kernel = project.join("build/cross/x86_64-unknown-none/dev/final/kernel");
    let entry = elf_entry(&kernel);
    assert!((0x100000..0x200000).contains(&entry), "entry {entry:#x}");

    let (_, second) = gluon_ok(&project, &["-v", "build"], &[]);
    assert_eq!(summary(&second), (0, built + seeded), "{second}");
    assert!(reran_scripts(&second).is_empty(), "{second}");

    fs::write(project.join("probe/probe.txt"), "changed\n").unwrap();
    let (_, third) = gluon_ok(&project, &["-v", "build"], &[]);
    let reran = reran_scripts(&third);
    assert_eq!(reran.len(), 2, "{third}");
    assert!(
        reran.iter().all(|r| r.starts_with("crate probe")),
        "{third}"
    );

    append(
        &project.join("consumer/src/lib.rs"),
        "\npub const MORE: u32 = 1;\n",
    );
    let (_, fourth) = gluon_ok(&project, &["-v", "build"], &[]);
    assert!(summary(&fourth).0 > 0, "{fourth}");
    assert!(reran_scripts(&fourth).is_empty(), "{fourth}");
}

// ---------------------------------------------------------------------
// Hadron-shaped workspace (`tests/fixtures/cargo-hadron`)
// ---------------------------------------------------------------------

/// Every file under `dir` whose name starts with `prefix`.
fn files_named(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_named(&path, prefix));
        } else if entry.file_name().to_string_lossy().starts_with(prefix) {
            out.push(path);
        }
    }
    out
}

/// The `output` files of the runs of `package`'s build script under
/// `platform_dir`.
fn run_outputs(platform_dir: &Path, package: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(platform_dir.join("build")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix(package)
                .is_some_and(|rest| rest.starts_with('-'))
        })
        .filter_map(|e| fs::read_to_string(e.path().join("output")).ok())
        .collect()
}

/// A workspace shaped like Hadron's, with Hadron's registry versions
/// pinned in its Cargo.lock: a path proc macro on `syn`, `quote` and
/// `proc-macro2`, and a `no_std` crate deriving zerocopy traits, built
/// for `x86_64-unknown-none`. Sources come from Cargo's registry cache.
/// Runs offline when the cache holds them; otherwise needs crates.io,
/// and skips when neither works.
#[test]
#[ignore]
fn hadron_shaped_workspace_builds_proc_macros_and_zerocopy() {
    let name = "hadron_shaped_workspace_builds_proc_macros_and_zerocopy";
    if !rustc_available(name) {
        return;
    }
    let (_tmp, project) = setup("tests/fixtures/cargo-hadron");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cached = Command::new(&cargo)
        .args(["fetch", "--locked", "--offline"])
        .current_dir(&project)
        .output()
        .expect("spawn cargo")
        .status
        .success();
    if !cached && !crates_io_reachable() {
        eprintln!("{name}: skipped — registry sources not cached and crates.io unreachable");
        return;
    }

    let seeded = seed_sysroot(&project);
    let (_, first) = gluon_ok(&project, &["build"], &[]);
    let (built, cached) = summary(&first);
    assert_eq!(cached, seeded, "{first}");
    let build = project.join("build");
    assert!(
        build
            .join("cross/x86_64-unknown-none/dev/final/kernel")
            .is_file()
    );

    // The build scripts of the proc macro's deps ran for the host.
    let host = build.join("host");
    for package in ["proc-macro2", "quote"] {
        assert_eq!(run_outputs(&host, package).len(), 1, "{package}: {first}");
    }
    // zerocopy's ran for the target, and on a current toolchain sets
    // only check-cfgs.
    let cross = build.join("cross/x86_64-unknown-none/dev");
    let zerocopy = run_outputs(&cross, "zerocopy");
    assert_eq!(zerocopy.len(), 1, "{first}");
    assert!(zerocopy[0].contains("rustc-check-cfg="), "{}", zerocopy[0]);
    assert!(!zerocopy[0].contains("rustc-cfg="), "{}", zerocopy[0]);

    // Proc macros and their deps are host-only.
    assert!(!files_named(&host.join("deps"), "libhello_derive-").is_empty());
    assert!(!files_named(&host.join("deps"), "libzerocopy_derive-").is_empty());
    assert!(!files_named(&host.join("deps"), "libsyn-").is_empty());
    assert!(files_named(&build.join("cross"), "libsyn-").is_empty());

    let (_, second) = gluon_ok(&project, &["build"], &[]);
    assert_eq!(summary(&second), (0, built + seeded), "{second}");

    // After the build, `gluon configure` gives rust-analyzer the proc
    // macros' dylibs and each build script's cfgs and `OUT_DIR`.
    let crates = configure_crates(&project);
    for package in ["hello-derive", "zerocopy-derive"] {
        let entry = crate_entry(&crates, package, None);
        let dylib = entry["proc_macro_dylib_path"]
            .as_str()
            .unwrap_or_else(|| panic!("{package}: no proc_macro_dylib_path: {entry:#}"));
        assert!(Path::new(dylib).is_file(), "{package}: {dylib} missing");
    }
    for (package, platform_dir, target) in [
        ("proc-macro2", &host, None),
        ("zerocopy", &cross, Some("x86_64-unknown-none")),
    ] {
        let entry = crate_entry(&crates, package, target);
        // Registry packages are indexed from Cargo's registry cache.
        let root = entry["root_module"].as_str().unwrap();
        assert!(root.contains("/registry/src/"), "{package}: {root}");
        let out_dir = entry["env"]["OUT_DIR"]
            .as_str()
            .unwrap_or_else(|| panic!("{package}: no OUT_DIR: {entry:#}"));
        assert!(Path::new(out_dir).is_dir(), "{package}: {out_dir} missing");
        // Every `rustc-cfg` the run printed, after the features.
        let output = &run_outputs(platform_dir, package)[0];
        let script_cfgs: Vec<&str> = output
            .lines()
            .filter_map(|l| {
                l.strip_prefix("cargo::rustc-cfg=")
                    .or_else(|| l.strip_prefix("cargo:rustc-cfg="))
            })
            .collect();
        let cfgs: Vec<&str> = entry["cfg"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c.as_str())
            .filter(|c| !c.starts_with("feature="))
            .collect();
        assert_eq!(cfgs, script_cfgs, "{package}: {entry:#}");
    }
    // proc-macro2's script sets cfgs on every toolchain, so the
    // comparison above is not vacuous.
    assert!(run_outputs(&host, "proc-macro2")[0].contains("rustc-cfg="));
}

/// Run `gluon configure` in `project` and return the `crates` array of
/// the `rust-project.json` it writes.
fn configure_crates(project: &Path) -> Vec<serde_json::Value> {
    gluon_ok(project, &["configure"], &[]);
    let text = fs::read_to_string(project.join("rust-project.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["crates"].as_array().unwrap().clone()
}

/// The entry of registry or path package `package` with `target`
/// (`None` for host), found by its source directory name.
fn crate_entry<'a>(
    crates: &'a [serde_json::Value],
    package: &str,
    target: Option<&str>,
) -> &'a serde_json::Value {
    crates
        .iter()
        .find(|c| {
            let dir = c["source"]["include_dirs"][0].as_str().unwrap_or("");
            let name = Path::new(dir)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Registry directories are `<name>-<version>`.
            let matches = name == package
                || name
                    .strip_prefix(package)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .is_some_and(|v| v.starts_with(|ch: char| ch.is_ascii_digit()));
            matches && c["target"].as_str() == target
        })
        .unwrap_or_else(|| panic!("no rust-project.json entry for {package} ({target:?})"))
}
