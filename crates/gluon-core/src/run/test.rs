//! `gluon test`: build the integration tests and boot each one.
//!
//! An integration test is a `harness = false` target in `tests/` of a
//! root package, lowered to a crate with [`CrateDef::is_test`]. Each
//! test binary is a whole kernel image, so it is booted on its own, the
//! way [`super::prepare`] boots the profile's boot binary, and passes
//! when QEMU exits with the success status.
//!
//! In UEFI mode a test needs an ESP. Gluon copies the built ESP that
//! holds the boot binary and replaces each entry whose source is the
//! boot binary with the test binary, so `gluon.rs` needs nothing beyond
//! what `gluon run` uses. Direct boot passes the test binary as the
//! kernel. BIOS boot reads a disk image Gluon cannot rewrite per test,
//! so it is rejected.
//!
//! Tests run one at a time: they share the per-profile OVMF vars copy,
//! and QEMU's serial output goes to the terminal.

use super::entry::{RunOptions, final_binary, invocation, launch, profile_target};
use super::qemu_cmd::QemuInvocation;
use crate::build_tests_with_reporter;
use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use crate::model::BootMode;
use crate::run::resolve::resolve_qemu;
use crate::scheduler::{BuildSummary, Reporter};
use gluon_model::{BuildModel, CrateDef, EspDef, EspEntrySource, Handle, ResolvedConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one test may run when neither `--timeout`, the profile's
/// `test_timeout` nor `qemu().test_timeout(...)` says. A kernel test
/// that hangs would otherwise stop the run for good.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Knobs passed into [`prepare_tests`] by the CLI layer.
#[derive(Debug, Clone, Default)]
pub struct TestOptions {
    /// Same meaning as in [`RunOptions`].
    pub boot_mode_override: Option<BootMode>,
    /// Per-test timeout. Falls back to the profile's `test_timeout`,
    /// then `qemu().test_timeout(...)`, then 60 seconds.
    pub timeout_override: Option<Duration>,
    /// Extra QEMU arguments for every test.
    pub extra_args: Vec<std::ffi::OsString>,
    pub workers: Option<usize>,
    /// Run only the tests whose name contains this string.
    pub filter: Option<String>,
    /// Skip the build and the ESP copies, so the caller can print each
    /// test's QEMU command without booting it.
    pub dry_run: bool,
    /// Skip the build, as `gluon run --no-build` does.
    pub no_build: bool,
}

/// How one test ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestStatus {
    Passed,
    /// QEMU exited with a status other than the success status. `None`
    /// when a signal killed it.
    Failed {
        code: Option<i32>,
    },
    TimedOut,
}

/// One integration test, ready to boot.
#[derive(Debug)]
pub struct PreparedTest {
    /// The test's crate name: the test target's name, with `@test`
    /// appended when it collides with another unit.
    pub name: String,
    pub invocation: QemuInvocation,
}

/// What [`prepare_tests`] produced.
#[derive(Debug)]
pub struct PreparedTests {
    /// The tests whose name matches the filter, in name order.
    pub tests: Vec<PreparedTest>,
    /// `None` when the build was skipped (`dry_run` or `no_build`).
    pub build: Option<BuildSummary>,
    /// The QEMU exit status that means a test passed.
    pub success_exit_code: i32,
}

/// Build the profile and its integration tests, then assemble the
/// QEMU command for each test whose name matches the filter. The
/// caller boots each one with [`run_test`].
pub fn prepare_tests(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    opts: &TestOptions,
    reporter: &mut dyn Reporter,
) -> Result<PreparedTests> {
    let profile = &resolved.profile;
    let boot = profile.boot_binary.ok_or_else(|| Error::NoBootBinary {
        profile: profile.name.clone(),
    })?;
    let qemu_def = model
        .qemu
        .as_ref()
        .expect("CLI rejects an absent qemu block before reaching prepare_tests()");

    let mut tests: Vec<(Handle<CrateDef>, &CrateDef)> = resolved
        .crates
        .iter()
        .filter(|c| !c.host)
        .filter_map(|c| Some((c.handle, model.crates.get(c.handle)?)))
        .filter(|(_, k)| k.is_test())
        .filter(|(_, k)| opts.filter.as_ref().is_none_or(|f| k.name.contains(f)))
        .collect();
    tests.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    let summary = if opts.dry_run || opts.no_build {
        None
    } else {
        let workers = opts.workers.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });
        Some(build_tests_with_reporter(
            ctx, model, resolved, workers, reporter,
        )?)
    };

    // The boot mode decides how a test binary gets booted. Resolve it
    // once, the same way `invocation` will.
    let target = profile_target(model, resolved)?;
    let boot_mode = resolve_qemu(
        qemu_def,
        profile,
        &target.spec,
        opts.boot_mode_override,
        None,
    )?
    .boot_mode;
    let esp = match boot_mode {
        BootMode::Uefi => Some(boot_esp(model, boot)?),
        BootMode::Direct => None,
        BootMode::Bios => {
            return Err(Error::Config(
                "gluon test boots each test through UEFI or direct boot; BIOS boot reads a \
                 disk image gluon cannot rebuild per test"
                    .into(),
            ));
        }
    };

    let timeout = opts
        .timeout_override
        .or_else(|| profile.test_timeout.map(|s| Duration::from_secs(s.into())))
        .or_else(|| qemu_def.test_timeout.map(|s| Duration::from_secs(s.into())))
        .unwrap_or(DEFAULT_TIMEOUT);
    let run_opts = RunOptions {
        boot_mode_override: opts.boot_mode_override,
        timeout_override: Some(timeout),
        extra_args: qemu_def
            .test_extra_args
            .iter()
            .map(Into::into)
            .chain(opts.extra_args.iter().cloned())
            .collect(),
        workers: opts.workers,
        dry_run: opts.dry_run,
        no_build: opts.no_build,
        gdb: false,
        test_mode: false,
    };

    let mut prepared = Vec::with_capacity(tests.len());
    for (handle, krate) in tests {
        let binary = final_binary(ctx, model, resolved, handle)?;
        let esp_dir = match &esp {
            Some((esp_handle, esp_def)) => Some(test_esp(
                ctx,
                model,
                resolved,
                summary.as_ref(),
                (*esp_handle, esp_def),
                boot,
                &krate.name,
                &binary,
                opts.dry_run,
            )?),
            None => None,
        };
        let invocation = invocation(
            ctx,
            model,
            resolved,
            &run_opts,
            &binary,
            esp_dir,
            summary.as_ref(),
        )?;
        prepared.push(PreparedTest {
            name: krate.name.clone(),
            invocation,
        });
    }
    Ok(PreparedTests {
        tests: prepared,
        build: summary,
        success_exit_code: qemu_def.success_exit_code.unwrap_or(0),
    })
}

