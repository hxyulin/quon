//! `gluon test` on the host: the libtest executables of the
//! `cfg.host_tests()` packages.
//!
//! Manifest expansion lowers each host-test package's lib and its
//! `harness = true` tests to [`CrateType::Test`] crates in the
//! `cargo:host-test` group (see `manifest::lower`). The build that
//! `gluon test` runs compiles them with the rest of the test crates;
//! this module finds their executables and how to start them.
//!
//! The executable paths come from the same unit lowering the build
//! uses ([`crate::unit::lower`]), so they name the files that build
//! wrote. A run right after the build cannot pick up a stale binary.
//! Under `--no-build` the files may predate the sources, as with any
//! `--no-build` run.
//!
//! Each executable runs as `cargo test` runs it: from its package's
//! directory, with the `CARGO_*` variables the crate was compiled
//! with.

use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateType, ResolvedConfig};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// One host test executable.
#[derive(Debug, Clone)]
pub struct HostTest {
    /// The package the tests belong to.
    pub package: String,
    /// The crate root, relative to the package directory:
    /// `src/lib.rs` for the unit tests, `tests/<name>.rs` for the others.
    pub source: String,
    pub binary: PathBuf,
    /// The package directory, where the executable runs.
    pub dir: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl HostTest {
    /// The command that runs the tests, with `args` passed to libtest.
    pub fn command(&self, args: &[OsString]) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args).current_dir(&self.dir).envs(&self.env);
        cmd
    }
}

/// The host test executables in `resolved`, sorted by package, then
/// source.
pub fn host_tests(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
) -> Result<Vec<HostTest>> {
    let wanted: Vec<_> = resolved
        .crates
        .iter()
        .filter(|c| {
            model
                .crates
                .get(c.handle)
                .is_some_and(|k| k.crate_type == CrateType::Test)
        })
        .map(|c| c.handle)
        .collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let outputs = crate::crate_outputs(ctx, model, resolved)?;

    let mut tests = Vec::with_capacity(wanted.len());
    for h in wanted {
        let def = model
            .crates
            .get(h)
            .ok_or_else(|| Error::Config("internal: host test handle does not resolve".into()))?;
        let binary = outputs.get(&h).cloned().ok_or_else(|| {
            Error::Config(format!(
                "internal: host test '{}' has no build unit",
                def.name
            ))
        })?;
        let (package, env) = match &def.cargo {
            Some(c) => (c.package.name.clone(), c.env.clone()),
            None => (def.name.clone(), BTreeMap::new()),
        };
        tests.push(HostTest {
            package,
            source: def.root.clone().unwrap_or_default(),
            binary,
            dir: resolved.project_root.join(&def.path),
            env,
        });
    }
    tests.sort_by(|a, b| (&a.package, &a.source).cmp(&(&b.package, &b.source)));
    Ok(tests)
}
