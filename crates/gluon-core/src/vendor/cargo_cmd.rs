//! Subprocess wrappers around the `cargo` binary.
//!
//! Two entry points:
//!
//! - [`run_cargo_vendor`] — populates `vendor/` from the scratch
//!   workspace's declared deps. Runs `cargo vendor`. Inherits stdio so
//!   the user sees live progress.
//!
//! - [`run_cargo_build_vendor`] — compiles the scratch vendor workspace
//!   and captures the rlib / dylib paths that cargo produced. This is
//!   the bridge between cargo's compile pipeline (which handles
//!   build.rs, feature resolution, transitive deps, etc.) and gluon's
//!   compile pipeline (which `--extern`s those artifacts into the
//!   user's crates). Captures stdout to parse `--message-format=json`;
//!   stderr is inherited so cargo's human-readable diagnostics still
//!   reach the user.
//!
//! Both resolve the cargo binary via `$CARGO` (matching cargo's own
//! convention and what rustup sets on the outer cargo invocation),
//! falling back to bare `cargo` on `$PATH`.
//!
//! On non-zero exit, returns an [`Error::Diagnostics`] with a note
//! pointing at the binary we tried to run, so the user can tell
//! whether it's a missing toolchain, a stale network, or a real dep
//! error.

use crate::error::{Diagnostic, Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Flags that modulate the `cargo vendor` invocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct VendorFlags {
    /// Pass `--offline` and `--frozen` to cargo — forbids any network
    /// access and any mutation of `Cargo.lock`. Useful in CI when the
    /// lockfile is expected to be up to date.
    pub offline: bool,
}

/// Run `cargo vendor` on the given scratch workspace, populating
/// `vendor_target` with the resolved dependency closure.
///
/// `workspace_dir` must contain a valid `Cargo.toml` (the one produced
/// by [`super::manifest_gen::write_vendor_workspace`]). `vendor_target`
/// is the directory that cargo will populate — typically
/// `<project>/vendor/`, computed by
/// [`crate::compile::BuildLayout::vendor_dir`].
///
/// Both paths are passed to cargo as absolute paths so a non-default
/// working directory can't confuse it.
pub fn run_cargo_vendor(
    workspace_dir: &Path,
    vendor_target: &Path,
    flags: VendorFlags,
) -> Result<()> {
    let cargo = resolve_cargo();

    let mut cmd = Command::new(&cargo);
    cmd.arg("vendor")
        .arg("--manifest-path")
        .arg(workspace_dir.join("Cargo.toml"))
        .arg(vendor_target);

    if flags.offline {
        cmd.arg("--frozen").arg("--offline");
    }

    // Inherit stdio so cargo's "Downloading …" / "Vendoring …" progress
    // shows up live in the user's terminal.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = cmd.status().map_err(|e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!("failed to spawn `cargo vendor`: {e}")).with_note(format!(
                "tried to invoke {} (set $CARGO to override)",
                cargo.display()
            )),
        ])
    })?;

    if !status.success() {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "`cargo vendor` failed with exit code {:?}",
                status.code()
            ))
            .with_note(format!("scratch workspace: {}", workspace_dir.display()))
            .with_note(format!("target directory: {}", vendor_target.display()))
            .with_note("see cargo's output above for the real error"),
        ]));
    }

    Ok(())
}

/// Resolve which `cargo` binary to invoke. Honors `$CARGO` first
/// (rustup sets this on the outer cargo invocation, so nested calls
/// naturally stick to the same toolchain), otherwise falls back to
/// bare `cargo` on `$PATH`.
fn resolve_cargo() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO") {
        return PathBuf::from(p);
    }
    PathBuf::from("cargo")
}