/// Boot one test and wait for it. A signal to gluon (Ctrl-C) is an
/// error, so the caller stops the whole run.
pub fn run_test(test: PreparedTest, success_exit_code: i32) -> Result<TestStatus> {
    match launch(test.invocation) {
        Ok(status) if status.code() == Some(success_exit_code) => Ok(TestStatus::Passed),
        Ok(status) => Ok(TestStatus::Failed {
            code: status.code(),
        }),
        Err(Error::QemuTimeout) => Ok(TestStatus::TimedOut),
        Err(e) => Err(e),
    }
}

/// The one ESP with an entry whose source is the boot binary.
fn boot_esp(model: &BuildModel, boot: Handle<CrateDef>) -> Result<(Handle<EspDef>, EspDef)> {
    let boot_name = model
        .crates
        .get(boot)
        .map(|k| k.name.clone())
        .unwrap_or_default();
    let mut found = model.esps.iter().filter(|(_, e)| {
        e.entries
            .iter()
            .any(|entry| entry.source == EspEntrySource::Crate(boot))
    });
    match (found.next(), found.next()) {
        (Some((h, e)), None) => Ok((h, e.clone())),
        (None, _) => Err(Error::Config(format!(
            "gluon test boots each test from a copy of the ESP that holds the boot binary \
             '{boot_name}', but no esp(...) block adds it"
        ))),
        (Some(_), Some(_)) => Err(Error::Config(format!(
            "more than one esp(...) block adds the boot binary '{boot_name}'; gluon test \
             cannot tell which one to boot tests from"
        ))),
    }
}

/// Assemble the ESP for one test: a copy of the built boot ESP with
/// each boot-binary entry replaced by the test binary. The copy lives
/// at `esp/<esp>.tests/<test>/`, next to the ESPs the build assembles,
/// and is rebuilt on every run, since copying a few files costs less
/// than proving the old copy current. With `paths_only`, nothing is
/// written and only the path is returned. Nothing is written either
/// when the boot ESP was never built, which only `--no-build` allows;
/// QEMU then reports the missing directory.
#[allow(clippy::too_many_arguments)]
fn test_esp(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    summary: Option<&BuildSummary>,
    (esp_handle, esp): (Handle<EspDef>, &EspDef),
    boot: Handle<CrateDef>,
    test_name: &str,
    test_binary: &Path,
    paths_only: bool,
) -> Result<PathBuf> {
    let target = profile_target(model, resolved)?;
    let built = summary
        .and_then(|s| s.esp_dirs.get(&esp_handle).cloned())
        .unwrap_or_else(|| ctx.layout.esp_dir(target, &resolved.profile, &esp.name));
    let dir = built
        .with_file_name(format!("{}.tests", esp.name))
        .join(test_name);
    if paths_only || !built.is_dir() {
        return Ok(dir);
    }

    let io = |what: &str, path: &Path, e: std::io::Error| {
        Error::Compile(format!(
            "test '{test_name}': failed to {what} {}: {e}",
            path.display()
        ))
    };
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| io("remove", &dir, e))?;
    }
    copy_dir(&built, &dir).map_err(|e| io("copy the ESP to", &dir, e))?;
    for entry in &esp.entries {
        if entry.source == EspEntrySource::Crate(boot) {
            let dest = dir.join(&entry.dest_path);
            std::fs::copy(test_binary, &dest)
                .map_err(|e| io("copy the test binary to", &dest, e))?;
        }
    }
    Ok(dir)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}
