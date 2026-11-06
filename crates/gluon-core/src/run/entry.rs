//! `gluon run` top-level entry.
//!
//! Wires together the runner's validation, `build()`, locating the run
//! binary, the runner's command, and the spawn.
//!
//! Public surface: [`prepare`] and [`launch`] plus the [`RunOptions`]
//! struct the CLI populates from its clap-parsed args. The command
//! itself comes from the runner (see [`super::runner`]); this file is
//! the one spot where subprocesses get spawned.

use super::runner::{Invocation, Runner, Runners, command_invocation, profile_runner};
use crate::build_with_reporter;
use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use crate::model::BootMode;
use crate::scheduler::{BuildSummary, Reporter};
use gluon_model::{BuildModel, CrateDef, Handle, ResolvedConfig, TargetDef, TargetSpec};
use gluon_runner::{Artifact, ArtifactKind, Context, PrepareRequest, TestBinary, ValidateResponse};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::time::{Duration, Instant};

/// How long one test may run when neither `--timeout`, the profile's
/// `test_timeout` nor the runner (QEMU's `test_timeout(...)`) says. A
/// kernel test that hangs would otherwise stop the run for good.
const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Knobs passed into [`prepare`] by the CLI layer.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Forces a boot mode over whatever the profile or runner declared,
    /// passed to a plugin runner as the `boot_mode` override. `None`
    /// means "use the profile-level setting, or the runner's default".
    pub boot_mode_override: Option<BootMode>,
    /// Forces a wall-clock timeout, overriding any profile-level
    /// `test_timeout`. `None` means "inherit profile, or no timeout".
    pub timeout_override: Option<Duration>,
    /// CLI pass-through arguments (`gluon run -- -device ...`).
    pub extra_args: Vec<OsString>,
    /// Worker count for the preceding `build()` step. `None` = auto.
    pub workers: Option<usize>,
    /// Skip the build and the ESP existence checks, so the caller can
    /// print the assembled command without spawning it. Used by
    /// integration tests and for users sanity-checking a config.
    pub dry_run: bool,
    /// Skip the implicit `build()` step and go straight to the spawn.
    /// The user is asserting the run binary on disk is already
    /// current; if it isn't, the runner will report a clearer "could
    /// not load kernel" error than gluon could fabricate from the
    /// outside. Intended for tight edit/run loops where rerunning
    /// gluon's fingerprint sweep is pure latency.
    ///
    /// Implied by `dry_run` (dry-run already skips the build).
    pub no_build: bool,
    /// Ask the runner to start the binary halted for a debugger. QEMU
    /// adds `-s -S` and returns a note saying where to connect.
    pub gdb: bool,
    /// Set by `gluon test`: the per-test timeout applies, and a plugin
    /// runner is told it runs a test (QEMU then adds its
    /// `test_extra_args(...)`).
    pub test: bool,
    /// The runner plugins built into the caller.
    pub runners: Runners,
}

/// What [`prepare`] produced: the command to run, and the summary of
/// the build that preceded it.
#[derive(Debug)]
pub struct Prepared {
    pub invocation: Invocation,
    /// `None` when the build was skipped (`dry_run` or `no_build`).
    pub build: Option<BuildSummary>,
}