/// Run `cargo build --release` against the scratch vendor workspace
/// and return the produced artifact paths, keyed by cargo's target
/// name.
///
/// `workspace_dir` is the scratch dir written by
/// [`super::manifest_gen::write_vendor_workspace`]. `target_dir` is
/// where cargo's intermediate state lives (typically
/// `<workspace_dir>/target` so `gluon clean` can wipe it as one unit).
/// `vendor_dir` is the populated `vendor/` from `cargo vendor`; we
/// pass it to cargo via `--config` so the build resolves against the
/// vendored sources rather than going to the network.
///
/// The keys in the returned map are cargo's `target.name` strings —
/// hyphens in package names are converted to underscores by cargo
/// (e.g. `proc-macro2` → `proc_macro2`). The caller is responsible
/// for any normalisation it needs to match user-declared names.
///
/// Only artifacts of kind `lib` or `proc-macro` are returned. Bins,
/// tests, examples, and the scratch workspace's own stub package are
/// filtered out.
pub fn run_cargo_build_vendor(
    workspace_dir: &Path,
    target_dir: &Path,
    vendor_dir: &Path,
    flags: VendorFlags,
) -> Result<BTreeMap<String, PathBuf>> {
    let cargo = resolve_cargo();

    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(workspace_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--message-format=json-render-diagnostics")
        // Force the build to resolve against the vendored sources.
        // Passing this via `--config` (instead of writing a
        // `.cargo/config.toml` next to the scratch) keeps the
        // filesystem footprint to one tree under `build/`. Cargo
        // expects valid TOML in the value position, hence the inline
        // string-with-quotes shape.
        .arg("--config")
        .arg("source.crates-io.replace-with=\"vendored-sources\"")
        .arg("--config")
        .arg(format!(
            "source.vendored-sources.directory={}",
            toml_string_literal(&vendor_dir.to_string_lossy()),
        ));

    if flags.offline {
        cmd.arg("--frozen").arg("--offline");
    }

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    let output = cmd.output().map_err(|e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!("failed to spawn `cargo build`: {e}")).with_note(format!(
                "tried to invoke {} (set $CARGO to override)",
                cargo.display()
            )),
        ])
    })?;

    if !output.status.success() {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "`cargo build` (vendor workspace) failed with exit code {:?}",
                output.status.code()
            ))
            .with_note(format!("scratch workspace: {}", workspace_dir.display()))
            .with_note(format!("target directory: {}", target_dir.display()))
            .with_note("see cargo's output above for the real error"),
        ]));
    }

    let stdout = std::str::from_utf8(&output.stdout).map_err(|e| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "`cargo build` produced non-UTF-8 stdout: {e}"
        ))])
    })?;

    Ok(parse_cargo_build_artifacts(stdout))
}

/// Parse the JSON-lines stdout of `cargo build --message-format=json`
/// and extract the primary artifact path per `compiler-artifact`
/// message of kind `lib` or `proc-macro`.
///
/// Pure function — split out from [`run_cargo_build_vendor`] so unit
/// tests can exercise the parsing logic against canned cargo output
/// without spawning cargo.
///
/// # Selection rules
///
/// - Only messages whose `reason == "compiler-artifact"` are
///   considered.
/// - Only artifacts whose `target.kind` contains `"lib"` or
///   `"proc-macro"` are kept (skips bins / build scripts / tests).
/// - The scratch workspace's stub package
///   (`gluon-vendor-workspace`) is dropped — it has no consumer.
/// - For `lib` artifacts the `.rlib` file is selected from
///   `filenames`. For `proc-macro` artifacts the `.dylib`/`.so`/
///   `.dll` file is selected. Other entries (`.rmeta`, `.d`,
///   intermediate object files) are ignored.
/// - When a target appears more than once (cargo can re-emit on
///   incremental rebuilds), the last entry wins. Cargo guarantees
///   the final artifact matches what's on disk.
///
/// Malformed lines (non-JSON, missing fields) are silently skipped
/// rather than erroring — `--message-format=json` interleaves
/// `compiler-message` events and other reasons we don't care about,
/// and cargo's vocabulary may grow over time. The selection criteria
/// are positive (we only keep things we recognise), so unknown
/// reasons are inherently safe.
pub fn parse_cargo_build_artifacts(stdout: &str) -> BTreeMap<String, PathBuf> {
    let mut out: BTreeMap<String, PathBuf> = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(value): std::result::Result<serde_json::Value, _> = serde_json::from_str(line)
        else {
            continue;
        };
        if value.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let Some(target) = value.get("target") else {
            continue;
        };
        let Some(name) = target.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name == "gluon-vendor-workspace" || name == "gluon_vendor_workspace" {
            continue;
        }
        let Some(kinds) = target.get("kind").and_then(|v| v.as_array()) else {
            continue;
        };
        let kind_strs: Vec<&str> = kinds.iter().filter_map(|k| k.as_str()).collect();
        let is_lib = kind_strs.contains(&"lib");
        let is_proc_macro = kind_strs.contains(&"proc-macro");
        if !is_lib && !is_proc_macro {
            continue;
        }
        let Some(filenames) = value.get("filenames").and_then(|v| v.as_array()) else {
            continue;
        };
        let pick: Option<PathBuf> = filenames.iter().filter_map(|f| f.as_str()).find_map(|p| {
            // Proc-macros are dylibs that the compiler dlopens, so the
            // platform-specific extension matters; libs are always
            // `.rlib`. Use case-insensitive matching for the dynamic
            // extensions because Windows is.
            let lower = p.to_ascii_lowercase();
            let proc_macro_ext = is_proc_macro
                && (lower.ends_with(".dylib") || lower.ends_with(".so") || lower.ends_with(".dll"));
            let lib_ext = is_lib && lower.ends_with(".rlib");
            (proc_macro_ext || lib_ext).then(|| PathBuf::from(p))
        });
        if let Some(path) = pick {
            out.insert(name.to_string(), path);
        }
    }
    out
}

