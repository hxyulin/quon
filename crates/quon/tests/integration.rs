//! End-to-end CLI integration tests for `quon`.
//!
//! These tests spawn the real `quon` binary against a copy of
//! `tests/fixtures/minimal/` in a temp dir, so no `build/` directory or
//! `rust-project.json` leaks back into the source tree.
//!
//! ## What's `#[ignore]`-gated and why
//!
//! `quon build`, `check`, `clippy` and `configure` spawn rustc, and
//! builds need the `rust-src` component for the custom sysroot, so
//! those tests are `#[ignore]`-gated and skip when the toolchain is
//! missing. The `quon clean` and `quon fmt` tests run in the default
//! suite.
//!
//! ## Sysroots
//!
//! Every build test except `quon_build_cold_then_cache_hit` and
//! `target_spec_edit_rebuilds` starts from a seeded sysroot (see
//! `support::seed_sysroot`), which saves about 15 s per test. Those
//! two build it themselves: the first covers the sysroot build through
//! the CLI, and the second uses a custom target spec, which a seed
//! cannot match.
//!
//! The freshness checks run as steps of three tests, each on one
//! fixture, so the suite pays for three cold builds instead of one per
//! check while the three still run in parallel. A failing step hides
//! the steps after it; each step's message names it.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use support::{
    counts, quon, quon_ok, rustc_available, seed_sysroot, seed_sysroot_for, setup_fixture, summary,
};

fn setup() -> (tempfile::TempDir, PathBuf) {
    setup_fixture("minimal")
}