/// Build the project and assemble the command that runs it.
///
/// Steps, in order:
///
/// 1. **Check the runner.** [`RunnerSession::start`] finds the
///    profile's runner and, for a plugin runner, validates its
///    settings, so a bad setting fails before the build.
/// 2. **Build.** Invokes [`build_with_reporter`] so `gluon run` is
///    idempotent with the current project state. A failed build
///    short-circuits without starting anything.
/// 3. **Locate run binary.** The resolved profile must have a
///    `run_binary` set; otherwise [`Error::NoRunBinary`] is
///    returned.
/// 4. **Assemble the command.** [`RunnerSession::invocation`].
///
/// The caller then prints what it wants (the command for a dry run,
/// the runner's notes) and hands the invocation to [`launch`].
/// Splitting here lets the CLI put its own lines between the build and
/// the program's output.
pub fn prepare(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    opts: &RunOptions,
    reporter: &mut dyn Reporter,
) -> Result<Prepared> {
    let session = RunnerSession::start(ctx, model, resolved, opts)?;

    // Step 2: build (skipped in dry-run or when --no-build is set).
    //
    // Dry-run exists to let users sanity-check their runner
    // configuration without touching rustc, the cache, or the
    // filesystem. The assembled argv is entirely derivable from the
    // resolved model and the layout's path arithmetic, so we can skip
    // the build wholesale and still produce the correct output.
    // Integration tests rely on this.
    //
    // `--no-build` is the same skip without the spawn skip: the user
    // is telling us "I just rebuilt by hand, don't waste my time
    // re-checking timestamps". If the run binary actually is stale
    // or missing, the runner will fail at load time with a clearer
    // diagnostic than gluon would produce by trying to second-guess.
    let build_summary = if !opts.dry_run && !opts.no_build {
        let workers = opts.workers.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });
        Some(build_with_reporter(
            ctx, model, resolved, workers, reporter,
        )?)
    } else {
        None
    };

    // Step 3: locate run binary.
    let profile = &resolved.profile;
    let boot_handle = profile.run_binary.ok_or_else(|| Error::NoRunBinary {
        profile: profile.name.clone(),
    })?;
    let kernel = final_binary(ctx, model, resolved, boot_handle)?;
    let invocation = session.invocation(
        ctx,
        model,
        resolved,
        opts,
        &kernel,
        None,
        build_summary.as_ref(),
    )?;

    Ok(Prepared {
        invocation,
        build: build_summary,
    })
}

/// Where the build puts `krate`'s binary for the profile's target.
pub(super) fn final_binary(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    handle: Handle<CrateDef>,
) -> Result<PathBuf> {
    let profile = &resolved.profile;
    let krate = model.crates.get(handle).ok_or_else(|| {
        Error::Config(format!(
            "internal: crate handle for profile '{}' does not resolve in the model",
            profile.name
        ))
    })?;
    if resolved.profile.target == TargetSpec::Host {
        // Host executables carry their metadata hash in the file name,
        // so the path comes from the unit lowering the build uses.
        let outputs = crate::crate_outputs(ctx, model, resolved)?;
        return outputs.get(&handle).cloned().ok_or_else(|| {
            Error::Config(format!(
                "profile '{}': crate '{}' is not built by this profile",
                profile.name, krate.name
            ))
        });
    }
    let target = profile_target(model, resolved)?;
    let suffix = crate::compile::compile_utils::exe_suffix_for_target(&target.spec);
    Ok(ctx.layout.cross_final_dir(target, profile).join(format!(
        "{}{suffix}",
        crate::compile::compile_utils::normalize_crate_name(&krate.name)
    )))
}

pub(super) fn profile_target<'m>(
    model: &'m BuildModel,
    resolved: &ResolvedConfig,
) -> Result<&'m TargetDef> {
    let profile = &resolved.profile;
    let t = profile.cross_target().ok_or_else(|| {
        Error::Config(format!(
            "profile '{}' builds for the host, and this needs a target; \
             use a profile with `.target(..)`",
            profile.name
        ))
    })?;
    model.targets.get(t).ok_or_else(|| {
        Error::Config(format!(
            "internal: profile '{}' target handle does not resolve",
            profile.name
        ))
    })
}

/// The profile's runner, checked against the options before the build.
pub(super) struct RunnerSession<'m> {
    runner: Runner<'m>,
    /// For a plugin runner: the plugin, the context every call to it
    /// carries, and its reply to `validate`.
    plugin: Option<(Arc<dyn gluon_runner::Runner>, Context, ValidateResponse)>,
}

