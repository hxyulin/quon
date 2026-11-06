//! `quon test` on the host: the libtest executables of the
//! `q.host_tests()` packages.
//!
//! Manifest expansion lowers each host-test package's lib and its
//! `harness = true` tests to [`CrateType::Test`] crates in the
//! `cargo:host-test` group (see `manifest::lower`). The build that
//! `quon test` runs compiles them with the rest of the test crates;
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
//!
//! A package's doctests run after its other tests, with the `rustdoc
//! --test` command the build stored next to its lib test executable
//! (see [`crate::compile::doctest`]).

use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use quon_model::{BuildModel, CrateType, HostTestEnv, ResolvedConfig};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// One host test run: a libtest executable, or a package's doctests.
#[derive(Debug, Clone)]
pub struct HostTest {
    /// The package the tests belong to.
    pub package: String,
    /// The crate root, relative to the package directory:
    /// `src/lib.rs` for the unit tests, `tests/<name>.rs` for the others.
    /// `doctests` for the doctests.
    pub source: String,
    /// The test executable, or rustdoc for the doctests.
    pub program: PathBuf,
    /// Arguments before libtest's: rustdoc's, for the doctests.
    pub args: Vec<OsString>,
    /// The package directory, where the tests run.
    pub dir: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub doctest: bool,
    /// Doctests whose command the build has not stored yet: under
    /// `--dry-run`, or `--no-build` before any build. `program` is then
    /// the file the build writes, and `args` is empty.
    pub unbuilt: bool,
}

impl HostTest {
    /// The command that runs the tests, with `args` passed to libtest.
    pub fn command(&self, args: &[OsString]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if self.doctest {
            // rustdoc hands libtest only what follows `--test-args`.
            for a in args {
                cmd.arg("--test-args").arg(a);
            }
        } else {
            cmd.args(args);
        }
        cmd.current_dir(&self.dir).envs(&self.env);
        cmd
    }
}

/// Create the `CARGO_TARGET_TMPDIR` that tests in `tests/` were
/// compiled with, since they may write to it right away. Call before
/// running them; `--dry-run` does not.
pub fn create_tmpdir(resolved: &ResolvedConfig) -> Result<()> {
    let tmp = resolved
        .project_root
        .join(crate::manifest::lower::HOST_TEST_TMPDIR);
    std::fs::create_dir_all(&tmp).map_err(|e| Error::Io {
        path: tmp,
        source: e,
    })
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
    let extra_env: Vec<(OsString, OsString)> = model
        .host_test_env
        .iter()
        .map(|(k, v)| {
            let v = match v {
                HostTestEnv::Value(s) => OsString::from(s),
                HostTestEnv::Path(p) => resolved.project_root.join(p).into_os_string(),
            };
            (OsString::from(k), v)
        })
        .collect();

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
        let dir = resolved.project_root.join(&def.path);
        if def.cargo.as_ref().is_some_and(|c| c.doctest_lib.is_some()) {
            let file = crate::compile::doctest::command_file(&binary);
            let (program, args, env, unbuilt) = if file.exists() {
                let doc = crate::compile::doctest::read_command(&binary)?;
                (doc.program, doc.args, doc.env.into_iter().collect(), false)
            } else {
                (file, Vec::new(), BTreeMap::new(), true)
            };
            tests.push(HostTest {
                package: package.clone(),
                source: "doctests".into(),
                program,
                args,
                dir: dir.clone(),
                env,
                doctest: true,
                unbuilt,
            });
        }
        tests.push(HostTest {
            package,
            source: def.root.clone().unwrap_or_default(),
            program: binary,
            args: Vec::new(),
            dir,
            env: env.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
            doctest: false,
            unbuilt: false,
        });
    }
    // `q.host_tests().env(..)` wins over the variables Quon sets.
    for t in &mut tests {
        t.env.extend(extra_env.iter().cloned());
    }
    // Doctests last within a package, as in `cargo test`.
    tests.sort_by(|a, b| {
        (&a.package, a.doctest, &a.source).cmp(&(&b.package, b.doctest, &b.source))
    });
    Ok(tests)
}
