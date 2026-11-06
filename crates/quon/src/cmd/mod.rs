//! Shared command context and plumbing for `quon` subcommands.
//!
//! Each subcommand needs some subset of the same preamble work: walk
//! up from the current directory to find a `quon.rs`,
//! load the build model, pick a profile, resolve the config, probe
//! rustc, load the cache manifest, and construct a `CompileCtx`. Two
//! entry points are provided here:
//!
//! - [`build_context_at_for_driver`] does the full wiring including the
//!   rustc probe. Used by `build`, `check`, `clippy`, and `configure`
//!   — all of which genuinely need rustc metadata (for sysroot
//!   compilation, the metadata-only check pass, the lint pass, and the
//!   analyzer's `rust_src` field respectively). The driver flag flows
//!   down into [`quon_core::BuildLayout`] so each command's output
//!   lands in its own subdirectory namespace.
//!
//! - [`build_layout_context_at`] stops just before the rustc probe
//!   and returns a [`LayoutContext`] (the evaluated model, resolved
//!   config, build layout, and project root). Used by `clean` and
//!   `fmt`, which do not need rustc metadata. This is important:
//!   both are subcommands users reach for when their toolchain is
//!   broken or partially installed, so forcing a rustc probe would
//!   make the tools useless in exactly that situation.
//!
//! Both `_at` variants take the working directory explicitly so unit
//! tests can exercise the full wiring against a tempdir without
//! mutating process-wide `current_dir`. Every builder takes the
//! working directory from the caller (see [`crate::run`]), never from
//! the process.

pub mod build;
pub mod check;
pub mod clean;
pub mod clippy;
pub mod configure;
pub mod external;
pub mod fmt;
pub mod init;
pub mod manifest;
pub mod run;
pub mod setup;
pub mod test;
pub mod vendor;