/// Run `cargo metadata --format-version 1 --frozen` against the scratch
/// vendor workspace and return the captured JSON stdout.
///
/// `--frozen` is always passed: by the time we call metadata, `cargo
/// vendor` has populated the workspace's lockfile, and any need to
/// update it would mean the vendor dir is out of sync with the
/// declared deps — surfacing that as a hard cargo error keeps the
/// build deterministic.
///
/// `--offline` is gated on [`VendorFlags::offline`], same as
/// [`run_cargo_vendor`]. When set, cargo additionally refuses any
/// network access.
///
/// `vendor_dir` redirects cargo to resolve crates.io packages from the
/// vendored copy rather than the global registry cache. Without this,
/// `manifest_path` entries in the JSON point at
/// `~/.cargo/registry/src/...`, so the downstream rustc dispatch path
/// would compile from the cargo cache instead of the vendored sources
/// — silently breaking offline-build and reproducibility guarantees.
/// Same `--config` shape as [`run_cargo_build_vendor`] uses for the
/// host pipeline; we keep both invocations consistent rather than
/// writing a `.cargo/config.toml` next to the scratch.
///
/// Returns the JSON string for downstream parsing by
/// [`super::cross::metadata::parse_metadata`]. Splitting subprocess
/// invocation from JSON parsing keeps each side independently
/// testable: the parser exercises canned JSON without spawning cargo,
/// and this function is exercised against a real toolchain in the
/// orchestration tests without needing the full graph machinery.
pub fn run_cargo_metadata(
    workspace_dir: &Path,
    vendor_dir: &Path,
    flags: VendorFlags,
) -> Result<String> {
    let cargo = resolve_cargo();

    let mut cmd = Command::new(&cargo);
    cmd.arg("metadata")
        .arg("--manifest-path")
        .arg(workspace_dir.join("Cargo.toml"))
        .arg("--format-version")
        .arg("1")
        .arg("--frozen")
        .arg("--config")
        .arg("source.crates-io.replace-with=\"vendored-sources\"")
        .arg("--config")
        .arg(format!(
            "source.vendored-sources.directory={}",
            toml_string_literal(&vendor_dir.to_string_lossy()),
        ));

    if flags.offline {
        cmd.arg("--offline");
    }

    // Capture stderr so the failure path can include cargo's diagnostic
    // text in the gluon error (unlike vendor, which inherits stderr to
    // show live progress — metadata is a quick query and produces no
    // user-facing progress, so capturing keeps the surface clean).
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().map_err(|e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!("failed to spawn `cargo metadata`: {e}")).with_note(format!(
                "tried to invoke {} (set $CARGO to override)",
                cargo.display()
            )),
        ])
    })?;

    if !output.status.success() {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "`cargo metadata` failed with exit code {:?}",
                output.status.code()
            ))
            .with_note(format!("scratch workspace: {}", workspace_dir.display()))
            .with_note(format!(
                "stderr:\n{}",
                String::from_utf8_lossy(&output.stderr).trim_end()
            )),
        ]));
    }

    String::from_utf8(output.stdout).map_err(|e| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "`cargo metadata` produced non-UTF-8 stdout: {e}"
        ))])
    })
}

