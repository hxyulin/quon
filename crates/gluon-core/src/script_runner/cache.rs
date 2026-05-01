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
    compile_script(script_path, &bin_path, rustc_info, rlib)?;
    Ok(bin_path)
}

/// Filename of the cached script binary inside its entry dir.
///
/// Kept consistent (no `.exe` on Windows) because rustc accepts the
/// extensionless name on every host we support — and `Command::new`
/// resolves it without help. If we ever ship Windows builds we'll
/// adjust this to a `cfg!(windows)` switch.
const SCRIPT_BIN_NAME: &str = "gluon-script";

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
    out_path: &Path,
    rustc_info: &RustcInfo,
    rlib: &RlibPaths,
) -> Result<()> {
    use std::process::Command;

    let extern_arg = format!("gluon_config={}", rlib.gluon_config_rlib.display());
    let l_dependency = format!("dependency={}", rlib.deps_dir.display());

    // We invoke rustc directly (not via cargo) so the user's gluon.rs
    // doesn't need a Cargo.toml. The flags mirror what a minimal
    // `cargo build` would emit:
    //   --error-format human      — see comment below.
    //   --edition 2024            — match the workspace edition; user's
    //                               script writes idiomatic modern Rust.
    //   --extern gluon_config=... — the precompiled rlib the script imports.
    //   -L dependency=...         — directory holding gluon_config's
    //                               transitive rlib deps (serde, etc.).
    //   --crate-type bin          — the script is an executable; we'll
    //                               spawn it and capture stdout.
    //   --crate-name gluon-script — stable name for diagnostics; never
    //                               surfaces to the user.
    let output = Command::new(&rustc_info.rustc_path)
        .args([
            // Force human-readable errors regardless of whether stderr is
            // piped — cargo's terminal-detection default would otherwise
            // swallow column info when our subprocess capture redirects.
            "--error-format",
            "human",
            // Stage 1 assumption: gluon-config and gluon-model both use
            // edition 2024 (matches workspace.package.edition in the root
            // Cargo.toml). If the workspace edition ever changes, this
            // constant must change in lockstep or the user's gluon.rs will
            // fail with a confusing edition-mismatch error. Stage 4+ may
            // read this from gluon-config's Cargo.toml at build.rs time and
            // bake it via env!() so it stays in sync.
            "--edition",
            "2024",
            "--extern",
            &extern_arg,
            "-L",
            &l_dependency,
            "--crate-type",
            "bin",
            // rustc rejects hyphens in `--crate-name`; we expose the
            // hyphenated name via the output filename instead.
            "--crate-name",
            "gluon_script",
            "-o",
            &out_path.display().to_string(),
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
}
