//! Quon command-line entry point.
//!
//! Parses argv via clap, dispatches to the appropriate subcommand
//! module, and converts any returned error into a non-zero exit status.
//! The actual command logic lives in `cmd::{build,clean,configure,external}`;
//! this file is intentionally thin so it's easy to audit.

mod cli;
mod cmd;
mod output;

use anyhow::Result;
use clap::Parser;
use output::Shell;

fn main() {
    let started = std::time::Instant::now();
    let cli = cli::Cli::parse();
    let mut shell = Shell::new(cli.color.into(), cli.quiet, cli.verbose, started);
    if let Err(e) = run(cli, &mut shell) {
        shell.error(&e);
        std::process::exit(1);
    }
}

fn run(cli: cli::Cli, shell: &mut Shell) -> Result<()> {
    // External subcommands dispatch before building any context — they
    // may be invoked outside any quon project, so they don't need a
    // project root or an evaluated build model.
    if let cli::Command::External(args) = cli.command {
        return cmd::external::run(args);
    }

    // `init` writes the quon.rs that every other command needs.
    if let cli::Command::Init(args) = &cli.command {
        return cmd::init::run(args);
    }

    // `setup` prepares $QUON_HOME; it needs no quon.rs.
    if let cli::Command::Setup = &cli.command {
        return cmd::setup::run();
    }

    // `manifest` reads a Cargo workspace directly; it needs no quon.rs.
    if let cli::Command::Manifest(sub) = &cli.command {
        return cmd::manifest::run(sub, cli.target.as_deref());
    }

    // `clean` and `fmt` take the lighter layout-only context (no
    // rustc probe) so they still work when the toolchain is broken
    // or missing. `fmt` only needs `rustfmt`, which it resolves
    // separately via $RUSTFMT or $PATH.
    let all = [quon_core::config::RunSelection::All];
    if let cli::Command::Clean(args) = &cli.command {
        let ctx = cmd::build_layout_context(
            cli.profile_name(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            &all[0],
        )?;
        return cmd::clean::run(ctx, args.keep_sysroot);
    }
    if let cli::Command::Fmt(args) = &cli.command {
        let ctx = cmd::build_layout_context(
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
        let cwd = std::env::current_dir()?;
        return cmd::vendor::run(
            &cwd,
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
    let selections = cmd::selections(cli.entry(), cli.config_file.as_deref(), unnamed)?;

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
            cli.profile_name(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            driver,
            &selections,
        )?;
        for ctx in &mut ctxs {
            ctx.ctx.verbose = cli.verbose;
            ctx.ctx.color = shell.color();
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
        cli.profile_name(),
        cli.target.as_deref(),
        cli.config_file.as_deref(),
        selection,
    )?;
    ctx.ctx.verbose = cli.verbose;
    ctx.ctx.color = shell.color();

    match cli.command {
        cli::Command::Configure(args) => cmd::configure::run(ctx, args.output),
        cli::Command::Run(args) => cmd::run::run(ctx, cli.jobs, args, shell),
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
