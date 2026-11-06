//! Quon as a library: the `quon` command line, run in-process, and a
//! read-only view of a project's `quon.rs`.
//!
//! A tool that drives Quon, such as a repository's `xtask`, can depend
//! on this crate instead of a `quon` binary on `$PATH`, so the Cargo
//! lock pins the Quon version it runs:
//!
//! ```no_run
//! use std::path::Path;
//!
//! let root = Path::new("/path/to/project");
//! let code = quon::run(root, ["build", "--release"]);
//! let project = quon::Project::load(root)?;
//! for entry in project.entries() {
//!     let elf = project.artifact(entry, "release")?;
//!     println!("{:?}: {}", entry.name, elf.display());
//! }
//! # let _ = code;
//! # Ok::<(), quon::Error>(())
//! ```
//!
//! [`run`] and [`Project`] are the whole library API. Like the command
//! line, they change only with an entry in the changelog. Everything
//! else, quon-core included, is internal.

mod cli;
mod cmd;
mod output;
mod project;

pub use project::{Entry, Error, Project};

use anyhow::Result;
use clap::Parser;
use output::Shell;
use std::ffi::OsString;
use std::path::Path;

/// The status a run ends with: what the `quon` binary exits with.
pub use std::process::ExitCode;

/// An error that ends a command with `code` after the command has
/// printed its own report, so [`run`] prints nothing more.
#[derive(Debug)]
pub(crate) struct Exit(pub u8);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// Run the `quon` command line in-process, as `quon <args>` would run
/// from `dir`: `args` leaves out the program name. Output and errors go
/// to stdout and stderr as the binary prints them, and the result is
/// the status the binary would exit with.
///
/// `dir` takes the place of the current directory: Quon searches
/// upward from it for `quon.rs`, and relative paths in `args` resolve
/// against it. The process's own current directory is not read or
/// changed.
///
/// `quon run` handles SIGINT and SIGTERM while its runner is up, to
/// stop the runner cleanly. Its first use registers handlers for both
/// that give them their default action outside a run, so a host
/// program's own handlers for them no longer apply after it.
pub fn run<I, S>(dir: &Path, args: I) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let started = std::time::Instant::now();
    let argv = std::iter::once(OsString::from("quon")).chain(args.into_iter().map(Into::into));
    let cli = match cli::Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            // `--help` and `--version` come here too, and succeed.
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let dir = match std::path::absolute(dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };
    let mut shell = Shell::new(cli.color.into(), cli.quiet, cli.verbose, started);
    match run_cli(cli.in_dir(&dir), &dir, &mut shell) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => match e.downcast_ref::<Exit>() {
            Some(Exit(code)) => ExitCode::from(*code),
            None => {
                shell.error(&e);
                ExitCode::FAILURE
            }
        },
    }
}

