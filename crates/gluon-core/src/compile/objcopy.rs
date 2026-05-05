//! Locate and invoke `rust-objcopy` for post-link binary transforms.
//!
//! `rust-objcopy` ships with the rustup `llvm-tools-preview` component
//! and lives next to `rustc` in the active toolchain's `bin/` directory.
//! We prefer the toolchain-sibling binary over `$PATH` candidates so a
//! user who has rustup-toolchain-pinned to nightly-X gets the same
//! `rust-objcopy` rustc itself was built against — avoids a class of
//! "ELF emitted by rustc-1.95 but stripped by an older host
//! `rust-objcopy`-1.86" mismatches.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Find a usable `rust-objcopy` binary.
///
/// Resolution order (first hit wins):
///
/// 1. `rust-objcopy` (or `rust-objcopy.exe`) in the same directory as
///    `rustc_path`.
/// 2. `rust-objcopy` on `$PATH`.
/// 3. `llvm-objcopy` on `$PATH` — the upstream LLVM binary, behaviour-
///    compatible for our `-O binary` use case.
///
/// Returns the resolved absolute path, or an error with a `rustup
/// component add llvm-tools-preview` hint if none of the above resolve.
pub fn probe_rust_objcopy(rustc_path: &Path) -> Result<PathBuf> {
    if let Some(p) = sibling_of(rustc_path, "rust-objcopy") {
        return Ok(p);
    }
    if let Some(p) = which_on_path("rust-objcopy") {
        return Ok(p);
    }
    if let Some(p) = which_on_path("llvm-objcopy") {
        return Ok(p);
    }
    Err(Error::Config(
        "rust-objcopy not found. RawBinary crates need it to convert ELF\n\
         to flat binary output. Install with:\n    \
         rustup component add llvm-tools-preview"
            .into(),
    ))
}

/// Apply the platform-specific executable suffix to `base`. On Windows
/// returns `<base>.exe`; elsewhere returns `<base>` unchanged.
fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_owned()
    }
}

fn sibling_of(rustc_path: &Path, name: &str) -> Option<PathBuf> {
    let dir = rustc_path.parent()?;
    let exe = exe_name(name);
    let candidate = dir.join(exe);
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe = exe_name(name);
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn touch_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, b"#!/bin/true\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(windows)]
    fn touch_executable(path: &Path) {
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn probe_prefers_toolchain_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let rustc = bin_dir.join(&exe_name("rustc"));
        let sibling = bin_dir.join(&exe_name("rust-objcopy"));
        touch_executable(&rustc);
        touch_executable(&sibling);

        let resolved = probe_rust_objcopy(&rustc).expect("probe");
        assert_eq!(resolved, sibling, "probe must prefer toolchain sibling");
    }

    #[test]
    fn probe_returns_path_hit_when_no_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let toolchain_dir = tmp.path().join("toolchain");
        fs::create_dir_all(&toolchain_dir).unwrap();
        let rustc = toolchain_dir.join(&exe_name("rustc"));
        touch_executable(&rustc);

        let path_dir = tmp.path().join("path");
        fs::create_dir_all(&path_dir).unwrap();
        let path_hit = path_dir.join(&exe_name("rust-objcopy"));
        touch_executable(&path_hit);

        // temp_env scopes the PATH override to this closure; the global
        // mutex inside the crate serialises tests that touch PATH.
        let resolved = temp_env::with_var(
            "PATH",
            Some(path_dir.as_os_str()),
            || probe_rust_objcopy(&rustc),
        )
        .expect("probe");
        assert_eq!(resolved, path_hit);
    }

    #[test]
    fn probe_errors_with_install_hint_when_nothing_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let rustc = tmp.path().join(&exe_name("rustc"));
        touch_executable(&rustc);

        let err = temp_env::with_var(
            "PATH",
            Some(""),
            || probe_rust_objcopy(&rustc),
        )
        .expect_err("probe must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("rustup component add llvm-tools-preview"),
            "error must hint at the install command, got: {msg}"
        );
    }
}
