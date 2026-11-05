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
/// `<build_dir>/script-runner/scripts/<key>/gluon_script[.exe]`.
/// Returns the path to the binary (cached or freshly built).
///
/// The lookup is two-tier: the outer directory is keyed by
/// `entrypoint + rustc + rlib` (cheap to recompute on every invocation),
/// and the inner check verifies that the on-disk transitive fingerprint
/// still matches the current hash of every file the script's depfile
/// lists. The fingerprint catches edits to files reached via `mod` /
/// `#[path]` from the entrypoint, which would otherwise leave the outer
/// key unchanged, so both layers are needed.
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

    if entry_dir.is_dir() && lookup_cached_binary(&entry_dir)? {
        return Ok(bin_path);
    }

    std::fs::create_dir_all(&entry_dir).map_err(|e| Error::Io {
        path: entry_dir.clone(),
        source: e,
    })?;
    compile_script(script_path, &entry_dir, rustc_info, rlib)?;
    write_transitive_fingerprint(&entry_dir)?;
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

/// Filename of the on-disk transitive-source fingerprint.
const TRANSITIVE_FP_NAME: &str = "transitive-fingerprint.txt";

/// SHA-256 over the contents of every file the script's depfile lists.
///
/// Used as the secondary cache key for the script-runner: the outer
/// directory is keyed by the entrypoint hash (cheap to recompute on
/// every invocation), and this fingerprint catches edits to any file
/// reached via `mod` / `#[path]` from the entrypoint. It lives outside
/// the entrypoint key because computing it means reading every file the
/// depfile lists, which the cheap outer lookup avoids.
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

/// Returns `Ok(true)` if `entry_dir` contains a complete cache entry whose
/// stored transitive fingerprint matches a freshly recomputed one;
/// `Ok(false)` for "miss; the caller should (re)compile". `Err` is reserved
/// for I/O errors raised while reading the fingerprint file itself —
/// failures during the secondary `transitive_fingerprint` recomputation
/// (a missing source listed in the depfile, an unreadable depfile, etc.)
/// deliberately collapse to a miss. The recompile that follows overwrites
/// the depfile and fingerprint, and any genuine source-tree problem gets
/// surfaced by rustc with full diagnostic context — strictly better than
/// masking it here as a "cache lookup failure".
fn lookup_cached_binary(entry_dir: &Path) -> Result<bool> {
    let bin = entry_dir.join(SCRIPT_BIN_NAME);
    let depfile = entry_dir.join("gluon_script.d");
    let fp_file = entry_dir.join(TRANSITIVE_FP_NAME);

    if !bin.is_file() || !depfile.is_file() || !fp_file.is_file() {
        return Ok(false);
    }

    let stored_fp = std::fs::read_to_string(&fp_file).map_err(|e| Error::Io {
        path: fp_file.clone(),
        source: e,
    })?;
    let current_fp = match transitive_fingerprint(&depfile) {
        Ok(fp) => fp,
        // Any failure here — depfile lists a missing file, depfile is
        // corrupted, source file unreadable — collapses to a miss. The
        // recompile is self-healing for the cache layer (overwrites the
        // depfile and fingerprint) and forwards any real source-tree
        // problem to rustc, which produces the canonical diagnostic.
        Err(_) => return Ok(false),
    };

    Ok(stored_fp.trim() == current_fp.trim())
}