use anyhow::{Context, Result};
use quon_core::config::{
    DEFAULT_ENV_PREFIX, DEFAULT_OVERRIDE_FILENAME, RunSelection, load_env_overrides,
    load_override_file, merge_overrides,
};
use quon_core::model::{BuildModel, ConfigValue, ResolvedConfig};
use quon_core::vendor as core_vendor;
use quon_core::{
    BuildLayout, CompileCtx, DriverKind, RustcInfo, find_project_root, load_build_model_for,
    resolve_config,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Everything a subcommand needs: the evaluated build model, the
/// resolved config for the active profile, the compile context
/// (layout + rustc info + cache), and where the project lives on disk.
pub struct CmdContext {
    /// The evaluated `quon.rs` model, kept around so subcommands can
    /// look up targets, crates, and groups referenced from the resolved
    /// config by handle.
    pub model: BuildModel,
    /// The resolved profile + options for this invocation.
    pub resolved: ResolvedConfig,
    /// Layout, rustc metadata, and cache manifest — everything rustc
    /// invocations need.
    pub ctx: CompileCtx,
    /// The directory containing `quon.rs` (the project root).
    ///
    /// Not consumed by today's three subcommands (`build`, `clean`,
    /// `configure`) — they all reach for `resolved.project_root`
    /// instead — but kept on the context so future subcommands and
    /// external plugins can route relative paths without re-walking
    /// from cwd.
    #[allow(dead_code)]
    pub project_root: PathBuf,
    /// The `-t` target and the config overrides `resolved` was resolved
    /// with, so a command can resolve another profile the same way.
    pub target: Option<String>,
    pub overrides: BTreeMap<String, ConfigValue>,
    /// The run entry the model was loaded for.
    pub selection: RunSelection,
}

/// The runner plugins built into the `quon` binary. Any other plugin
/// is found as `quon-runner-<name>` on `$PATH`.
pub fn runners() -> quon_core::run::Runners {
    let mut runners = quon_core::run::Runners::default();
    runners.register(quon_core::run::QEMU_PLUGIN, quon_runner_qemu::QemuRunner);
    runners
}

/// Build a [`CmdContext`] by walking up from `cwd`. Thin wrapper
/// around [`build_context_at_for_driver`]; the resulting context uses
/// [`DriverKind::Rustc`] (i.e. the historical `quon build` layout).
pub fn build_context(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    selection: &RunSelection,
) -> Result<CmdContext> {
    build_context_at_for_driver(
        cwd,
        profile,
        target,
        config_file,
        DriverKind::Rustc,
        selection,
    )
}

/// Build a [`CmdContext`] starting the project-root search at `cwd`,
/// using the requested driver flavor.
///
/// Both axes are needed for the check/clippy commands to share
/// behavior with the existing build/configure tests, since the
/// integration tests parameterize on cwd (tempdir) and the new
/// commands parameterize on driver.
pub fn build_context_at_for_driver(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    driver: DriverKind,
    selection: &RunSelection,
) -> Result<CmdContext> {
    let layout_ctx = build_layout_context_at(cwd, profile, target, config_file, selection)?;
    context_for_driver(layout_ctx, target, driver)
}

/// One [`CmdContext`] per run entry in `selections`, for a command that
/// builds them together. `quon.rs` runs and the workspace manifests are
/// read once for all of them.
pub fn build_contexts_for_driver(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    driver: DriverKind,
    selections: &[RunSelection],
) -> Result<Vec<CmdContext>> {
    let project_root = find_project_root(cwd)
        .context("could not find a quon.rs in the current directory or any parent")?;
    let models = quon_core::load_build_models_for(&project_root, selections)
        .context("failed to load build model")?;
    selections
        .iter()
        .zip(models)
        .map(|(selection, model)| {
            let opts = LayoutContextOpts {
                selection: selection.clone(),
                model: Some(model),
                ..LayoutContextOpts::default()
            };
            let layout_ctx =
                build_layout_context_at_with_opts(cwd, profile, target, config_file, opts)?;
            context_for_driver(layout_ctx, target, driver)
        })
        .collect()
}

/// The [`CmdContext`] of `layout_ctx`, with its layout flavored for
/// `driver`.
fn context_for_driver(
    layout_ctx: LayoutContext,
    target: Option<&str>,
    driver: DriverKind,
) -> Result<CmdContext> {
    let LayoutContext {
        model,
        resolved,
        layout,
        project_root,
        overrides,
        selection,
    } = layout_ctx;

    // Re-flavor the layout for the requested driver. The shared
    // `build_layout_context_at` constructs a Rustc-flavored layout by
    // default; for check/clippy we need to swap in a layout whose
    // user-crate path methods route under `tool/<driver>/`. Sysroot,
    // cache manifest, and config crate paths are unchanged because
    // they don't reference the user-crate root — see `BuildLayout`'s
    // doc comment for the rationale.
    let layout = BuildLayout::with_driver(
        layout.root().to_path_buf(),
        resolved.project.name.clone(),
        driver,
    );

    let rustc_info = Arc::new(
        RustcInfo::load_or_probe(&layout).context("failed to load or probe rustc metadata")?,
    );
    let ctx = CompileCtx::new(layout, rustc_info);

    Ok(CmdContext {
        model,
        resolved,
        ctx,
        project_root,
        target: target.map(str::to_owned),
        overrides,
        selection,
    })
}

/// Lighter-weight context used by subcommands that don't need rustc.
///
/// Contains everything up to and including the resolved config and
/// build layout, but not a [`CompileCtx`] — that would force a rustc
/// probe, which is exactly what we want to avoid for `clean`.
pub struct LayoutContext {
    pub model: BuildModel,
    pub resolved: ResolvedConfig,
    pub layout: BuildLayout,
    pub project_root: PathBuf,
    /// The `.quon-config` and `QUON_*` overrides `resolved` used.
    pub overrides: BTreeMap<String, ConfigValue>,
    /// The run entry the model was loaded for.
    pub selection: RunSelection,
}

/// Build a [`LayoutContext`] (no rustc probe) starting at `cwd`.
///
/// Used by `clean` so that a user with a broken or missing toolchain
/// can still wipe the build directory. `build` and `configure` go
/// through [`build_context_at_for_driver`] instead because they need the rustc
/// probe — `build` for sysroot compilation, `configure` for the
/// analyzer's `rust_src` field.
///
/// Thin wrapper around [`build_layout_context_at_with_opts`] with
/// vendor auto-registration enabled; use the `_with_opts` variant
/// when a subcommand needs to bypass auto-registration (`quon
/// vendor` itself must, to avoid chicken-and-egg failures on stale
/// lockfiles).
pub fn build_layout_context_at(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    selection: &RunSelection,
) -> Result<LayoutContext> {
    build_layout_context_at_with_opts(
        cwd,
        profile,
        target,
        config_file,
        LayoutContextOpts {
            selection: selection.clone(),
            ..LayoutContextOpts::default()
        },
    )
}

/// Options tweaking the model load sequence.
///
/// Exists so `quon vendor` can short-circuit auto-registration while
/// every other subcommand gets the default
/// register-vendored-deps-before-resolve behavior.
#[derive(Debug, Clone)]
pub struct LayoutContextOpts {
    /// Skip the [`quon_core::vendor::auto_register_vendored_deps`]
    /// pass. Needed by `quon vendor --check` / sync because they
    /// *are* the thing that fixes a stale lock — running the
    /// auto-register with a stale lock would abort before the fix can
    /// run.
    pub skip_vendor_autoreg: bool,
    /// The run entry to load the model for.
    pub selection: RunSelection,
    /// That entry's model, already loaded, as
    /// [`build_contexts_for_driver`] loads several at once.
    pub model: Option<BuildModel>,
}

impl Default for LayoutContextOpts {
    fn default() -> Self {
        Self {
            skip_vendor_autoreg: false,
            selection: RunSelection::All,
            model: None,
        }
    }
}

/// Full-control version of [`build_layout_context_at`]. Every public
/// wrapper ultimately funnels through this.
pub fn build_layout_context_at_with_opts(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    opts: LayoutContextOpts,
) -> Result<LayoutContext> {
    let project_root = find_project_root(cwd)
        .context("could not find a quon.rs in the current directory or any parent")?;
    let mut model = match opts.model {
        Some(model) => model,
        None => load_build_model_for(&project_root, &opts.selection)
            .context("failed to load build model")?,
    };

    // --- Vendor auto-registration ---
    //
    // Runs after the build model is loaded (intern pass already
    // happened inside `load_build_model`) and before `resolve_config`.
    // Reads `quon.lock`, verifies its fingerprint against the live
    // model, and inserts synthetic CrateDefs for every pinned package
    // so the compile path sees vendored crates as ordinary model
    // entries. `quon vendor` skips this via `opts.skip_vendor_autoreg`
    // because *it* is the thing that fixes a stale lock — running the
    // check here would abort before the fix could run.
    if !opts.skip_vendor_autoreg {
        let layout_for_vendor = BuildLayout::new(
            project_root.join("build"),
            // Use a placeholder name here — `auto_register_vendored_deps`
            // only reads the build root and project root, not the
            // project name, so we don't have to wait for
            // `resolve_config` to learn it.
            "quon-vendor-autoreg",
        );
        core_vendor::auto_register_vendored_deps(&mut model, &layout_for_vendor, &project_root)
            .context("failed to auto-register vendored dependencies")?;
    }

    // Pick a default profile when the user didn't pass `-p` or `-r`;
    // see `default_profile`.
    let default_name;
    let profile_name = match profile {
        Some(p) => p,
        None => {
            default_name = default_profile(&model).map(str::to_owned);
            match default_name.as_deref() {
                Some(n) => n,
                None => {
                    return Err(anyhow::anyhow!(
                        "no profiles declared in quon.rs; add at least one \
                         `profile(...)` definition or pass `-p <name>`"
                    ));
                }
            }
        }
    };

    let overrides = load_overrides(&project_root, config_file)?;

    let overrides_arg = if overrides.is_empty() {
        None
    } else {
        Some(&overrides)
    };
    let resolved = resolve_config(&model, profile_name, target, &project_root, overrides_arg)
        .context("failed to resolve config")?;

    let layout = BuildLayout::new(project_root.join("build"), &resolved.project.name);

    Ok(LayoutContext {
        model,
        resolved,
        layout,
        project_root,
        overrides,
        selection: opts.selection,
    })
}

/// Thin wrapper around [`build_layout_context_at`] that reads cwd.
pub fn build_layout_context(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    selection: &RunSelection,
) -> Result<LayoutContext> {
    build_layout_context_at(cwd, profile, target, config_file, selection)
}

/// Per-checkout overrides: the file (`-C` names an explicit path,
/// otherwise `<root>/.quon-config` if present), with `QUON_*`
/// environment variables layered on top. An absent default file is not
/// an error; an absent explicit file is. Env beats file on conflicts
/// (see `config::overrides::merge_overrides`).
fn load_overrides(
    project_root: &Path,
    config_file: Option<&Path>,
) -> Result<BTreeMap<String, ConfigValue>> {
    let file_path = match config_file {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let default = project_root.join(DEFAULT_OVERRIDE_FILENAME);
            default.exists().then_some(default)
        }
    };
    let file_overrides = match &file_path {
        Some(p) => {
            if config_file.is_some() && !p.exists() {
                return Err(anyhow::anyhow!(
                    "config override file {} does not exist",
                    p.display()
                ));
            }
            load_override_file(p).context("failed to read config override file")?
        }
        None => Default::default(),
    };
    let env_overrides = load_env_overrides(DEFAULT_ENV_PREFIX);
    Ok(merge_overrides(file_overrides, env_overrides))
}

