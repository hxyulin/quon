//! Locate and invoke `rust-objcopy` for post-link binary transforms.
//!
//! The rustup `llvm-tools` component installs `llvm-objcopy` under
//! `<sysroot>/lib/rustlib/<host>/bin`. We prefer toolchain binaries over
//! `$PATH` candidates so a user who has rustup-toolchain-pinned to
//! nightly-X gets the objcopy rustc itself was built against — avoids a class of
//! "ELF emitted by rustc-1.95 but stripped by an older host
//! `rust-objcopy`-1.86" mismatches.

use crate::error::{Error, Result};
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Find a usable `rust-objcopy` binary.
///
/// Resolution order (first hit wins):
///
/// 1. `rust-objcopy` (or `rust-objcopy.exe`) in the same directory as
///    `rustc_path`.
/// 2. `llvm-objcopy` in `rustlib_bin`, normally
///    `<sysroot>/lib/rustlib/<host>/bin`. This is where the rustup
///    `llvm-tools` component installs it; it never lands next to `rustc`
///    or on `$PATH`.
/// 3. `rust-objcopy` on `$PATH` (the cargo-binutils wrapper).
/// 4. `llvm-objcopy` on `$PATH` — the upstream LLVM binary, behaviour-
///    compatible for our `-O binary` use case.
///
/// Returns the resolved absolute path, or an error with a `rustup
/// component add llvm-tools` hint if none of the above resolve.
pub fn probe_rust_objcopy(rustc_path: &Path, rustlib_bin: &Path) -> Result<PathBuf> {
    probe_with_path(rustc_path, rustlib_bin, std::env::var_os("PATH").as_deref())
}

/// `<sysroot>/lib/rustlib/<host>/bin`, where rustup's `llvm-tools`
/// component puts its binaries.
pub fn rustlib_bin_dir(sysroot: &Path, host_triple: &str) -> PathBuf {
    sysroot
        .join("lib")
        .join("rustlib")
        .join(host_triple)
        .join("bin")
}