/// Persist the transitive fingerprint for the freshly-compiled binary.
/// Reads the depfile rustc just emitted and writes the hash to
/// `<entry_dir>/transitive-fingerprint.txt`. Errors propagate — failure
/// here means a future `ensure_script_binary` call would treat this
/// entry as a miss and recompile, which is wasteful but correct.
fn write_transitive_fingerprint(entry_dir: &Path) -> Result<()> {
    let depfile = entry_dir.join("gluon_script.d");
    let fp = transitive_fingerprint(&depfile)?;
    let target = entry_dir.join(TRANSITIVE_FP_NAME);
    std::fs::write(&target, fp).map_err(|e| Error::Io {
        path: target,
        source: e,
    })?;
    Ok(())
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
    use std::ffi::OsString;
    use std::process::Command;

    let extern_arg = format!("gluon_config={}", rlib.gluon_config_rlib.display());
    let l_dependency = format!("dependency={}", rlib.deps_dir.display());

    // Pinning the depfile path explicitly (rather than letting rustc derive
    // `<crate-name>.d`) decouples the cache from any future `-C extra-filename`
    // additions — the depfile lands exactly where `transitive_fingerprint`
    // expects to find it.
    let depfile_path = entry_dir.join("gluon_script.d");
    let mut emit_arg = OsString::from("link,dep-info=");
    emit_arg.push(depfile_path.as_os_str());

    // We invoke rustc directly (not via cargo) so the user's gluon.rs
    // doesn't need a Cargo.toml. The flags mirror what a minimal
    // `cargo build` would emit, plus:
    //   --out-dir <entry_dir>     — rustc places `gluon_script` here
    //                               (no extension on unix, `.exe` on
    //                               Windows). This replaces the previous
    //                               `-o <path>` model.
    //   --emit link,dep-info=<p>  — `link` is the default and produces the
    //                               binary; `dep-info=<p>` produces the
    //                               depfile we need for transitive cache
    //                               invalidation, with the path pinned
    //                               (see crate::script_runner::cache::transitive_fingerprint).
    //
    // Path arguments are passed as `&OsStr` (via `Path::as_os_str`) rather
    // than going through `.display().to_string()` so non-UTF-8 paths
    // survive intact — see the same pattern in
    // `crate::compile::rustc::RustcCommandBuilder`.
    let output = Command::new(&rustc_info.rustc_path)
        .arg("--error-format")
        .arg("human")
        .arg("--edition")
        .arg("2024")
        .arg("--extern")
        .arg(&extern_arg)
        .arg("-L")
        .arg(&l_dependency)
        .arg("--crate-type")
        .arg("bin")
        .arg("--crate-name")
        .arg("gluon_script")
        .arg("--emit")
        .arg(&emit_arg)
        .arg("--out-dir")
        .arg(entry_dir.as_os_str())
        .arg(script_path.as_os_str())
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

        assert_ne!(
            fp1, fp2,
            "edit to a listed file must change the fingerprint"
        );
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

    // Helpers for the integration-style cache tests below. We can't run
    // the real rustc through `ensure_script_binary` in a unit test
    // (~2 s, plus we'd need a real rlib), so these tests stub the
    // compile by short-circuiting the entry-dir layout: we hand-write
    // the binary, depfile, and fingerprint that a successful compile
    // would have produced, then exercise the lookup code.
    fn make_fake_compile(
        entry_dir: &Path,
        bin_bytes: &[u8],
        depfile_lines: &str,
        sources: &[(PathBuf, &[u8])],
    ) {
        fs::create_dir_all(entry_dir).unwrap();
        for (p, b) in sources {
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, b).unwrap();
        }
        fs::write(entry_dir.join(SCRIPT_BIN_NAME), bin_bytes).unwrap();
        fs::write(entry_dir.join("gluon_script.d"), depfile_lines).unwrap();
        let fp = transitive_fingerprint(&entry_dir.join("gluon_script.d")).unwrap();
        fs::write(entry_dir.join(TRANSITIVE_FP_NAME), fp).unwrap();
    }

    #[test]
    fn cache_hit_when_no_files_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("entry");
        let src = tmp.path().join("script.rs");
        let bin_v1 = b"V1 BINARY";
        make_fake_compile(
            &entry,
            bin_v1,
            &format!("script: {}\n", src.display()),
            &[(src.clone(), b"// stable")],
        );

        let hit = lookup_cached_binary(&entry).expect("lookup");
        assert!(hit, "stable inputs must report a cache hit");
        assert_eq!(
            fs::read(entry.join(SCRIPT_BIN_NAME)).unwrap(),
            bin_v1,
            "cache hit must NOT have rewritten the binary"
        );
    }

    #[test]
    fn cache_miss_when_listed_file_edited() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("entry");
        let src = tmp.path().join("subsys.rs");
        make_fake_compile(
            &entry,
            b"V1",
            &format!("script: {}\n", src.display()),
            &[(src.clone(), b"// v1")],
        );

        // User edits the subsystem file — fingerprint stored on disk
        // no longer matches what we'd compute now.
        fs::write(&src, b"// v2 EDITED").unwrap();

        let hit = lookup_cached_binary(&entry).expect("lookup");
        assert!(!hit, "edited subsystem file must trigger a cache miss");
    }

    #[test]
    fn cache_miss_when_listed_file_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("entry");
        let src = tmp.path().join("vanishing.rs");
        make_fake_compile(
            &entry,
            b"V1",
            &format!("script: {}\n", src.display()),
            &[(src.clone(), b"// soon to vanish")],
        );

        fs::remove_file(&src).unwrap();

        let hit = lookup_cached_binary(&entry).expect("lookup");
        assert!(!hit, "removed listed file must trigger a cache miss");
    }

    #[test]
    fn cache_miss_when_no_fingerprint_yet() {
        // Bare entry dir with binary but no depfile / no fingerprint.
        // This is the state of an outer dir from before this feature
        // landed — must report miss so we recompile and seed.
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("entry");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join(SCRIPT_BIN_NAME), b"old binary").unwrap();

        let hit = lookup_cached_binary(&entry).expect("lookup");
        assert!(!hit, "missing fingerprint must report miss");
    }
}