impl<'m> RunnerSession<'m> {
    /// Find the profile's runner. A command or host runner refuses the
    /// options only a plugin takes; a plugin runner is found and
    /// validated.
    pub(super) fn start(
        ctx: &CompileCtx,
        model: &'m BuildModel,
        resolved: &ResolvedConfig,
        opts: &RunOptions,
    ) -> Result<Self> {
        let runner = profile_runner(model, resolved)?;
        runner.require_plugin(opts.gdb, "--gdb")?;
        runner.require_plugin(opts.boot_mode_override.is_some(), "a boot mode flag")?;
        let plugin = match &runner {
            Runner::Plugin {
                name,
                plugin,
                settings,
                overrides,
            } => {
                let mut overrides = overrides.clone();
                if let Some(mode) = opts.boot_mode_override {
                    // The command line beats the profile.
                    overrides.insert(
                        "boot_mode".into(),
                        serde_json::to_value(mode).expect("a boot mode serializes"),
                    );
                }
                let target = resolved
                    .profile
                    .cross_target()
                    .and_then(|t| model.targets.get(t))
                    .map(|t| t.spec.clone());
                let context = Context {
                    runner: (*name).to_string(),
                    profile: resolved.profile.name.clone(),
                    target,
                    build_dir: ctx.layout.root().to_path_buf(),
                    settings: settings.clone(),
                    overrides,
                    test: opts.test,
                };
                let found = opts.runners.find(name, plugin)?;
                let validated = found.validate(&context).map_err(Error::from)?;
                Some((found, context, validated))
            }
            Runner::Command { .. } | Runner::Host => None,
        };
        Ok(Self { runner, plugin })
    }

    /// How `gluon test` boots an integration test's binary.
    pub(super) fn test_binary(&self) -> TestBinary {
        match &self.plugin {
            Some((_, _, v)) => v.test_binary.clone(),
            None => TestBinary::AsBinary,
        }
    }

    /// The wall-clock limit for one run.
    ///
    /// Under `gluon test`: `--timeout`, then the profile's
    /// `test_timeout`, then the runner's default, then
    /// [`DEFAULT_TEST_TIMEOUT`]. Under `gluon run`: `--timeout`, and
    /// for a plugin runner the profile's `test_timeout`, as the QEMU
    /// runner always applied it.
    pub(super) fn timeout(&self, opts: &RunOptions, resolved: &ResolvedConfig) -> Option<Duration> {
        let profile = resolved
            .profile
            .test_timeout
            .map(|s| Duration::from_secs(s.into()));
        if opts.test {
            let runner = self
                .plugin
                .as_ref()
                .and_then(|(_, _, v)| v.test_timeout)
                .map(Duration::from_secs);
            return Some(
                opts.timeout_override
                    .or(profile)
                    .or(runner)
                    .unwrap_or(DEFAULT_TEST_TIMEOUT),
            );
        }
        match self.runner {
            Runner::Plugin { .. } => opts.timeout_override.or(profile),
            Runner::Command { .. } | Runner::Host => opts.timeout_override,
        }
    }

    /// The command that runs `kernel` with the profile's runner.
    /// `boot_esp`, when set, is the ESP to boot instead of the one the
    /// project declares; `gluon test` passes each test's copy.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn invocation(
        &self,
        ctx: &CompileCtx,
        model: &BuildModel,
        resolved: &ResolvedConfig,
        opts: &RunOptions,
        kernel: &Path,
        boot_esp: Option<PathBuf>,
        build_summary: Option<&BuildSummary>,
    ) -> Result<Invocation> {
        let timeout = self.timeout(opts, resolved);
        let (runner, context, _) = match (&self.runner, &self.plugin) {
            (Runner::Plugin { .. }, Some(plugin)) => plugin,
            (Runner::Command { name, def }, _) => {
                return command_invocation(name, def, kernel, &opts.extra_args, timeout);
            }
            (Runner::Host, _) => {
                return Ok(Invocation {
                    binary: kernel.to_string_lossy().into_owned(),
                    args: opts.extra_args.clone(),
                    env: BTreeMap::new(),
                    timeout,
                    runner: "host".into(),
                    success_exit_code: None,
                    notes: Vec::new(),
                });
            }
            (Runner::Plugin { .. }, None) => unreachable!("start() finds every plugin"),
        };
        let request = PrepareRequest {
            context: context.clone(),
            binary: kernel.to_path_buf(),
            artifacts: artifacts(ctx, model, resolved, build_summary),
            boot_esp,
            // The protocol carries text; an argument that is not UTF-8
            // cannot reach a plugin intact.
            extra_args: opts
                .extra_args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            gdb: opts.gdb,
            built: !(opts.dry_run || opts.no_build),
        };
        let reply = runner.prepare(&request).map_err(Error::from)?;
        Ok(Invocation {
            binary: reply.program,
            args: reply.args.into_iter().map(OsString::from).collect(),
            env: reply.env,
            timeout,
            runner: context.runner.clone(),
            success_exit_code: reply.success_exit_code,
            notes: reply.notes,
        })
    }
}

