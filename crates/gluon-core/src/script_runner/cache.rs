//! Per-`gluon.rs` script binary cache.
//!
//! ## What this caches
//!
//! Compiling a user's `gluon.rs` with rustc takes ~1–2s on a warm
//! gluon-config rlib. Running it takes ~50ms. We cache the compiled
//! binary so subsequent `gluon build` invocations skip rustc entirely.
//!
//! ## Cache key
//!
//! `sha256(rustc_version || sha256(gluon.rs) || sha256(gluon_config_rlib))`
//!
//! - `rustc_version` covers "the user upgraded their toolchain".
//! - The script-source hash covers any edit to `gluon.rs`.
//! - The rlib hash covers a rebuild of `gluon-config` (which would
//!   change ABI/layout and require relinking the script).
//!
//! Anything outside that triple — env vars, working directory, etc. —
//! does NOT invalidate. That's deliberate: those don't affect the
//! compiled binary's behaviour. Re-running a cached binary is free, so
//! the cost of a missed invalidation is bounded.
//!
//! ## What's not cached
//!
//! Stderr from rustc on a *successful* compile is discarded. On
//! failure we surface the full stderr verbatim through `Error::Config`.
//! There is no diagnostic-level structured-error path yet — Stage 4
//! may upgrade this to parse `--error-format=json` and emit
//! `Error::Diagnostics`.

use crate::compile::RustcInfo;
use crate::error::{Error, Result};
use crate::script_runner::rlib::RlibPaths;
use std::path::{Path, PathBuf};

/// Compile `script_path` into a binary cached under
/// `<build_dir>/script-runner/scripts/<key>/gluon-script`. Returns the
/// path to the binary (cached or freshly built).
pub fn ensure_script_binary(
    script_path: &Path,
    build_dir: &Path,
    rustc_info: &RustcInfo,
    rlib: &RlibPaths,
) -> Result<PathBuf> {
    let cache_root = build_dir.join("script-runner").join("scripts");
    let key = compute_key(script_path, &rustc_info.version, &rlib.gluon_config_rlib)?;
    let entry_dir = cache_root.join(&key);
    let bin_path = entry_dir.join(SCRIPT_BIN_NAME);

    if bin_path.is_file() {
        return Ok(bin_path);
    }

    std::fs::create_dir_all(&entry_dir).map_err(|e| Error::Io {
        path: entry_dir.clone(),
        source: e,
    })?;
    compile_script(script_path, &entry_dir, rustc_info, rlib)?;
    Ok(bin_path)
}

/// Filename of the cached script binary inside its entry dir.
///
/// Underscored to match `--crate-name gluon_script` — rustc emits the
/// binary with the crate name (no extension on unix; `.exe` on Windows)
/// when invoked with `--out-dir` instead of `-o`. The on-disk name
/// is never user-visible; we keep the `_` form solely to match what
/// rustc writes.
#[cfg(not(windows))]
const SCRIPT_BIN_NAME: &str = "gluon_script";
#[cfg(windows)]
const SCRIPT_BIN_NAME: &str = "gluon_script.exe";

