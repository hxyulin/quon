//! Helpers for Gluon's own tests. Not a stable API.
//!
//! An end-to-end test builds a fixture in a fresh tempdir, and the
//! custom sysroot is about three quarters of that build (about 15 s of
//! 19 s for `tests/fixtures/minimal`). [`seed_sysroots`] builds each
//! sysroot once into a directory shared by every test process, then
//! copies it and its fingerprint records into the fixture's build root,
//! so the fixture's own build finds the sysroot fresh.
//!
//! Seeding does not bypass the cache. The copied records go through
//! the normal freshness check, which reads nothing tied to the seed's
//! location: the command hash covers the target spec, panic strategy
//! and `rust-src` path, the declared inputs are the rustc binary (and a
//! custom spec file), and outputs are the query's paths, not the
//! record's. A seed built by another toolchain is rebuilt under the
//! lock before it is copied.

use crate::cache::{CacheLock, sha256_bytes};
use crate::compile::{BuildLayout, CompileCtx, RustcInfo};
use crate::error::{Error, Result};
use crate::model::{BuildModel, TargetDef};
use crate::sysroot::{SYSROOT_CRATES, sysroot_command};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Copy a sysroot for every builtin target in `model` into
/// `build_root` (the fixture's `build/` directory), for a `gluon` that
/// runs `rustc` (a path, or a name looked up on `$PATH`). Returns how
/// many it copied, which the fixture's first build counts as cached.
///
/// Custom JSON targets are skipped: their declared inputs include the
/// spec file inside the fixture, so a seed's record would never match
/// and the fixture builds its own.
pub fn seed_sysroots(build_root: &Path, model: &BuildModel, rustc: &OsStr) -> Result<usize> {
    let targets: Vec<&TargetDef> = model
        .targets
        .iter()
        .map(|(_, t)| t)
        .filter(|t| t.builtin)
        .collect();
    if targets.is_empty() {
        return Ok(0);
    }
    let rustc_info = Arc::new(RustcInfo::probe_with(rustc)?);
    let dest = CompileCtx::new(
        BuildLayout::new(build_root, "seeded"),
        Arc::clone(&rustc_info),
    );
    for target in &targets {
        let seed = CompileCtx::new(
            BuildLayout::new(seed_root(rustc, &dest, target), "gluon-test-seed"),
            Arc::clone(&rustc_info),
        );
        // Tests seed in parallel threads and processes. The lock keeps
        // one of them building while the rest wait, and keeps a copy
        // from reading a half-written rlib.
        let _lock = CacheLock::acquire(&seed.layout)?;
        let mut stderr = Vec::new();
        let (_, cached) = crate::sysroot::ensure_sysroot(&seed, target, &mut stderr)?;
        if !cached {
            // rustc's incremental state is about three times the size
            // of the rlibs, and a seed is only rebuilt from scratch.
            let incremental = seed.layout.root().join("incremental");
            std::fs::remove_dir_all(&incremental).map_err(|e| io_err(&incremental, e))?;
        }
        copy_dir(
            &seed.layout.sysroot_dir(target),
            &dest.layout.sysroot_dir(target),
        )?;
        let node = format!("sysroot:{}", target.name);
        let keys = SYSROOT_CRATES
            .iter()
            .map(|c| format!("{node}:{c}"))
            .chain([node.clone()]);
        std::fs::create_dir_all(dest.store.dir()).map_err(|e| io_err(dest.store.dir(), e))?;
        for key in keys {
            let from = seed.store.record_path(&key);
            let to = dest.store.record_path(&key);
            std::fs::copy(&from, &to).map_err(|e| io_err(&from, e))?;
        }
    }
    Ok(targets.len())
}

/// Load `project`'s model and seed `<project>/build` for a `gluon`
/// that runs `rustc`, or `$RUSTC` (else `rustc`) when `None`. For tests
/// that drive the `gluon` binary and never hold a model of their own.
pub fn seed_project(project: &Path, rustc: Option<&OsStr>) -> Result<usize> {
    let model = crate::load_build_model(project)?;
    let default = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    seed_sysroots(&project.join("build"), &model, rustc.unwrap_or(&default))
}

/// The build root of one seed, in the system temp directory so every
/// test binary of every crate shares it. It is keyed by the `rustc`
/// argument and the sysroot's command hash, because both decide whether
/// a seed's record matches:
///
/// - the rustc path is a declared input, so a test that runs `gluon`
///   with `RUSTC` set needs a seed built with the same `RUSTC`;
/// - two fixtures can name the same target with different panic
///   strategies, and would otherwise rebuild one shared seed in turn.
///
/// Freshness keeps a stale seed from being copied, so sharing seeds
/// across checkouts is safe.
fn seed_root(rustc: &OsStr, ctx: &CompileCtx, target: &TargetDef) -> PathBuf {
    let rustc = sha256_bytes(rustc.as_encoded_bytes());
    let command = sysroot_command(ctx, target);
    std::env::temp_dir()
        .join("gluon-test-sysroots")
        .join(format!("{}-{}", hex8(&rustc), hex8(&command)))
}

fn hex8(bytes: &[u8; 32]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| io_err(dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| io_err(src, e))? {
        let entry = entry.map_err(|e| io_err(src, e))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| io_err(&from, e))?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| io_err(&from, e))?;
        }
    }
    Ok(())
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