/// [`probe_rust_objcopy`] with `$PATH` passed in, so tests can supply
/// their own search path without mutating the process environment
/// (which races with any parallel test that spawns a subprocess).
fn probe_with_path(
    rustc_path: &Path,
    rustlib_bin: &Path,
    path_var: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(p) = sibling_of(rustc_path, "rust-objcopy") {
        return Ok(p);
    }
    let rustlib_objcopy = rustlib_bin.join(exe_name("llvm-objcopy"));
    if rustlib_objcopy.is_file() {
        return Ok(rustlib_objcopy);
    }
    if let Some(p) = which_on_path(path_var, "rust-objcopy") {
        return Ok(p);
    }
    if let Some(p) = which_on_path(path_var, "llvm-objcopy") {
        return Ok(p);
    }
    Err(Error::Config(
        "rust-objcopy not found. RawBinary crates need it to convert ELF\n\
         to flat binary output. Install with:\n    \
         rustup component add llvm-tools"
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

fn which_on_path(path_var: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    let exe = exe_name(name);
    for dir in std::env::split_paths(path_var?) {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// First four bytes of every ELF file: `0x7F` 'E' 'L' 'F'.
/// Defined in the ELF spec — see `e_ident[EI_MAG0..EI_MAG3]`.
const ELF_MAGIC: [u8; 4] = [0x7f, 0x45, 0x4c, 0x46];

/// Run `rust-objcopy -O binary <elf> <bin>`. The ELF input is expected
/// to be a real ELF file emitted by rustc + lld; we pre-check the magic
/// bytes so a user whose linker script forces `OUTPUT_FORMAT(binary)`
/// gets a specific actionable error instead of an opaque
/// `rust-objcopy: error: failed to parse object file` message.
pub fn run_objcopy_to_binary(objcopy: &Path, elf: &Path, bin: &Path) -> Result<()> {
    let mut header = [0u8; 4];
    let mut f = std::fs::File::open(elf).map_err(|e| Error::Io {
        path: elf.to_path_buf(),
        source: e,
    })?;
    match f.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(Error::Config(format!(
                "{} is shorter than 4 bytes — not a valid ELF file.",
                elf.display()
            )));
        }
        Err(e) => {
            return Err(Error::Io {
                path: elf.to_path_buf(),
                source: e,
            });
        }
    }
    if header != ELF_MAGIC {
        return Err(Error::Config(format!(
            "rust-objcopy could not parse {} as ELF.\n\
             This usually means your linker script contains \
             `OUTPUT_FORMAT(binary)`. Remove that line — quon now \
             controls output format directly so the rustc step emits \
             a normal ELF and an objcopy step produces the flat .bin.",
            elf.display()
        )));
    }

    let output = std::process::Command::new(objcopy)
        .arg("-O")
        .arg("binary")
        .arg(elf)
        .arg(bin)
        .output()
        .map_err(|e| Error::ObjcopySpawnFailed {
            binary: objcopy.to_path_buf(),
            source: e,
        })?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "rust-objcopy failed converting {} → {} (status {}):\n{}",
            elf.display(),
            bin.display(),
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
        let rustc = bin_dir.join(exe_name("rustc"));
        let sibling = bin_dir.join(exe_name("rust-objcopy"));
        touch_executable(&rustc);
        touch_executable(&sibling);

        let rustlib_bin = tmp.path().join("rustlib-bin");
        fs::create_dir_all(&rustlib_bin).unwrap();
        touch_executable(&rustlib_bin.join(exe_name("llvm-objcopy")));

        let resolved = probe_with_path(&rustc, &rustlib_bin, None).expect("probe");
        assert_eq!(resolved, sibling, "probe must prefer toolchain sibling");
    }

    #[test]
    fn probe_finds_llvm_tools_component_in_rustlib_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let rustc = bin_dir.join(exe_name("rustc"));
        touch_executable(&rustc);
        let rustlib_bin = rustlib_bin_dir(tmp.path(), "x86_64-unknown-linux-gnu");
        fs::create_dir_all(&rustlib_bin).unwrap();
        let objcopy = rustlib_bin.join(exe_name("llvm-objcopy"));
        touch_executable(&objcopy);

        // An empty PATH proves the hit comes from the sysroot, not PATH.
        let resolved = probe_with_path(&rustc, &rustlib_bin, Some(OsStr::new(""))).expect("probe");
        assert_eq!(resolved, objcopy);
    }

    #[test]
    fn probe_returns_path_hit_when_no_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let toolchain_dir = tmp.path().join("toolchain");
        fs::create_dir_all(&toolchain_dir).unwrap();
        let rustc = toolchain_dir.join(exe_name("rustc"));
        touch_executable(&rustc);

        let path_dir = tmp.path().join("path");
        fs::create_dir_all(&path_dir).unwrap();
        let path_hit = path_dir.join(exe_name("rust-objcopy"));
        touch_executable(&path_hit);

        let missing = tmp.path().join("no-rustlib-bin");
        let resolved =
            probe_with_path(&rustc, &missing, Some(path_dir.as_os_str())).expect("probe");
        assert_eq!(resolved, path_hit);
    }

    #[test]
    fn probe_errors_with_install_hint_when_nothing_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let rustc = tmp.path().join(exe_name("rustc"));
        touch_executable(&rustc);

        let missing = tmp.path().join("no-rustlib-bin");
        let err =
            probe_with_path(&rustc, &missing, Some(OsStr::new(""))).expect_err("probe must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("rustup component add llvm-tools"),
            "error must hint at the install command, got: {msg}"
        );
    }

    #[test]
    fn run_objcopy_rejects_non_elf_input_with_outputformat_hint() {
        // A file whose first bytes are NOT 7f 45 4c 46 — this is what we'd
        // see if the user's linker script contains OUTPUT_FORMAT(binary).
        let tmp = tempfile::tempdir().unwrap();
        let fake_elf = tmp.path().join("not-actually-elf.elf");
        fs::write(&fake_elf, b"this is just raw bytes, no ELF header").unwrap();

        let bin_out = tmp.path().join("out.bin");
        // The magic-byte pre-check in run_objcopy_to_binary fires BEFORE the
        // rust-objcopy spawn, so we don't need a real binary here.
        let fake_objcopy = PathBuf::from("/nonexistent/objcopy");
        let err = run_objcopy_to_binary(&fake_objcopy, &fake_elf, &bin_out)
            .expect_err("non-ELF input must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("OUTPUT_FORMAT(binary)"),
            "error must mention the linker-script gotcha, got: {msg}"
        );
    }

    // On macOS and Windows the host `rustc` produces Mach-O / PE, not ELF.
    // The magic-byte check would reject those — that's correct behaviour,
    // not a bug. Only Linux hosts produce native ELF, so the fixture below
    // only makes sense there. Cross-compiling for a Linux target is an
    // alternative but requires the cross-target to be installed; gating
    // by OS is simpler and sufficient for the CI matrix.
    #[cfg(target_os = "linux")]
    #[test]
    fn run_objcopy_emits_flat_bin_from_real_elf() {
        // Build a tiny throwaway C-free ELF the host toolchain can produce.
        // Use rustc itself to compile a `fn main() {}` for the host target,
        // then convert.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("p.rs");
        fs::write(&src, b"fn main() {}").unwrap();
        let elf = tmp.path().join(exe_name("p"));
        let status = std::process::Command::new("rustc")
            .args(["--edition", "2024", "-o"])
            .arg(&elf)
            .arg(&src)
            .status()
            .expect("spawn rustc");
        assert!(status.success(), "fixture rustc failed");

        let bin = tmp.path().join("p.bin");
        let rustc = which::which("rustc").expect("rustc on PATH");
        let info = crate::compile::rustc_info::RustcInfo::probe().expect("probe rustc");
        let rustlib_bin = rustlib_bin_dir(&info.sysroot, &info.host_triple);
        let objcopy = probe_rust_objcopy(&rustc, &rustlib_bin).expect("toolchain probe");
        run_objcopy_to_binary(&objcopy, &elf, &bin).expect("objcopy");
        assert!(bin.is_file(), ".bin must exist after objcopy");
        // The flat output is non-zero — strip-nothing -O binary preserves
        // every loadable section.
        assert!(fs::metadata(&bin).unwrap().len() > 0);
    }
}