/// SHA-256 over the contents of every file the script's depfile lists.
///
/// Used as the secondary cache key for the script-runner: the outer
/// directory is keyed by the entrypoint hash (cheap to recompute on
/// every invocation), and this fingerprint catches edits to any file
/// reached via `mod` / `#[path]` from the entrypoint. See the
/// "Script-runner cache invariant change" section of
/// `docs/superpowers/specs/2026-05-05-modular-workspace-config-design.md`
/// for why this lives outside the entrypoint key.
///
/// File order matters for the hash — we feed paths in the order rustc
/// listed them, plus the file contents, so a permutation of the depfile
/// would produce a different fingerprint. In practice rustc emits
/// depfile entries deterministically for a given source set; if that
/// ever changed, the consequence would be a spurious recompile (not a
/// stale build), so we don't sort the list to harden against it.
fn transitive_fingerprint(depfile: &Path) -> Result<String> {
    use crate::cache::{parse_depfile, sha256_file};
    use sha2::{Digest, Sha256};

    let sources = parse_depfile(depfile)?;
    let mut h = Sha256::new();
    for src in &sources {
        // Include the path bytes so two files with identical contents
        // at different paths still affect the hash — this catches
        // "subsystem renamed but contents identical" cases.
        h.update(src.as_os_str().as_encoded_bytes());
        h.update(b"\0");
        let bytes = sha256_file(src).map_err(|e| match e {
            Error::Io { source, .. } => Error::Io {
                path: src.clone(),
                source,
            },
            other => other,
        })?;
        h.update(bytes);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn compute_key(script_path: &Path, rustc_version: &str, rlib: &Path) -> Result<String> {
    use crate::cache::sha256_file;
    use sha2::{Digest, Sha256};

    let mut h = Sha256::new();
    h.update(rustc_version.as_bytes());
    let s = sha256_file(script_path).map_err(|e| match e {
        Error::Io { source, .. } => Error::Io {
            path: script_path.to_path_buf(),
            source,
        },
        other => other,
    })?;
    h.update(s);
    let r = sha256_file(rlib).map_err(|e| match e {
        Error::Io { source, .. } => Error::Io {
            path: rlib.to_path_buf(),
            source,
        },
        other => other,
    })?;
    h.update(r);
    Ok(format!("{:x}", h.finalize()))
}

fn compile_script(
    script_path: &Path,
    entry_dir: &Path,
    rustc_info: &RustcInfo,
    rlib: &RlibPaths,
) -> Result<()> {
    use std::process::Command;

    let extern_arg = format!("gluon_config={}", rlib.gluon_config_rlib.display());
    let l_dependency = format!("dependency={}", rlib.deps_dir.display());

    // We invoke rustc directly (not via cargo) so the user's gluon.rs
    // doesn't need a Cargo.toml. The flags mirror what a minimal
    // `cargo build` would emit, plus:
    //   --out-dir <entry_dir>     — rustc places `gluon_script` AND
    //                               `gluon_script.d` here under predictable
    //                               names. This replaces the previous
    //                               `-o <path>` model so the depfile
    //                               lands beside the binary.
    //   --emit link,dep-info      — `link` is the default and produces the
    //                               binary; `dep-info` produces the depfile
    //                               we need for transitive cache invalidation
    //                               (see crate::script_runner::cache::transitive_fingerprint).
    let output = Command::new(&rustc_info.rustc_path)
        .args([
            "--error-format",
            "human",
            "--edition",
            "2024",
            "--extern",
            &extern_arg,
            "-L",
            &l_dependency,
            "--crate-type",
            "bin",
            "--crate-name",
            "gluon_script",
            "--emit",
            "link,dep-info",
            "--out-dir",
            &entry_dir.display().to_string(),
            &script_path.display().to_string(),
        ])
        .output()
        .map_err(|e| Error::Io {
            path: rustc_info.rustc_path.clone(),
            source: e,
        })?;

    if !output.status.success() {
        return Err(Error::Config(format!(
            "rustc failed compiling {} (status {}):\n{}",
            script_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn compute_key_changes_with_script_content() {
        let tmp = tempfile::tempdir().unwrap();
        let s1 = tmp.path().join("a.rs");
        let s2 = tmp.path().join("b.rs");
        let rlib = tmp.path().join("librlib.rlib");
        fs::write(&s1, b"fn main() {}").unwrap();
        fs::write(&s2, b"fn main() { /* different */ }").unwrap();
        fs::write(&rlib, b"fake rlib bytes").unwrap();

        let k1 = compute_key(&s1, "rustc 1", &rlib).expect("k1");
        let k2 = compute_key(&s2, "rustc 1", &rlib).expect("k2");
        assert_ne!(k1, k2, "different scripts must produce different keys");
    }

    #[test]
    fn compute_key_changes_with_rlib_content() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("a.rs");
        let r1 = tmp.path().join("r1.rlib");
        let r2 = tmp.path().join("r2.rlib");
        fs::write(&s, b"fn main() {}").unwrap();
        fs::write(&r1, b"first").unwrap();
        fs::write(&r2, b"second").unwrap();

        let k1 = compute_key(&s, "rustc 1", &r1).expect("k1");
        let k2 = compute_key(&s, "rustc 1", &r2).expect("k2");
        assert_ne!(k1, k2, "different rlibs must produce different keys");
    }

    #[test]
    fn compute_key_changes_with_rustc_version() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("a.rs");
        let r = tmp.path().join("r.rlib");
        fs::write(&s, b"fn main() {}").unwrap();
        fs::write(&r, b"rlib").unwrap();

        let k1 = compute_key(&s, "rustc 1.85", &r).expect("k1");
        let k2 = compute_key(&s, "rustc 1.86", &r).expect("k2");
        assert_ne!(
            k1, k2,
            "different rustc versions must produce different keys"
        );
    }

    #[test]
    fn transitive_fingerprint_changes_when_listed_file_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        let b = tmp.path().join("b.rs");
        fs::write(&a, b"// a v1").unwrap();
        fs::write(&b, b"// b v1").unwrap();
        let depfile = tmp.path().join("script.d");
        fs::write(
            &depfile,
            format!("script: {} {}\n", a.display(), b.display()),
        )
        .unwrap();

        let fp1 = transitive_fingerprint(&depfile).expect("fp1");
        fs::write(&b, b"// b v2 - edited").unwrap();
        let fp2 = transitive_fingerprint(&depfile).expect("fp2");

        assert_ne!(fp1, fp2, "edit to a listed file must change the fingerprint");
    }

    #[test]
    fn transitive_fingerprint_is_deterministic_for_same_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        fs::write(&a, b"// stable").unwrap();
        let depfile = tmp.path().join("script.d");
        fs::write(&depfile, format!("script: {}\n", a.display())).unwrap();

        let fp1 = transitive_fingerprint(&depfile).expect("fp1");
        let fp2 = transitive_fingerprint(&depfile).expect("fp2");
        assert_eq!(fp1, fp2, "same inputs must produce same fingerprint");
    }

    #[test]
    fn transitive_fingerprint_changes_when_path_changes_with_identical_content() {
        // Guards the invariant in transitive_fingerprint's inline comment:
        // two files with identical bytes at different paths must produce
        // different fingerprints.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        let b = tmp.path().join("b.rs");
        let same = b"// identical bytes";
        fs::write(&a, same).unwrap();
        fs::write(&b, same).unwrap();

        let depfile_a = tmp.path().join("script-a.d");
        let depfile_b = tmp.path().join("script-b.d");
        fs::write(&depfile_a, format!("script: {}\n", a.display())).unwrap();
        fs::write(&depfile_b, format!("script: {}\n", b.display())).unwrap();

        let fp_a = transitive_fingerprint(&depfile_a).expect("fp_a");
        let fp_b = transitive_fingerprint(&depfile_b).expect("fp_b");
        assert_ne!(
            fp_a, fp_b,
            "different paths with identical content must produce different fingerprints"
        );
    }

    #[test]
    fn transitive_fingerprint_errors_when_listed_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let depfile = tmp.path().join("script.d");
        fs::write(
            &depfile,
            format!("script: {}\n", tmp.path().join("ghost.rs").display()),
        )
        .unwrap();

        assert!(
            transitive_fingerprint(&depfile).is_err(),
            "missing listed file must surface as an error"
        );
    }
}