/// Every ESP, image and build step the project declares, with the path
/// the build put it at. Without a build (dry-run / `--no-build`) the
/// paths come from layout arithmetic, so a reference still resolves to
/// a deterministic path; the file just may not exist. After a build,
/// an image or build step the build did not produce has no path, and
/// an ESP falls back to where the layout puts it.
fn artifacts(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    summary: Option<&BuildSummary>,
) -> Vec<Artifact> {
    let mut out = Vec::new();
    // ESPs live under the target's directory, so a host profile has
    // none to offer.
    if let Some(target) = resolved
        .profile
        .cross_target()
        .and_then(|t| model.targets.get(t))
    {
        for (h, esp) in model.esps.iter() {
            let path = summary
                .and_then(|s| s.esp_dirs.get(&h).cloned())
                .unwrap_or_else(|| ctx.layout.esp_dir(target, &resolved.profile, &esp.name));
            out.push(Artifact {
                name: esp.name.clone(),
                kind: ArtifactKind::Esp,
                path: Some(path),
            });
        }
    }
    for (h, img) in model.images.iter() {
        let path = match summary {
            Some(s) => s.image_paths.get(&h).cloned(),
            None => Some(ctx.layout.image_path(&img.name)),
        };
        out.push(Artifact {
            name: img.name.clone(),
            kind: ArtifactKind::Image,
            path,
        });
    }
    for (h, bs) in model.build_steps.iter() {
        let path = match summary {
            Some(s) => s.build_step_paths.get(&h).cloned(),
            None => Some(ctx.layout.build_step_dir(&bs.name).join(&bs.output)),
        };
        out.push(Artifact {
            name: bs.name.clone(),
            kind: ArtifactKind::BuildStep,
            path,
        });
    }
    out
}

impl Invocation {
    /// The command in a copy-pasteable form, for `--dry-run` and the
    /// CLI's `Running` line.
    pub fn command_line(&self) -> String {
        let mut words: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", shell_quote(v)))
            .collect();
        words.push(shell_quote(&self.binary));
        words.extend(self.args.iter().map(|a| shell_quote(&a.to_string_lossy())));
        words.join(" ")
    }
}

/// Start `invocation`'s program and wait for it; see
/// [`spawn_and_wait`] for the timeout and signal handling.
pub fn launch(invocation: Invocation) -> Result<ExitStatus> {
    spawn_and_wait(invocation)
}