/// Quote a string literal for cargo's `--config` flag (TOML basic
/// string). Mirrors what we do for `escape_toml_str` in
/// [`super::manifest_gen`] but inlines the wrapping quotes since
/// `--config` parses values as TOML.
fn toml_string_literal(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cargo_honors_env_var() {
        // `temp_env::with_var` serializes through a global mutex and
        // restores prior state on drop, so this is safe under cargo's
        // parallel test runner.
        let sentinel = "/tmp/gluon-vendor-test-sentinel-cargo";
        temp_env::with_var("CARGO", Some(sentinel), || {
            assert_eq!(resolve_cargo(), PathBuf::from(sentinel));
        });
        temp_env::with_var("CARGO", None::<&str>, || {
            assert_eq!(resolve_cargo(), PathBuf::from("cargo"));
        });
    }

    #[test]
    fn parse_picks_rlib_for_lib_artifact() {
        // Trimmed cargo output for syn 2.0.117 — kept terse for
        // readability. Real cargo output has many more fields; only
        // the ones we read are required.
        let stdout = r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#syn@2.0.117","target":{"name":"syn","kind":["lib"]},"filenames":["/tmp/build/release/deps/libsyn-abcdef.rlib","/tmp/build/release/deps/libsyn-abcdef.rmeta"]}"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("syn"),
            Some(&PathBuf::from("/tmp/build/release/deps/libsyn-abcdef.rlib")),
        );
    }

    #[test]
    fn parse_picks_dylib_for_proc_macro_artifact() {
        let stdout = r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#proc-macro2@1.0.106","target":{"name":"proc_macro2","kind":["proc-macro"]},"filenames":["/tmp/build/release/deps/libproc_macro2-xyz.dylib","/tmp/build/release/deps/libproc_macro2-xyz.rmeta"]}"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert_eq!(map.len(), 1);
        // Note the underscore — cargo normalises the package's hyphen.
        assert_eq!(
            map.get("proc_macro2"),
            Some(&PathBuf::from(
                "/tmp/build/release/deps/libproc_macro2-xyz.dylib"
            )),
        );
    }

    #[test]
    fn parse_handles_so_and_dll_for_proc_macro() {
        let so_line = r#"{"reason":"compiler-artifact","target":{"name":"foo","kind":["proc-macro"]},"filenames":["/x/libfoo-1.so"]}"#;
        let dll_line = r#"{"reason":"compiler-artifact","target":{"name":"bar","kind":["proc-macro"]},"filenames":["C:\\x\\bar-1.dll"]}"#;
        let map = parse_cargo_build_artifacts(&format!("{so_line}\n{dll_line}\n"));
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("foo").map(|p| p.to_string_lossy().into_owned()),
            Some("/x/libfoo-1.so".into())
        );
        assert_eq!(
            map.get("bar").map(|p| p.to_string_lossy().into_owned()),
            Some(r"C:\x\bar-1.dll".into())
        );
    }

    #[test]
    fn parse_skips_non_compiler_artifact_messages() {
        // build-finished, compiler-message (a diagnostic), and
        // build-script-executed all appear in real cargo output and
        // must not produce map entries.
        let stdout = r#"{"reason":"build-script-executed","package_id":"x","linked_libs":[]}
{"reason":"compiler-message","package_id":"x","message":{"rendered":"warning: ..."}}
{"reason":"build-finished","success":true}
"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert!(map.is_empty(), "got: {map:?}");
    }

    #[test]
    fn parse_skips_workspace_stub_package() {
        // The scratch workspace's own `[lib]` entry produces a
        // compiler-artifact too — we must not record it because no
        // consumer would ever --extern against it.
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"gluon-vendor-workspace","kind":["lib"]},"filenames":["/tmp/release/deps/libgluon_vendor_workspace-x.rlib"]}
{"reason":"compiler-artifact","target":{"name":"gluon_vendor_workspace","kind":["lib"]},"filenames":["/tmp/release/deps/libgluon_vendor_workspace-y.rlib"]}"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert!(map.is_empty(), "stub leaked: {map:?}");
    }

    #[test]
    fn parse_skips_bin_and_test_kinds() {
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"runme","kind":["bin"]},"filenames":["/tmp/runme"]}
{"reason":"compiler-artifact","target":{"name":"my_test","kind":["test"]},"filenames":["/tmp/test-bin"]}"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert!(map.is_empty(), "got: {map:?}");
    }

    #[test]
    fn parse_last_entry_wins_for_duplicate_targets() {
        // Cargo can re-emit on incremental rebuilds; the latest entry
        // is the one that matches what's on disk now.
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"syn","kind":["lib"]},"filenames":["/tmp/old/libsyn-old.rlib"]}
{"reason":"compiler-artifact","target":{"name":"syn","kind":["lib"]},"filenames":["/tmp/new/libsyn-new.rlib"]}"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert_eq!(
            map.get("syn"),
            Some(&PathBuf::from("/tmp/new/libsyn-new.rlib")),
            "later entry must override earlier"
        );
    }

    #[test]
    fn parse_tolerates_blank_and_garbage_lines() {
        let stdout = r#"
not json at all
{"reason":"compiler-artifact","target":{"name":"x","kind":["lib"]},"filenames":["/p/libx-1.rlib"]}

"#;
        let map = parse_cargo_build_artifacts(stdout);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("x"));
    }

    #[test]
    fn toml_string_literal_quotes_and_escapes() {
        assert_eq!(toml_string_literal("/tmp/v"), r#""/tmp/v""#);
        assert_eq!(toml_string_literal(r"C:\x"), r#""C:\\x""#);
        assert_eq!(toml_string_literal(r#"with"quote"#), r#""with\"quote""#);
    }

    #[test]
    fn spawn_failure_is_diagnosed() {
        // Point $CARGO at a binary that cannot possibly exist so we
        // exercise the spawn-error arm of run_cargo_vendor.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            b"[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(ws.join("lib.rs"), b"").unwrap();

        let bogus = "/nonexistent/gluon-vendor-test-cargo-missing";
        let result = temp_env::with_var("CARGO", Some(bogus), || {
            run_cargo_vendor(&ws, &tmp.path().join("out"), VendorFlags::default())
        });

        let err = result.expect_err("must fail — bogus CARGO path");
        let msg = err.to_string();
        assert!(msg.contains("failed to spawn"), "msg: {msg}");
        assert!(msg.contains(bogus), "msg: {msg}");
    }

    #[test]
    fn metadata_spawn_failure_is_diagnosed() {
        // Same shape as spawn_failure_is_diagnosed but exercises the
        // `cargo metadata` arm. Confirms the diagnostic includes both
        // the headline ("failed to spawn `cargo metadata`") and the
        // bogus path so the user can tell which subprocess failed.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            b"[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(ws.join("lib.rs"), b"").unwrap();

        let bogus = "/nonexistent/gluon-vendor-test-cargo-metadata-missing";
        let vendor = ws.join("vendor");
        let result = temp_env::with_var("CARGO", Some(bogus), || {
            run_cargo_metadata(&ws, &vendor, VendorFlags::default())
        });

        let err = result.expect_err("must fail — bogus CARGO path");
        let msg = err.to_string();
        assert!(msg.contains("failed to spawn"), "msg: {msg}");
        assert!(msg.contains("cargo metadata"), "msg: {msg}");
        assert!(msg.contains(bogus), "msg: {msg}");
    }

    #[test]
    fn metadata_failure_includes_cargo_stderr_in_diagnostic() {
        // Run real cargo against an invalid manifest so cargo exits
        // non-zero and writes a diagnostic to stderr. Confirms the
        // run_cargo_metadata error path captures and surfaces it.
        // Skipped if cargo isn't on $PATH (shouldn't happen in this
        // workspace's CI, but be defensive).
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped — cargo not on $PATH");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Deliberately malformed: missing `[package]` section produces
        // a precise, stable cargo diagnostic on stderr.
        std::fs::write(
            ws.join("Cargo.toml"),
            b"this is not valid toml syntax !!!\n",
        )
        .unwrap();

        let vendor = ws.join("vendor");
        let err = run_cargo_metadata(&ws, &vendor, VendorFlags::default())
            .expect_err("invalid manifest must fail");
        let msg = err.to_string();
        assert!(msg.contains("`cargo metadata` failed"), "msg: {msg}");
        assert!(msg.contains("stderr:"), "msg: {msg}");
        // Cargo's exact wording for parse errors has shifted across
        // versions ("could not parse" vs "expected" etc.), but the
        // failed path is always quoted in stderr — assert that.
        assert!(
            msg.contains(ws.join("Cargo.toml").to_string_lossy().as_ref()),
            "stderr capture must include the offending manifest path: {msg}",
        );
    }
}