fn run_cli(cli: cli::Cli, cwd: &Path, shell: &mut Shell) -> Result<()> {
    // External subcommands dispatch before building any context — they
    // may be invoked outside any quon project, so they don't need a
    // project root or an evaluated build model.
    if let cli::Command::External(args) = cli.command {
        return cmd::external::run(args, cwd);
    }

    // `init` writes the quon.rs that every other command needs.
    if let cli::Command::Init(args) = &cli.command {
        return cmd::init::run_at(cwd, args);
    }

    // `setup` prepares $QUON_HOME; it needs no quon.rs.
    if let cli::Command::Setup = &cli.command {
        return cmd::setup::run();
    }

    // `manifest` reads a Cargo workspace directly; it needs no quon.rs.
    if let cli::Command::Manifest(sub) = &cli.command {
        return cmd::manifest::run(sub, cli.target.as_deref(), cwd);
    }

    // `clean` and `fmt` take the lighter layout-only context (no
    // rustc probe) so they still work when the toolchain is broken
    // or missing. `fmt` only needs `rustfmt`, which it resolves
    // separately via $RUSTFMT or $PATH.
    let all = [quon_core::config::RunSelection::All];
    if let cli::Command::Clean(args) = &cli.command {
        let ctx = cmd::build_layout_context(
            cwd,
            cli.profile_name(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            &all[0],
        )?;
        return cmd::clean::run(ctx, args.keep_sysroot);
    }
    if let cli::Command::Fmt(args) = &cli.command {
        let ctx = cmd::build_layout_context(
            cwd,
            cli.profile_name(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            &all[0],
        )?;
        return cmd::fmt::run(ctx, args.check);
    }
    // `vendor` loads the model itself: it is the thing that fixes a
    // stale lock or vendor directory, so it must not fail on one first.
    if let cli::Command::Vendor(args) = &cli.command {
        return cmd::vendor::run(
            cwd,
            cli.profile_name(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            args,
        );
    }

    // A project with several run entries loads one model per entry, so
    // Cargo features resolve per entry; see `quon_core::config::select_run`.
    let unnamed = match &cli.command {
        cli::Command::Build(_) | cli::Command::Check(_) | cli::Command::Clippy(_) => {
            cmd::Unnamed::EachEntry
        }
        cli::Command::Test(_) => cmd::Unnamed::HostTestsOnly,
        _ => cmd::Unnamed::Only,
    };
    let selections = cmd::selections(cwd, cli.entry(), cli.config_file.as_deref(), unnamed)?;

    // `check` and `clippy` build a context whose layout is flavored
    // for the relevant driver — that's what routes user-crate output
    // under `build/tool/<driver>/` so it can't clobber `quon build`
    // artifacts. Same rustc probe path otherwise; both still need the
    // host toolchain.
    let driver = match &cli.command {
        cli::Command::Check(_) => quon_core::DriverKind::Check,
        cli::Command::Clippy(_) => quon_core::DriverKind::Clippy,
        _ => quon_core::DriverKind::Rustc,
    };
    if let cli::Command::Build(_) | cli::Command::Check(_) | cli::Command::Clippy(_) = &cli.command
    {
        let mut ctxs = cmd::build_contexts_for_driver(
            cwd,
            cli.profile_name(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            driver,
            &selections,
        )?;
        let lint_args = match &cli.command {
            cli::Command::Clippy(a) => a.lint_args.clone(),
            _ => Vec::new(),
        };
        for ctx in &mut ctxs {
            ctx.ctx.verbose = cli.verbose;
            ctx.ctx.color = shell.color();
            ctx.ctx.lint_args.clone_from(&lint_args);
        }
        if ctxs.len() > 1 {
            // Several entries share one schedule; see
            // `quon_core::build_entries_with_reporter`.
            let noun = match &cli.command {
                cli::Command::Build(_) => "built",
                cli::Command::Check(_) => "checked",
                _ => "linted",
            };
            return cmd::build::run_entries(&ctxs, cli.jobs, shell, noun);
        }
        let ctx = ctxs.pop().expect("selections are never empty");
        match &cli.command {
            cli::Command::Build(_) => cmd::build::run(ctx, cli.jobs, shell)?,
            cli::Command::Check(_) => cmd::check::run(ctx, cli.jobs, shell)?,
            _ => cmd::clippy::run(ctx, cli.jobs, shell)?,
        }
        return Ok(());
    }

    let [selection] = selections.as_slice() else {
        unreachable!("only build, check and clippy take each entry in turn")
    };
    let mut ctx = cmd::build_context(
        cwd,
        cli.profile_name(),
        cli.target.as_deref(),
        cli.config_file.as_deref(),
        selection,
    )?;
    ctx.ctx.verbose = cli.verbose;
    ctx.ctx.color = shell.color();

    match cli.command {
        cli::Command::Configure(args) => cmd::configure::run(ctx, args.output),
        cli::Command::Run(args) => cmd::run::run(ctx, cli.jobs, args, shell, cwd),
        cli::Command::Test(args) => cmd::test::run(ctx, cli.jobs, args, shell),
        cli::Command::Build(_)
        | cli::Command::Check(_)
        | cli::Command::Clippy(_)
        | cli::Command::Clean(_)
        | cli::Command::Fmt(_)
        | cli::Command::Init(_)
        | cli::Command::Setup
        | cli::Command::Vendor(_)
        | cli::Command::Manifest(_)
        | cli::Command::External(_) => {
            unreachable!("handled above")
        }
    }
}