/// Minimal POSIX-shell quoting suitable for human consumption. We
/// wrap anything with whitespace or shell metacharacters in single
/// quotes; everything else is printed verbatim.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | ','))
    {
        return s.to_string();
    }
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Spawn the program (QEMU, or a command runner's) and wait for it,
/// enforcing `invocation.timeout`
/// and a SIGINT/SIGTERM trap that forwards the signal to the child.
///
/// Stdio is inherited so the user sees serial output live. If the
/// caller set `SerialMode::File`, QEMU is the one writing the file;
/// we still inherit our stdio so diagnostic output ("QEMU:
/// warning:...") reaches the terminal.
///
/// On Unix, we register SIGINT + SIGTERM handlers via `signal-hook`
/// that flip an `AtomicBool`. The try_wait loop polls that flag and,
/// when set, kills the child cleanly, reaps it, and returns
/// [`Error::KilledBySignal`]. This prevents the common failure mode
/// where the user hits Ctrl-C and the shell's SIGINT gets delivered
/// to both gluon and QEMU — gluon returning while QEMU keeps running
/// as an orphan with a dangling stdio inheritance. We'd rather eat
/// the signal, tear the child down, and tell the user what happened.
///
/// On Windows the signal handler is a no-op; the Windows console
/// uses `SetConsoleCtrlHandler` which is a different mechanism and
/// the runner is Unix-centric today. Wiring it up can come later if
/// someone needs it.
fn spawn_and_wait(invocation: Invocation) -> Result<ExitStatus> {
    let mut cmd = Command::new(&invocation.binary);
    cmd.args(&invocation.args);
    cmd.envs(&invocation.env);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::RunnerProgramNotFound {
                runner: invocation.runner.clone(),
                program: invocation.binary.clone(),
            }
        } else {
            Error::QemuSpawnFailed {
                binary: invocation.binary.clone(),
                source: e,
            }
        }
    })?;

    // Install signal trap only after we have a live child — before
    // this point there's nothing to tear down if a signal arrives,
    // and the default signal disposition (exit) is fine.
    let signal_flag = install_signal_trap();

    let deadline = invocation.timeout.map(|d| Instant::now() + d);
    loop {
        match child.try_wait().map_err(|e| Error::QemuSpawnFailed {
            binary: invocation.binary.clone(),
            source: e,
        })? {
            Some(status) => return Ok(status),
            None => {
                // Signal check first: a user hitting Ctrl-C expects
                // immediate teardown, even if we're a few polls away
                // from the timeout.
                if let Some(sig) = check_signal(&signal_flag) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::KilledBySignal { signal: sig });
                }
                if let Some(dl) = deadline
                    && Instant::now() >= dl
                {
                    // Kill + reap. Ignore errors: the child may
                    // have exited on its own between the try_wait
                    // and the kill, which is harmless.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::RunTimeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Install a SIGINT + SIGTERM handler that stores the received signal
/// number in a shared `AtomicI32`. Returns the shared flag — zero means
/// "no signal yet", any other value is the received signal number.
///
/// On non-Unix this is a no-op: the returned flag will always read 0,
/// which means the runner falls back to its previous behaviour
/// (timeout-only teardown).
#[cfg(unix)]
fn install_signal_trap() -> Arc<AtomicI32> {
    let flag = Arc::new(AtomicI32::new(0));
    // signal-hook's `low_level::register` installs a handler that
    // runs async-signal-safe code (atomic store) — no allocation, no
    // Rust panic unwinding through the signal frame. Ignore
    // registration errors: if we can't install the handler (for
    // example because one's already installed by the host program)
    // we fall back to the default signal disposition, which is
    // "exit the process" — no worse than the pre-signal-hook
    // behaviour.
    //
    // SAFETY: signal-hook's `low_level::register` is `unsafe` because
    // the closure runs in a signal context; we use only atomic stores
    // (async-signal-safe) inside it and capture by `move`, satisfying
    // the API's contract.
    for &sig in &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let flag_clone = flag.clone();
        #[allow(unsafe_code)]
        let _ = unsafe {
            signal_hook::low_level::register(sig, move || {
                flag_clone.store(sig, std::sync::atomic::Ordering::SeqCst);
            })
        };
    }
    flag
}

#[cfg(not(unix))]
fn install_signal_trap() -> Arc<AtomicI32> {
    Arc::new(AtomicI32::new(0))
}

fn check_signal(flag: &Arc<AtomicI32>) -> Option<i32> {
    let v = flag.load(std::sync::atomic::Ordering::SeqCst);
    if v == 0 { None } else { Some(v) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_each_argument() {
        let inv = Invocation {
            binary: "qemu-system-x86_64".into(),
            args: vec!["-m".into(), "128M".into(), "-append".into(), "a b".into()],
            env: BTreeMap::new(),
            timeout: None,
            runner: "qemu".into(),
            success_exit_code: None,
            notes: Vec::new(),
        };
        assert_eq!(
            inv.command_line(),
            "qemu-system-x86_64 -m 128M -append 'a b'"
        );
        let mut inv = inv;
        inv.env.insert("DEFMT_LOG".into(), "info trace".into());
        assert_eq!(
            inv.command_line(),
            "DEFMT_LOG='info trace' qemu-system-x86_64 -m 128M -append 'a b'"
        );
    }

    #[test]
    fn shell_quote_cases() {
        let cases = [
            ("q35", "q35"),
            ("/path/to/file.fd", "/path/to/file.fd"),
            ("format=raw,file=foo", "format=raw,file=foo"),
            ("hello world", "'hello world'"),
            ("it's", "'it'\\''s'"),
            ("", "''"),
        ];
        for (input, expected) in cases {
            assert_eq!(shell_quote(input), expected, "input {input:?}");
        }
    }
}