/// The override key that names the default run entry: `RUN = "<name>"`
/// in `.quon-config`, or `QUON_RUN`. A config option named `RUN` would
/// read the same value.
const RUN_OVERRIDE: &str = "RUN";

/// What a command does when neither the command line nor `RUN` names a
/// run entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unnamed {
    /// Every entry, one after another (`build`, `check`, `clippy`).
    EachEntry,
    /// The project's only entry; several are an error (`run`,
    /// `configure`).
    Only,
    /// With several entries, none, so only the host tests run
    /// (`test`).
    HostTestsOnly,
}

/// The run entries a command loads the model for, one pass each, in
/// declaration order. `entry` is the name given on the command line.
pub fn selections(
    cwd: &Path,
    entry: Option<&str>,
    config_file: Option<&Path>,
    unnamed: Unnamed,
) -> Result<Vec<RunSelection>> {
    let project_root = find_project_root(cwd)
        .context("could not find a quon.rs in the current directory or any parent")?;
    let default = run_override(&load_overrides(&project_root, config_file)?)?;
    if let Some(name) = entry.map(str::to_owned).or(default) {
        return Ok(vec![RunSelection::Named(name)]);
    }
    let several = || -> Result<Vec<String>> {
        let names =
            quon_core::run_entry_names(&project_root).context("failed to read the run entries")?;
        Ok(if names.len() > 1 { names } else { Vec::new() })
    };
    Ok(match unnamed {
        Unnamed::Only => vec![RunSelection::Only],
        Unnamed::EachEntry => match several()? {
            names if names.is_empty() => vec![RunSelection::Only],
            names => names.into_iter().map(RunSelection::Named).collect(),
        },
        Unnamed::HostTestsOnly if several()?.is_empty() => vec![RunSelection::Only],
        Unnamed::HostTestsOnly => vec![RunSelection::None],
    })
}