fn kernel_bin(fixture: &Path) -> PathBuf {
    fixture.join("build/cross/x86_64-unknown-none/dev/final/kernel")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn append(path: &Path, text: &str) {
    let mut src = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    src.push_str(text);
    fs::write(path, src).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
}

/// Append a `#[used]` static named `name` to the kernel, so `value` (a
/// `&str` expression) ends up in the binary's bytes.
fn embed_in_kernel(fixture: &Path, name: &str, value: &str) {
    append(
        &fixture.join("crates/kernel/src/main.rs"),
        &format!("\n#[used]\n#[no_mangle]\npub static {name}: &str = {value};\n"),
    );
}

/// Cold build, then a second build that hits the cache. This is the one
/// CLI test that builds the sysroot from scratch.
#[test]
#[ignore]
fn quon_build_cold_then_cache_hit() {
    if !rustc_available("quon_build_cold_then_cache_hit") {
        return;
    }
    let (_tmp, fixture) = setup();

    let first = quon_ok(&fixture, &["build"]);
    let (built, cached) = summary(&first);
    assert!(built > 0, "cold build built nothing:\n{first}");
    assert_eq!(cached, 0, "cold build found something cached:\n{first}");

    // The sysroot's freshness lives in its fingerprint record, not in
    // a sidecar stamp file.
    let store = quon_core::FingerprintStore::new(fixture.join("build/.fingerprint"), Vec::new());
    assert!(
        store.record_path("sysroot:x86_64-unknown-none").is_file(),
        "expected the sysroot record after a cold build"
    );
    assert!(
        kernel_bin(&fixture).is_file(),
        "kernel binary missing after a cold build:\n{first}"
    );

    let second = quon_ok(&fixture, &["build"]);
    assert_eq!(summary(&second), (0, built), "second build:\n{second}");
}

/// `quon configure` must write a parseable `rust-project.json` at the
/// default location, containing the config crate plus one entry per
/// resolved crate in the fixture.
#[test]
#[ignore]
fn quon_configure_emits_valid_json() {
    if !rustc_available("quon_configure_emits_valid_json") {
        return;
    }
    let (_tmp, fixture) = setup();
    quon_ok(&fixture, &["configure"]);

    let rp_path = fixture.join("rust-project.json");
    let body = fs::read_to_string(&rp_path).expect("read rust-project.json");

    // serde_json is not a dev-dependency of quon, so these are
    // substring checks on the structure.
    assert!(
        body.trim_start().starts_with('{'),
        "rust-project.json must start with '{{': {body}"
    );
    assert!(
        body.contains("\"crates\""),
        "rust-project.json must declare a 'crates' key: {body}"
    );
    assert!(
        body.contains("\"is_proc_macro\": true") || body.contains("\"is_proc_macro\":true"),
        "rust-project.json must mark the proc-macro crate with is_proc_macro=true: {body}"
    );
    assert!(
        body.contains("crates/kernel"),
        "rust-project.json must reference crates/kernel: {body}"
    );
    assert!(
        body.contains("crates/proc_macro"),
        "rust-project.json must reference crates/proc_macro: {body}"
    );
    // The generated `<project>_config` crate always lives under
    // `<build>/generated/minimal_config/`.
    assert!(
        body.contains("minimal_config"),
        "rust-project.json must reference the generated minimal_config crate: {body}"
    );
    // `sysroot_src` must point to the `library/` subdirectory (where
    // core/alloc/std live), not the parent `src/rust` directory. Missing
    // this suffix silently breaks stdlib completions in rust-analyzer.
    assert!(
        body.contains("/library\""),
        "sysroot_src must end in /library: {body}"
    );
    // Cross crates must carry a `target` field so rust-analyzer evaluates
    // `#[cfg(target_os = "none")]` against the kernel's triple.
    assert!(
        body.contains("\"target\""),
        "at least one crate must declare a target field: {body}"
    );
}

/// `quon clean` (default: no `--keep-sysroot`) must remove the entire
/// `build/` directory. Runs without rustc because `clean` uses the
/// layout-only context.
#[test]
fn quon_clean_removes_build_dir() {
    let (_tmp, fixture) = setup();

    let build_dir = fixture.join("build");
    fs::create_dir_all(&build_dir).expect("mkdir build");
    fs::write(build_dir.join("sentinel"), b"hello").expect("write sentinel");

    quon_ok(&fixture, &["clean"]);
    assert!(
        !build_dir.exists(),
        "build/ should have been removed by quon clean"
    );
}

/// `quon clean --keep-sysroot` must remove build state but leave the
/// sysroot subtree and its fingerprint records intact.
/// Runs without rustc because `clean` uses the layout-only context.
#[test]
fn quon_clean_keep_sysroot_preserves_sysroot() {
    let (_tmp, fixture) = setup();

    // Fabricate a fake pre-built state: a sysroot rlib, a sysroot
    // record and another record, plus some other build artifact. The
    // sysroot subtree and its record should survive; the other record
    // and the other artifact should be deleted.
    let build_dir = fixture.join("build");
    let sysroot_lib_dir = build_dir
        .join("sysroot")
        .join("x86_64-unknown-none")
        .join("lib");
    fs::create_dir_all(&sysroot_lib_dir).expect("mkdir sysroot lib");
    let core_rlib = sysroot_lib_dir.join("libcore-quon-core.rlib");
    fs::write(&core_rlib, b"fake rlib").expect("write rlib");

    let other = build_dir.join("cross").join("leftover");
    fs::create_dir_all(other.parent().unwrap()).expect("mkdir cross");
    fs::write(&other, b"junk").expect("write junk");

    // One sysroot record and one other record. Their content does not
    // matter to clean, which keeps records by file name.
    let store = quon_core::FingerprintStore::new(build_dir.join(".fingerprint"), Vec::new());
    let sysroot_record = store.record_path("sysroot:x86_64-unknown-none");
    let other_record = store.record_path("image:boot");
    fs::create_dir_all(store.dir()).expect("mkdir .fingerprint");
    fs::write(&sysroot_record, b"{}").expect("write sysroot record");
    fs::write(&other_record, b"{}").expect("write other record");

    quon_ok(&fixture, &["clean", "--keep-sysroot"]);

    assert!(
        core_rlib.is_file(),
        "sysroot rlib should be preserved by --keep-sysroot"
    );
    assert!(
        !other.exists(),
        "non-sysroot build state should have been removed"
    );
    assert!(
        sysroot_record.exists(),
        "the sysroot record should survive --keep-sysroot"
    );
    assert!(
        !other_record.exists(),
        "other records should be removed by --keep-sysroot"
    );
}

/// `quon clippy`:
/// - lints every crate into `build/tool/clippy/`, including the
///   project's own proc macro, which is built with plain rustc so
///   dependents can load the real dylib;
/// - shares that dylib with `quon build`;
/// - lints every crate again after either kind of `quon clean`, since
///   both remove `build/tool/`.
#[test]
#[ignore]
fn quon_clippy_lints_every_crate() {
    if !rustc_available("quon_clippy_lints_every_crate") {
        return;
    }
    if !clippy_driver_available() {
        eprintln!("quon_clippy_lints_every_crate: skipped, clippy-driver not found");
        return;
    }
    let (_tmp, fixture) = setup();
    seed_sysroot(&fixture);
    append(
        &fixture.join("crates/proc_macro/src/lib.rs"),
        "\npub(crate) fn answer() -> i32 {\n    return 42;\n}\n",
    );
    let tool = fixture.join("build/tool");

    let first = quon_ok(&fixture, &["clippy"]);
    let (linted, _) = counts(&first, "linted");
    assert!(linted > 0, "clippy linted nothing:\n{first}");
    assert!(
        first.contains("needless_return"),
        "clippy should report the proc macro's needless return:\n{first}"
    );
    assert!(
        tool.join("clippy").is_dir(),
        "clippy writes build/tool/clippy/"
    );

    quon_ok(&fixture, &["build"]);
    let host_deps = fixture.join("build/host/deps");
    let dylibs = fs::read_dir(&host_deps)
        .expect("read host deps")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("libminimal_derive-")
        })
        .count();
    assert!(dylibs > 0, "proc macro dylib missing from {host_deps:?}");

    quon_ok(&fixture, &["clean", "--keep-sysroot"]);
    assert!(
        !tool.exists(),
        "clean --keep-sysroot must remove build/tool/"
    );
    let again = quon_ok(&fixture, &["clippy"]);
    assert_eq!(
        counts(&again, "linted").0,
        linted,
        "clippy after clean --keep-sysroot must lint every crate:\n{again}"
    );

    quon_ok(&fixture, &["clean"]);
    assert!(!tool.exists(), "clean must remove build/tool/");
    seed_sysroot(&fixture);
    let again = quon_ok(&fixture, &["clippy"]);
    assert_eq!(
        counts(&again, "linted").0,
        linted,
        "clippy after clean must lint every crate:\n{again}"
    );
}

