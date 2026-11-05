//! `gluon run` top-level entry.
//!
//! Wires together `build()`, resolve, OVMF resolution (when needed),
//! argv assembly, and QEMU spawn.
//!
//! Public surface: [`prepare`] and [`launch`] plus the [`RunOptions`]
//! struct the CLI populates from its clap-parsed args. Keeping everything else in
//! sibling submodules means the whole pipeline stays unit-testable in
//! isolation: `qemu_cmd` is pure, `resolve` is pure, `ovmf` only
//! touches the filesystem behind an injected context, and this file
//! is the one spot where subprocesses get spawned.

use super::ovmf::{OvmfResolveCtx, resolve_ovmf};
use super::qemu_cmd::{QemuInvocation, build_qemu_command};
use super::resolve::{resolve_disk_source, resolve_qemu};
use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use crate::scheduler::{BuildSummary, Reporter};
use crate::{build_with_reporter, model::BootMode};
use gluon_model::{BuildModel, CrateDef, EspSource, Handle, ResolvedConfig, TargetDef};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::time::{Duration, Instant};

/// Knobs passed into [`prepare`] by the CLI layer.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Forces `BootMode::Uefi` / `BootMode::Direct`, overriding whatever
    /// the profile's `qemu().boot_mode(...)` declared. `None` means
    /// "use the profile-level setting, or direct".
    pub boot_mode_override: Option<BootMode>,
    /// Forces a wall-clock timeout, overriding any profile-level
    /// `test_timeout`. `None` means "inherit profile, or no timeout".
    pub timeout_override: Option<Duration>,
    /// CLI pass-through arguments (`gluon run -- -device ...`).
    pub extra_args: Vec<OsString>,
    /// Worker count for the preceding `build()` step. `None` = auto.
    pub workers: Option<usize>,
    /// Skip the build and the ESP existence checks, so the caller can
    /// print the assembled QEMU command without spawning it. Used by
    /// integration tests and for users sanity-checking a config.
    pub dry_run: bool,
    /// Skip the implicit `build()` step and go straight to the QEMU
    /// spawn. The user is asserting the boot binary on disk is
    /// already current; if it isn't, QEMU will report a clearer
    /// "could not load kernel" error than gluon could fabricate from
    /// the outside. Intended for tight edit/run loops where rerunning
    /// gluon's fingerprint sweep is pure latency.
    ///
    /// Implied by `dry_run` (dry-run already skips the build).
    pub no_build: bool,
    /// Start QEMU with a GDB server on :1234 and halt the CPU before
    /// the first instruction (`-s -S`). The CLI prints a hint before
    /// the spawn so the user knows where to point their debugger.
    pub gdb: bool,
    /// Emit an `-device isa-debug-exit,iobase=<port>,iosize=0x04`
    /// entry in the QEMU argv so a guest can report an exit code
    /// through an I/O port write. The port comes from
    /// [`super::ResolvedQemu::test_exit_port`].
    ///
    /// Not wired to a CLI flag today. `gluon test` leaves it off:
    /// projects such as Hadron add the device in
    /// `qemu().extra_args(...)`, and a second device on the same port
    /// would clash.
    pub test_mode: bool,
}

/// What [`prepare`] produced: the QEMU command, and the summary of the
/// build that preceded it.
#[derive(Debug)]
pub struct Prepared {
    pub invocation: QemuInvocation,
    /// `None` when the build was skipped (`dry_run` or `no_build`).
    pub build: Option<BuildSummary>,
}