/// The `RUN` override, if set.
fn run_override(overrides: &BTreeMap<String, ConfigValue>) -> Result<Option<String>> {
    match overrides.get(RUN_OVERRIDE) {
        None => Ok(None),
        Some(ConfigValue::Str(s)) => Ok(Some(s.clone())),
        // An unquoted all-digit name parses as a number.
        Some(ConfigValue::U64(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(anyhow::anyhow!(
            "{RUN_OVERRIDE} must name a run entry, got {other:?}"
        )),
    }
}

/// Pick the default profile when the user did not pass `-p/--profile`.
///
/// Precedence:
///
/// 1. `project().default_profile("...")` if set — this is the user's
///    explicit choice and we prefer it over any derived default.
///    Validation at intern time guarantees the named profile exists,
///    so we can dereference it here without re-checking.
/// 2. Otherwise the first profile name in alphabetical order
///    (`Arena::names` iterates over a `BTreeMap`, so this is
///    deterministic). Historically this was the only behaviour; it
///    remains a sensible fallback for single-profile projects, but
///    is a footgun for `debug`/`dev`/`release` — which is exactly
///    what the explicit `default_profile` field is there to prevent.
///
/// Returns `None` only if the model declares no profiles at all.
fn default_profile(model: &BuildModel) -> Option<&str> {
    if let Some(project) = model.project.as_ref()
        && let Some(name) = project.default_profile.as_deref()
    {
        return Some(name);
    }
    model.profiles.names().next().map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write the smallest possible valid `quon.rs` that evaluates
    /// and resolves cleanly — a project and a builtin default target,
    /// which the built-in `dev` profile builds for. No crates required.
    fn write_min_script(dir: &Path) {
        let script = r#"use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("demo", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));
    Ok(())
}
"#;
        fs::write(dir.join("quon.rs"), script).expect("write quon.rs");
    }

    // These tests exercise the full context-building pipeline, which
    // calls `RustcInfo::load_or_probe` and therefore spawns `rustc`.
    // They follow the same probe-or-skip + `#[ignore]` pattern as the
    // scheduler e2e tests so they stay opt-in in sandboxed CI.

    #[test]
    #[ignore]
    fn build_context_at_wires_up_a_tempdir_project() {
        if quon_core::RustcInfo::probe().is_err() {
            eprintln!("cli e2e test skipped: rustc probe failed");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        write_min_script(tmp.path());

        let cmd = build_context_at_for_driver(
            tmp.path(),
            None,
            None,
            None,
            quon_core::DriverKind::Rustc,
            &RunSelection::Only,
        )
        .expect("build_context_at_for_driver");
        assert_eq!(cmd.resolved.project.name, "demo");
        assert_eq!(cmd.resolved.profile.name, "dev");
        // project_root should be the tempdir (canonicalized by find_project_root).
        assert!(
            cmd.project_root.canonicalize().ok() == tmp.path().canonicalize().ok()
                || cmd.project_root == tmp.path()
        );
    }

    #[test]
    #[ignore]
    fn default_profile_is_dev_unless_the_script_names_one() {
        if quon_core::RustcInfo::probe().is_err() {
            eprintln!("cli e2e test skipped: rustc probe failed");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        // `alpha` sorts ahead of `dev`, so a fallback to the first
        // profile by name would pick it.
        let script = r#"use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("demo", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));
    q.add(Profile::new("zeta"));
    q.add(Profile::new("alpha"));
    Ok(())
}
"#;
        fs::write(tmp.path().join("quon.rs"), script).expect("write");
        let cmd = build_context_at_for_driver(
            tmp.path(),
            None,
            None,
            None,
            quon_core::DriverKind::Rustc,
            &RunSelection::Only,
        )
        .expect("build_context_at_for_driver");
        assert_eq!(cmd.resolved.profile.name, "dev");
    }
}