fn clippy_driver_available() -> bool {
    if let Some(path) = std::env::var_os("CLIPPY_DRIVER") {
        return Path::new(&path).is_file();
    }
    // Sibling of rustc — the rustup install layout puts both in the
    // same toolchain bin dir.
    if let Ok(info) = quon_core::RustcInfo::probe()
        && let Some(parent) = info.rustc_path.parent()
    {
        let cand = parent.join(if cfg!(windows) {
            "clippy-driver.exe"
        } else {
            "clippy-driver"
        });
        if cand.is_file() {
            return true;
        }
    }
    // Final fallback: bare-name lookup via spawning `clippy-driver -V`.
    Command::new("clippy-driver")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `quon fmt --check` on the fixture passes when the sources are
/// already formatted. The fixture's crates start out clean.
#[test]
fn quon_fmt_check_passes_on_formatted_fixture() {
    if !rustfmt_available() {
        eprintln!("quon_fmt_check_passes_on_formatted_fixture: skipped — rustfmt not found");
        return;
    }
    let (_tmp, fixture) = setup();

    let stderr = quon_ok(&fixture, &["fmt", "--check"]);
    assert!(
        stderr.contains("processed"),
        "fmt summary should report 'processed', got: {stderr}"
    );
}

/// `quon fmt --check` on a deliberately-unformatted source must
/// exit non-zero and name the offending crate.
#[test]
fn quon_fmt_check_fails_on_unformatted_fixture() {
    if !rustfmt_available() {
        eprintln!("quon_fmt_check_fails_on_unformatted_fixture: skipped — rustfmt not found");
        return;
    }
    let (_tmp, fixture) = setup();

    // Multiple statements on a line is something rustfmt always
    // normalizes.
    append(
        &fixture.join("crates/kernel/src/main.rs"),
        "\nfn _trailing(){let _x =1;let _y= 2;}\n",
    );

    let output = quon(&fixture)
        .args(["fmt", "--check"])
        .output()
        .expect("spawn quon fmt --check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "quon fmt --check should fail on an unformatted fixture, but it succeeded; stderr=\n{stderr}"
    );
    assert!(
        stderr.contains("unformatted") || stderr.contains("need formatting"),
        "stderr should mention unformatted/need formatting, got: {stderr}"
    );
}

fn rustfmt_available() -> bool {
    if let Some(path) = std::env::var_os("RUSTFMT") {
        return Path::new(&path).is_file();
    }
    Command::new("rustfmt")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Loads the multi-profile fixture via `load_build_model` and verifies
/// the model captures multiple profiles, cfg_flags, dependency features,
/// and `default_profile`.
///
/// `#[ignore]`-gated because `load_build_model` spawns rustc to compile
/// the script-runner binary.
#[test]
#[ignore]
fn multi_profile_fixture_model() {
    if !rustc_available("multi_profile_fixture_model") {
        return;
    }
    let model = quon_core::load_build_model(&support::fixture_dir("multi-profile"))
        .expect("load_build_model for multi-profile should succeed");

    // -- Project & default_profile --
    let project = model.project.as_ref().expect("project should be set");
    assert_eq!(project.name, "multiprofile_test");
    assert_eq!(
        project.default_profile.as_deref(),
        Some("debug"),
        "default_profile should be 'debug'"
    );

    // -- Two profiles --
    let debug_h = model
        .profiles
        .lookup("debug")
        .expect("model should have a 'debug' profile");
    let debug = model.profiles.get(debug_h).unwrap();
    assert_eq!(debug.opt_level, Some(0));
    assert_eq!(debug.debug_info, Some(true));

    let release_h = model
        .profiles
        .lookup("release")
        .expect("model should have a 'release' profile");
    let release = model.profiles.get(release_h).unwrap();
    assert_eq!(release.opt_level, Some(2));
    assert_eq!(release.debug_info, Some(false));
    assert_eq!(release.lto.as_deref(), Some("thin"));

    // -- One group per crate --
    assert!(
        model.groups.lookup("mylib").is_some(),
        "model should have a 'mylib' group"
    );
    assert!(
        model.groups.lookup("app").is_some(),
        "model should have an 'app' group"
    );

    // -- Two crates --
    assert!(
        model.crates.lookup("mylib").is_some(),
        "model should have a 'mylib' crate"
    );
    let app_h = model
        .crates
        .lookup("app")
        .expect("model should have an 'app' crate");
    let app = model.crates.get(app_h).unwrap();

    // -- cfg_flags on app --
    assert_eq!(
        app.cfg_flags,
        vec!["my_feature".to_string(), "test_mode".to_string()],
        "app should have cfg_flags [my_feature, test_mode]"
    );

    // -- app depends on mylib with features: ["logging"] --
    let dep = app
        .deps
        .get("mylib")
        .expect("app should have a dep named 'mylib'");
    let mylib_h = model
        .crates
        .lookup("mylib")
        .expect("mylib crate should be present");
    assert_eq!(
        dep.resolution,
        quon_core::model::DepResolution::Project(mylib_h),
        "app's mylib dep should resolve to the mylib project crate"
    );
    assert_eq!(
        dep.features,
        vec!["logging".to_string()],
        "mylib dep should have features [logging]"
    );
}

// ---------------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------------

/// Add two kernel libraries, `klib_top` using `klib_base`, and make the
/// kernel use `klib_top`.
fn add_library_using_library(fixture: &Path) {
    for (name, src) in [
        ("klib_base", "#![no_std]\npub const ANSWER: u32 = 42;\n"),
        (
            "klib_top",
            "#![no_std]\npub fn answer() -> u32 {\n    klib_base::ANSWER\n}\n",
        ),
    ] {
        let dir = fixture.join("crates").join(name).join("src");
        fs::create_dir_all(&dir).expect("create lib dir");
        fs::write(dir.join("lib.rs"), src).expect("write lib.rs");
    }
    let quon_rs = fixture.join("quon.rs");
    let script = fs::read_to_string(&quon_rs).expect("read quon.rs");
    // The libraries come before the kernel, which takes their handles.
    let edited = script.replacen(
        "    let kernel = q.add(",
        "    let klib_base = q.add(\n        \
         Crate::lib(\"klib_base\", \"crates/klib_base\")\n            \
         .target(&target)\n            .edition(Edition::E2021),\n    );\n    \
         let klib_top = q.add(\n        \
         Crate::lib(\"klib_top\", \"crates/klib_top\")\n            \
         .target(&target)\n            .edition(Edition::E2021)\n            \
         .dep(&klib_base),\n    );\n    \
         let kernel = q.add(",
        1,
    );
    // Only crates reachable from the kernel are part of the build.
    let edited = edited.replacen(
        ".dep(&derive),",
        ".dep(&derive)\n            .dep(&klib_top),",
        1,
    );
    assert_ne!(script, edited, "fixture quon.rs changed shape");
    fs::write(&quon_rs, edited).expect("write quon.rs");
    let main_rs = fixture.join("crates/kernel/src/main.rs");
    let main = fs::read_to_string(&main_rs).expect("read main.rs");
    fs::write(
        &main_rs,
        main.replace(
            "let _ = HELLO;",
            "let _ = HELLO;\n    let _ = klib_top::answer();",
        ),
    )
    .expect("write main.rs");
}

/// Environment variable naming the file [`rustc_spawn_logger`] appends
/// to. Unset, the logger only runs rustc.
const RUSTC_LOG: &str = "QUON_TEST_RUSTC_LOG";

/// A shell script that logs every compile (any argv with
/// `--crate-name`) to `$QUON_TEST_RUSTC_LOG` and then runs the real
/// rustc. Passed to the child `quon` as `RUSTC`.
///
/// Every test shares one script at a fixed path: the rustc path is an
/// input of every cache record, so a per-test script would never match
/// the seeded sysroot. The script is only rewritten when its content
/// changes, through a rename, so a test running it never reads half a
/// file.
#[cfg(unix)]
fn rustc_spawn_logger() -> PathBuf {
    static SCRIPT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SCRIPT.get_or_init(install_rustc_spawn_logger).clone()
}

#[cfg(unix)]
fn install_rustc_spawn_logger() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let real = String::from_utf8(
        Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .expect("rustc --print sysroot")
            .stdout,
    )
    .expect("utf-8 sysroot");
    let real = Path::new(real.trim()).join("bin/rustc");
    let body = format!(
        "#!/bin/sh\nif [ -n \"${RUSTC_LOG}\" ]; then\n  case \"$*\" in *--crate-name*) echo \"$*\" >> \"${RUSTC_LOG}\";; esac\nfi\nexec '{}' \"$@\"\n",
        real.display()
    );
    let script = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rustc-logger.sh");
    if fs::read_to_string(&script).ok().as_deref() != Some(body.as_str()) {
        let tmp = script.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, &body).expect("write rustc logger");
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).expect("chmod logger");
        fs::rename(&tmp, &script).expect("install rustc logger");
    }
    script
}