/// Build the project and assemble the QEMU command that runs it.
///
/// Steps, in order:
///
/// 1. **Build.** Invokes [`build_with_reporter`] so `gluon run` is
///    idempotent with the current project state. A failed build
///    short-circuits without touching QEMU.
/// 2. **Locate boot binary.** The resolved profile must have a
///    `boot_binary` set; otherwise [`Error::NoBootBinary`] is
///    returned. The binary path is
///    `BuildLayout::cross_final_dir(target, profile).join(crate_name)`.
/// 3. **Resolve QEMU.** [`resolve_qemu`] merges profile overrides and
///    fills defaults.
/// 4. **UEFI: resolve OVMF.** Only when `boot_mode == Uefi`. The
///    writable-vars copy (when needed) lands under
///    `build/ovmf_vars-<profile>.fd`.
/// 5. **Build argv.** Pure [`build_qemu_command`] call.
///
/// The caller then prints what it wants (the command for a dry run, a
/// GDB hint) and hands the invocation to [`launch`]. Splitting here
/// lets the CLI put its own lines between the build and QEMU's output.
pub fn prepare(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    opts: &RunOptions,
    reporter: &mut dyn Reporter,
) -> Result<Prepared> {
    // Step 1: build (skipped in dry-run or when --no-build is set).
    //
    // Dry-run exists to let users sanity-check their QEMU
    // configuration without touching rustc, the cache, or the
    // filesystem. The assembled argv is entirely derivable from the
    // resolved model and the `BuildLayout::cross_final_dir` helper,
    // so we can skip the build wholesale and still produce the
    // correct output. Integration tests rely on this.
    //
    // `--no-build` is the same skip without the spawn skip: the user
    // is telling us "I just rebuilt by hand, don't waste my time
    // re-checking timestamps". If the boot binary actually is stale
    // or missing, QEMU will fail at load time with a clearer
    // diagnostic than gluon would produce by trying to second-guess.
    // Captured if we actually built — carries the ESP output paths
    // used by Step 4 to auto-wire QEMU's `-drive fat:rw:` flag.
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

    // Step 2: locate boot binary.
    let profile = &resolved.profile;
    let boot_handle = profile.boot_binary.ok_or_else(|| Error::NoBootBinary {
        profile: profile.name.clone(),
    })?;
    let kernel = final_binary(ctx, model, resolved, boot_handle)?;
    let invocation = invocation(
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
    krate: Handle<CrateDef>,
) -> Result<PathBuf> {
    let profile = &resolved.profile;
    let krate = model.crates.get(krate).ok_or_else(|| {
        Error::Config(format!(
            "internal: crate handle for profile '{}' does not resolve in the model",
            profile.name
        ))
    })?;
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
    model.targets.get(resolved.profile.target).ok_or_else(|| {
        Error::Config(format!(
            "internal: profile '{}' target handle does not resolve",
            resolved.profile.name
        ))
    })
}

/// Steps 3 to 5 of [`prepare`]: resolve QEMU, the disk, the ESP and
/// OVMF, and assemble the command that boots `kernel`. `esp_override`,
/// when set, is the ESP directory to mount in UEFI mode instead of the
/// one the project declares; `gluon test` passes each test's copy.
pub(super) fn invocation(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    opts: &RunOptions,
    kernel: &Path,
    esp_override: Option<PathBuf>,
    build_summary: Option<&BuildSummary>,
) -> Result<QemuInvocation> {
    let profile = &resolved.profile;
    let target = profile_target(model, resolved)?;

    // Step 3: resolve QEMU config.
    //
    // `model.qemu` is `Option<QemuDef>` since Phase G of Stage 4: the CLI
    // is responsible for rejecting an absent qemu block at the entry
    // point with `Error::QemuNotConfigured`. By the time we reach `prepare`
    // the field must be `Some` — `expect` documents that contract for
    // future readers and produces a panic, not a silent default, if the
    // CLI ever forgets the check. Refactoring the function signature to
    // accept `&QemuDef` directly is the cleaner long-term shape, but
    // that's a wider blast radius than this phase needs.
    //
    // We pass the target's spec as the triple because for builtin
    // targets it is the triple (`x86_64-unknown-none`), and for custom
    // JSON specs it's the spec path — which users typically name with
    // the arch prefix, so the arch-prefix match in
    // `default_binary_for_target` still has a reasonable shot.
    let qemu_def = model
        .qemu
        .as_ref()
        .expect("CLI rejects an absent qemu block before reaching run()");
    let mut resolved_qemu = resolve_qemu(
        qemu_def,
        profile,
        &target.spec,
        opts.boot_mode_override,
        opts.timeout_override,
    )?;

    // Step 3a: resolve `qemu().disk_*(...)` to a concrete path.
    //
    // `disk_image("path")` is a host file the user manages directly.
    // `disk_ref("name")` references an `image("name")` declared in the
    // same script — we resolve through the build summary's
    // `image_paths` map, falling back to `BuildLayout::image_path` in
    // dry-run / `--no-build` modes (where the build didn't run, so the
    // map is empty).
    if let Some(disk_source) = qemu_def.disk.clone() {
        let image_paths = match build_summary {
            Some(s) => s.image_paths.clone(),
            None => {
                // Dry-run / no-build fallback: synthesise paths via
                // layout arithmetic. Keeps `gluon run --dry-run` from
                // failing at config-resolve time when the image hasn't
                // been built yet.
                let mut map: BTreeMap<_, _> = BTreeMap::new();
                for (h, img) in model.images.iter() {
                    map.insert(h, ctx.layout.image_path(&img.name));
                }
                map
            }
        };
        // Build-step output paths follow the same pattern: take the
        // post-build map, or synthesise from the layout in dry-run /
        // --no-build mode so `disk_ref` to a build step's name still
        // resolves to a deterministic path (the file just won't exist).
        let build_step_paths = match build_summary {
            Some(s) => s.build_step_paths.clone(),
            None => {
                let mut map: BTreeMap<_, _> = BTreeMap::new();
                for (h, bs) in model.build_steps.iter() {
                    map.insert(h, ctx.layout.build_step_dir(&bs.name).join(&bs.output));
                }
                map
            }
        };
        let disk_path = resolve_disk_source(&disk_source, model, &image_paths, &build_step_paths)?;
        resolved_qemu.disk = Some(disk_path);
    }

    // Step 3b: auto-wire the ESP directory for UEFI boot.
    //
    // If the final boot mode is UEFI, the user has NOT set an explicit
    // `qemu().esp_dir()` / `esp_image()`, and the project declares
    // exactly one `esp(...)` block, then point QEMU at the build-output
    // ESP directory we just assembled (or, in dry-run / --no-build
    // mode, at the path we *would* have assembled via pure path
    // arithmetic). More than one declared ESP is an error: the user
    // must pick explicitly because we can't guess. Zero declarations
    // falls through — QEMU will simply have no ESP drive, same as
    // before this feature existed.
    if let Some(dir) = esp_override.filter(|_| resolved_qemu.boot_mode == BootMode::Uefi) {
        resolved_qemu.esp = Some(EspSource::Dir(dir));
    } else if resolved_qemu.boot_mode == BootMode::Uefi && resolved_qemu.esp.is_none() {
        match model.esps.len() {
            0 => { /* no automation requested */ }
            1 => {
                // Prefer the path the build actually produced, so a
                // test-only run (without build) never disagrees with a
                // real build. Fall back to path arithmetic when we
                // didn't build (dry-run / --no-build).
                let esp_handle = model
                    .esps
                    .iter()
                    .next()
                    .map(|(h, _)| h)
                    .expect("model.esps.len() == 1");
                let esp_def = model.esps.get(esp_handle).ok_or_else(|| {
                    Error::Compile("internal: esp handle iterates but does not resolve".to_string())
                })?;
                let esp_path = build_summary
                    .and_then(|s| s.esp_dirs.get(&esp_handle).cloned())
                    .unwrap_or_else(|| ctx.layout.esp_dir(target, profile, &esp_def.name));
                resolved_qemu.esp = Some(EspSource::Dir(esp_path));
            }
            n => {
                return Err(Error::Compile(format!(
                    "profile '{}' uses boot_mode('uefi') and the project declares {} esp(...) blocks; \
                     gluon can't guess which one to mount. Set qemu().esp_dir(...) explicitly, \
                     or remove the extra esp declarations.",
                    profile.name, n
                )));
            }
        }
    }

    // Step 4: OVMF (only when the final boot mode is UEFI).
    let ovmf = if resolved_qemu.boot_mode == BootMode::Uefi {
        let ovmf_ctx =
            OvmfResolveCtx::from_env(ctx.layout.root(), &profile.name, &resolved_qemu.binary);
        Some(resolve_ovmf(qemu_def, &ovmf_ctx)?)
    } else {
        None
    };

    // Step 5: assemble argv.
    //
    // `--gdb` is surfaced here by prepending `-s -S` to the CLI
    // pass-through list. This keeps `build_qemu_command` pure — it
    // doesn't need to know anything about gdb, just about argv
    // ordering — and it places the gdb flags *after* the user's
    // Rhai-level `extra_args`, so a user who wants to customize
    // the gdb port (`-gdb tcp::9999`) can do it via their config
    // without being clobbered.
    let extra_args_with_gdb: Vec<OsString> = if opts.gdb {
        let mut v = Vec::with_capacity(opts.extra_args.len() + 2);
        v.push(OsString::from("-s"));
        v.push(OsString::from("-S"));
        v.extend(opts.extra_args.iter().cloned());
        v
    } else {
        opts.extra_args.clone()
    };
    build_qemu_command(
        &resolved_qemu,
        kernel,
        resolved_qemu.boot_mode,
        ovmf.as_ref(),
        &extra_args_with_gdb,
        opts.test_mode,
        // Skip ESP existence checks in dry-run / --no-build. In dry-run
        // we haven't built anything, so the auto-wired ESP path does
        // not exist on disk; in --no-build the user is asserting the
        // build tree is already current, and QEMU will report a better
        // error than we can if they're wrong.
        opts.dry_run || opts.no_build,
    )
}

impl QemuInvocation {
    /// The command in a copy-pasteable form, for `--dry-run` and the
    /// CLI's `Running` line.
    pub fn command_line(&self) -> String {
        let mut line = shell_quote(&self.binary);
        for a in &self.args {
            line.push(' ');
            line.push_str(&shell_quote(&a.to_string_lossy()));
        }
        line
    }
}

/// Spawn QEMU from `invocation` and wait for it; see
/// [`spawn_and_wait`] for the timeout and signal handling.
pub fn launch(invocation: QemuInvocation) -> Result<ExitStatus> {
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

/// Spawn the QEMU binary and wait for it, enforcing `invocation.timeout`
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
fn spawn_and_wait(invocation: QemuInvocation) -> Result<ExitStatus> {
    let mut cmd = Command::new(&invocation.binary);
    cmd.args(&invocation.args);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::QemuBinaryNotFound {
                binary: invocation.binary.clone(),
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
                    return Err(Error::QemuTimeout);
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
        let inv = QemuInvocation {
            binary: "qemu-system-x86_64".into(),
            args: vec!["-m".into(), "128M".into(), "-append".into(), "a b".into()],
            timeout: None,
        };
        assert_eq!(
            inv.command_line(),
            "qemu-system-x86_64 -m 128M -append 'a b'"
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