/// The variable the kernel reads through `option_env!`.
const ENV_DEP: &str = "QUON_TEST_ENV_DEP";

/// One fixture, built once, then a series of edits. Each edit asserts
/// what the next build rebuilds, spawns or prints.
#[cfg(unix)]
struct Steps {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    log: PathBuf,
    rustc: PathBuf,
}

#[cfg(unix)]
impl Steps {
    /// A seeded copy of the minimal fixture whose kernel embeds the
    /// proc macro's `HELLO` and `ENV_DEP`, and has a function that is
    /// never called, for a warning. `None` when rustc is missing.
    fn new(test_name: &str) -> Option<Self> {
        if !rustc_available(test_name) {
            return None;
        }
        let (tmp, dir) = setup();
        let log = tmp.path().join("rustc-spawns.log");
        let rustc = rustc_spawn_logger();
        seed_sysroot_for(&dir, &rustc);
        embed_in_kernel(&dir, "QUON_TEST_HELLO", "HELLO");
        embed_in_kernel(
            &dir,
            "QUON_TEST_ENV",
            &format!(r#"match option_env!("{ENV_DEP}") {{ Some(v) => v, None => "env-unset" }}"#),
        );
        append(
            &dir.join("crates/kernel/src/main.rs"),
            "\nfn quon_test_never_called() {}\n",
        );
        Some(Self {
            _tmp: tmp,
            dir,
            log,
            rustc,
        })
    }

    /// Run `quon <args>` with the rustc logger and `ENV_DEP` set to
    /// `env` (unset for `None`). Returns the output and the compiles
    /// it spawned.
    fn run(&self, args: &[&str], env: Option<&str>) -> (std::process::Output, String) {
        let _ = fs::remove_file(&self.log);
        let mut cmd = quon(&self.dir);
        cmd.args(args)
            .env("RUSTC", &self.rustc)
            .env(RUSTC_LOG, &self.log);
        match env {
            Some(v) => cmd.env(ENV_DEP, v),
            None => cmd.env_remove(ENV_DEP),
        };
        let out = cmd.output().expect("spawn quon");
        (out, fs::read_to_string(&self.log).unwrap_or_default())
    }

    /// [`Steps::run`] that must succeed; returns stderr and the
    /// compiles it spawned.
    fn ok(&self, step: &str, args: &[&str], env: Option<&str>) -> (String, String) {
        let (out, spawned) = self.run(args, env);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "{step}: quon {args:?} failed:\n{stderr}"
        );
        (stderr, spawned)
    }

    /// `quon build`; returns how many nodes it built.
    fn built(&self, step: &str, env: Option<&str>) -> usize {
        let (stderr, _) = self.ok(step, &["build"], env);
        summary(&stderr).0
    }

    fn kernel(&self) -> Vec<u8> {
        fs::read(kernel_bin(&self.dir)).expect("read kernel")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

/// A build with nothing changed spawns no rustc and replays warnings,
/// a swap file is not an input, `-v` names why a node reruns, and
/// `quon check` caches on its own without touching the build's cache.
#[cfg(unix)]
#[test]
#[ignore]
fn unchanged_inputs_rebuild_nothing() {
    let Some(s) = Steps::new("unchanged_inputs_rebuild_nothing") else {
        return;
    };

    let (first, spawned) = s.ok("cold build", &["-j1", "build"], None);
    assert!(!spawned.is_empty(), "cold build spawned no rustc");
    assert!(
        summary(&first).1 > 0,
        "cold build rebuilt the seeded sysroot:\n{first}"
    );
    assert!(
        first.contains("quon_test_never_called"),
        "cold build did not print the warning:\n{first}"
    );

    for jobs in ["1", "8"] {
        let (stderr, spawned) = s.ok("no-op build", &["-j", jobs, "build"], None);
        assert!(
            spawned.is_empty(),
            "-j{jobs}: no-op build spawned rustc:\n{spawned}"
        );
        assert_eq!(
            summary(&stderr).0,
            0,
            "-j{jobs}: no-op build rebuilt:\n{stderr}"
        );
        assert!(
            stderr.contains("quon_test_never_called"),
            "-j{jobs}: cached build did not replay the warning:\n{stderr}"
        );
    }

    // An editor's swap file changes the directory mtime but no input.
    let swap = s.path("crates/kernel/src/.main.rs.swp");
    fs::write(&swap, b"swap").expect("create swap file");
    fs::remove_file(&swap).expect("remove swap file");
    assert_eq!(s.built("swap file", None), 0, "swap file caused a rebuild");

    append(&s.path("crates/kernel/src/main.rs"), "\n// edited\n");
    let (stderr, _) = s.ok("verbose", &["-v", "build"], None);
    let line = stderr
        .lines()
        .find(|l| l.contains("rebuilding crate kernel (x86_64-unknown-none, dev)"))
        .unwrap_or_else(|| panic!("verbose: no stale reason for the kernel:\n{stderr}"));
    assert!(
        line.contains("main.rs changed"),
        "verbose: stale reason does not name the edited file: {line}"
    );
    assert!(
        !stderr.contains("rebuilding crate minimal_derive"),
        "verbose: fresh proc macro reported as rebuilt:\n{stderr}"
    );

    let (stderr, _) = s.ok("check", &["check"], None);
    assert!(
        counts(&stderr, "checked").0 > 0,
        "check checked nothing:\n{stderr}"
    );
    assert!(
        s.path("build/tool/check").is_dir(),
        "check: no build/tool/check/"
    );
    let (stderr, _) = s.ok("check again", &["check"], None);
    assert_eq!(
        counts(&stderr, "checked").0,
        0,
        "second check rechecked:\n{stderr}"
    );
    assert_eq!(
        s.built("build after check", None),
        0,
        "check wiped the build cache"
    );
    assert!(
        kernel_bin(&s.dir).is_file(),
        "check removed the kernel binary"
    );
}

/// Inputs rustc reports only partly, or not at all, still rebuild what
/// reads them: a new module file, the linker script, the proc-macro
/// dylib and a variable read through `option_env!`.
#[cfg(unix)]
#[test]
#[ignore]
fn edited_inputs_rebuild_their_readers() {
    let Some(s) = Steps::new("edited_inputs_rebuild_their_readers") else {
        return;
    };
    let one = Some("env-value-one");
    s.built("cold build", one);
    assert!(contains_bytes(&s.kernel(), b"hello"));
    assert!(contains_bytes(&s.kernel(), b"env-value-one"));

    // A new module is tracked through the depfile, and so are later
    // edits to it alone.
    append(&s.path("crates/kernel/src/main.rs"), "\nmod foo;\n");
    let foo_rs = s.path("crates/kernel/src/foo.rs");
    fs::write(&foo_rs, "pub const A: u32 = 1;\n").expect("write foo.rs");
    assert!(s.built("new module", one) > 0, "new module did not rebuild");
    fs::write(&foo_rs, "pub const A: u32 = 2;\n").expect("edit foo.rs");
    assert!(
        s.built("module edit", one) > 0,
        "foo.rs edit did not rebuild"
    );

    // rustc does not list `-C link-arg=-T<path>` files in its depfile.
    append(&s.path("crates/kernel/kernel.ld"), "\n/* edited */\n");
    assert!(
        s.built("linker script", one) > 0,
        "linker script edit did not rebuild"
    );

    // Output paths are fixed, so the kernel's argv does not change when
    // the proc-macro dylib is rebuilt; the dylib must be a tracked input.
    let proc_macro = s.path("crates/proc_macro/src/lib.rs");
    let src = fs::read_to_string(&proc_macro).expect("read proc macro");
    fs::write(
        &proc_macro,
        src.replace("\\\"hello\\\"", "\\\"proc-macro-v2\\\""),
    )
    .expect("write proc macro");
    s.built("proc macro", one);
    assert!(
        contains_bytes(&s.kernel(), b"proc-macro-v2"),
        "proc macro: kernel binary does not reflect the edit"
    );

    // rustc records `option_env!` reads as `# env-dep:` depfile lines.
    assert_eq!(s.built("env unchanged", one), 0, "unchanged env rebuilt");
    s.built("env change", Some("env-value-two"));
    assert!(
        contains_bytes(&s.kernel(), b"env-value-two"),
        "env change: kernel binary does not reflect the new value"
    );
    s.built("env unset", None);
    assert!(
        contains_bytes(&s.kernel(), b"env-unset"),
        "env unset: kernel binary does not reflect the variable being unset"
    );
}

/// A failed build keeps the records of the nodes that finished, outputs
/// are kept per profile, and a checked library hands its dependents its
/// `.rmeta`.
#[cfg(unix)]
#[test]
#[ignore]
fn failures_and_profile_switches_keep_finished_work() {
    let Some(s) = Steps::new("failures_and_profile_switches_keep_finished_work") else {
        return;
    };
    let main_rs = s.path("crates/kernel/src/main.rs");

    // The config crate, proc macro and sysroot finish before the
    // kernel fails. Once the kernel is fixed only it rebuilds. The
    // failure names the crate, its target and the profile.
    let good = fs::read_to_string(&main_rs).expect("read main.rs");
    append(&main_rs, "\ncompile_error!(\"broken on purpose\");\n");
    let (out, _) = s.run(&["build"], None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "broken kernel built:\n{stderr}");
    assert!(
        stderr.contains("crate kernel (x86_64-unknown-none, dev)"),
        "failed build: the error does not name the crate:\n{stderr}"
    );
    fs::write(&main_rs, good).expect("fix main.rs");
    assert_eq!(
        s.built("after failure", None),
        1,
        "nodes that finished before the failure rebuilt"
    );

    // Every unit, the config crate included, keeps one output per
    // profile, and proc macros use the fixed host profile, so switching
    // back to dev spawns no rustc.
    let (_, release) = s.ok("release", &["--profile", "release", "build"], None);
    assert!(
        !release.contains("--crate-name minimal_derive"),
        "release build rebuilt the proc macro:\n{release}"
    );
    let (_, dev) = s.ok("back to dev", &["build"], None);
    assert!(
        dev.is_empty(),
        "switching back to dev spawned rustc:\n{dev}"
    );

    add_library_using_library(&s.dir);
    s.ok("library using library", &["check"], None);
}

/// Editing a custom target JSON spec must rebuild the sysroot and every
/// crate for that target. rustc does not list the spec in its depfile.
#[test]
#[ignore]
fn target_spec_edit_rebuilds() {
    if !rustc_available("target_spec_edit_rebuilds") {
        return;
    }
    let (_tmp, fixture) = setup();
    let spec_src = support::fixture_dir("multi-profile").join("targets/x86_64-unknown-test.json");
    let spec = fixture.join("targets/x86_64-unknown-test.json");
    fs::create_dir_all(spec.parent().unwrap()).expect("mkdir targets");
    fs::copy(&spec_src, &spec).expect("copy target spec");
    let quon_rs = fixture.join("quon.rs");
    let script = fs::read_to_string(&quon_rs).expect("read quon.rs");
    let script = script.replace(
        r#"Target::builtin("x86_64-unknown-none")"#,
        r#"Target::spec("x86_64-unknown-test", "targets/x86_64-unknown-test.json")"#,
    );
    fs::write(&quon_rs, script).expect("write quon.rs");
    quon_ok(&fixture, &["build"]);

    append(&spec, "\n");

    // The sysroot, config crate and kernel are built for the custom
    // target, so all three must rebuild. The host proc macro does not
    // read the spec.
    let stderr = quon_ok(&fixture, &["build"]);
    assert!(
        summary(&stderr).0 >= 3,
        "target spec edit did not rebuild the sysroot, config crate and kernel:\n{stderr}"
    );
}

/// A build directory from a Quon that kept every record in
/// `cache-manifest.json` is cleared once, with one note, and builds.
/// `final/` and `esp/` directories survive.
#[test]
#[ignore]
fn legacy_cache_manifest_is_removed_once() {
    if !rustc_available("legacy_cache_manifest_is_removed_once") {
        return;
    }
    let (_tmp, fixture) = setup();
    // The migration keeps `build/sysroot/`, so the seed survives it.
    seed_sysroot(&fixture);
    let build = fixture.join("build");
    let old_dep = build.join("host/deps/libold-quon-old.rlib");
    let old_cross = build.join("cross/x86_64-unknown-none/dev/deps/libold.rlib");
    let kept = build.join("cross/x86_64-unknown-none/dev/esp/boot/keep.txt");
    for f in [&old_dep, &old_cross, &kept] {
        fs::create_dir_all(f.parent().unwrap()).expect("mkdir");
        fs::write(f, b"old").expect("write old artifact");
    }
    let manifest = build.join("cache-manifest.json");
    fs::write(&manifest, br#"{"version":2,"rustc":{},"composite":{}}"#).expect("write manifest");

    let first = quon_ok(&fixture, &["build"]);
    assert!(
        first.contains("cache-manifest.json"),
        "no migration note:\n{first}"
    );
    assert!(!manifest.exists(), "old manifest survived");
    assert!(!old_dep.exists(), "old host artifact survived");
    assert!(!old_cross.exists(), "old cross artifact survived");
    assert!(kept.exists(), "esp/ was removed");
    assert!(kernel_bin(&fixture).exists(), "kernel not built");

    let second = quon_ok(&fixture, &["build"]);
    assert!(
        !second.contains("cache-manifest.json"),
        "migration note repeated:\n{second}"
    );
    assert_eq!(summary(&second).0, 0, "second build rebuilt:\n{second}");
}

/// `quon setup` builds the shared quon-config library under
/// `QUON_HOME` (the checkout's `target/` for tests, per
/// `.cargo/config.toml`) and prints its directory.
#[test]
#[ignore = "requires rustc; spawns cargo"]
fn setup_prints_the_shared_library_entry() {
    if !rustc_available("setup_prints_the_shared_library_entry") {
        return;
    }
    // The repository root, so rustup picks the toolchain the other
    // tests use and the entry is shared with them.
    let out = quon(&support::repo_root())
        .arg("setup")
        .output()
        .expect("spawn quon");
    assert!(
        out.status.success(),
        "quon setup failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = PathBuf::from(String::from_utf8(out.stdout).expect("utf-8").trim());
    assert!(
        dir.join("manifest.json").is_file(),
        "no manifest in {dir:?}"
    );
    let home = std::env::var_os("QUON_HOME").expect("QUON_HOME is set for tests");
    assert!(dir.starts_with(&home), "{dir:?} is not under {home:?}");
}
