//! Clap command-line definition for the `gluon` binary.
//!
//! The root [`Cli`] struct captures global flags (profile, target, jobs,
//! verbose, quiet) that apply to every subcommand, plus a [`Command`]
//! enum that selects between the `build`, `clean`, `configure`
//! subcommands and the `external_subcommand` plugin arm.
//!
//! Parsing lives here so `main.rs` stays a thin dispatcher; keeping the
//! argv schema in one file makes it easier to unit-test without spinning
//! up the whole command pipeline.

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

/// Parse `-j/--jobs`. Rejects zero with a friendly message instead of
/// the numeric-overflow noise rustc would produce later in the
/// scheduler.
fn parse_jobs(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("'{s}' is not a number"))?;
    if n == 0 {
        return Err("must be at least 1 (use no -j flag for the default)".into());
    }
    Ok(n)
}

/// Values of `--color`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

impl From<ColorWhen> for anstream::ColorChoice {
    fn from(w: ColorWhen) -> Self {
        match w {
            ColorWhen::Auto => Self::Auto,
            ColorWhen::Always => Self::Always,
            ColorWhen::Never => Self::Never,
        }
    }
}

/// Top-level `gluon` command-line interface.
#[derive(Parser, Debug)]
#[command(name = "gluon", version, about = "Bare-metal Rust kernel build system")]
pub struct Cli {
    /// Profile name to use (overrides the default in gluon.rs).
    #[arg(short = 'p', long, global = true)]
    pub profile: Option<String>,

    /// Target name to use (overrides the profile's pinned target).
    #[arg(short = 't', long, global = true)]
    pub target: Option<String>,

    /// Override file with `KEY = value` entries (defaults to
    /// `<project_root>/.gluon-config` when present).
    ///
    /// Values from this file are merged on top of the defaults declared
    /// in `gluon.rs`. Environment variables prefixed `GLUON_` win over
    /// the file. See `gluon_core::config::overrides` for the grammar.
    #[arg(short = 'C', long = "config-file", global = true)]
    pub config_file: Option<PathBuf>,

    /// Number of parallel compile jobs (defaults to available parallelism).
    ///
    /// Must be at least 1; clap rejects `-j 0` at parse time via
    /// [`parse_jobs`] so the scheduler never sees a worker count that
    /// would deadlock the ready queue.
    #[arg(short = 'j', long, global = true, value_parser = parse_jobs)]
    pub jobs: Option<usize>,

    /// Emit more verbose output.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Suppress non-error output.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// When to color output: `auto` colors only when stderr is a
    /// terminal and `NO_COLOR` is unset.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub color: ColorWhen,

    /// Which subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands dispatched by the `gluon` binary.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build the project.
    Build(BuildArgs),
    /// Run a metadata-only check pass over every crate in the build
    /// model. Equivalent to `cargo check` but uses gluon's per-crate
    /// flag assembly so the same `--cfg`s, target, sysroot, and
    /// extern deps that `gluon build` would use are applied here too.
    Check(CheckArgs),
    /// Run clippy over every crate in the build model. Same per-crate
    /// flag assembly as `gluon build`, but the program path swaps
    /// `rustc` for `clippy-driver` (resolved via `$CLIPPY_DRIVER`,
    /// then a sibling-of-rustc heuristic, then `$PATH`).
    Clippy(ClippyArgs),
    /// Write a `gluon.rs` for the Cargo workspace in the current
    /// directory, from what its manifests and toolchain files declare.
    Init(InitArgs),
    /// Run `rustfmt` over every crate. Pass `--check` to verify
    /// formatting without rewriting (matches `cargo fmt --check`).
    Fmt(FmtArgs),
    /// Remove the build directory.
    Clean(CleanArgs),
    /// Generate `rust-project.json` for rust-analyzer.
    Configure(ConfigureArgs),
    /// Build the gluon-config library that gluon.rs links against, for
    /// the rustc in the current directory, and print its directory.
    ///
    /// The library is built once per gluon version and rustc into
    /// `$GLUON_HOME` (default `~/.gluon`) and shared by every project.
    /// Other commands build it on first use; `setup` does it ahead of
    /// time, for example while a network is available.
    Setup,
    /// Vendor external dependencies.
    ///
    /// For the Cargo workspace named by `cfg.cargo_workspace`, runs
    /// `cargo vendor --versioned-dirs --locked` into the configured
    /// vendor directory (default `vendor/`) and stamps it with the
    /// `Cargo.lock` hash. For `cfg.dependency()` declarations,
    /// synthesises a scratch `Cargo.toml`, invokes `cargo vendor` to
    /// populate `./vendor/`, and writes `gluon.lock` pinning the
    /// result. `--check` verifies without touching disk; `--force`
    /// bypasses the fingerprint-match fast path.
    Vendor(VendorArgs),
    /// Build the project and start the run binary with the profile's
    /// runner.
    ///
    /// A QEMU runner picks between direct-kernel boot (`-kernel <elf>`)
    /// and UEFI (OVMF pflash): `--uefi`/`--direct` flags, then the
    /// profile's `qemu_boot_mode(...)`, then the runner's
    /// `.boot_mode(...)`, then direct. A command runner runs its argv.
    /// Extra arguments after `--` are appended to the runner's.
    Run(RunArgs),
    /// Build and run the tests: host tests, then integration tests,
    /// then boot tests.
    ///
    /// Host tests are the unit tests and `harness = true` tests of the
    /// packages in `cfg.host_tests()`, run on the host. Integration
    /// tests are the `harness = false` targets in `tests/` of packages
    /// named with `group.package(...)`; each boots the way `gluon run`
    /// boots the run binary, with the test binary in its place.
    /// Boot tests (`cfg.boot_test(...)`) run a profile's run binary.
    /// Both pass when the runner exits with its success status. Every
    /// run happens even when an earlier one fails.
    Test(TestArgs),
    /// Inspect the Cargo workspace as Gluon reads it. Debug output only.
    #[command(subcommand)]
    Manifest(ManifestCommand),
    /// Dispatch to an external `gluon-<name>` binary on `$PATH`.
    ///
    /// Clap's `external_subcommand` captures everything after the
    /// unknown command name, and the first element of the returned
    /// vector is the subcommand name itself (not stripped by clap).
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Arguments for `gluon build`.
#[derive(clap::Args, Debug)]
pub struct BuildArgs {}

/// Arguments for `gluon check`. Currently empty — the same global
/// `-p`/`-t`/`-j`/`-C` flags that apply to `build` apply here.
#[derive(clap::Args, Debug)]
pub struct CheckArgs {}

/// Arguments for `gluon clippy`. Currently empty.
#[derive(clap::Args, Debug)]
pub struct ClippyArgs {}

/// Arguments for `gluon fmt`.
#[derive(clap::Args, Debug)]
pub struct FmtArgs {
    /// Verify formatting without rewriting files (mirrors
    /// `cargo fmt --check`). Exit code is non-zero if any crate has
    /// unformatted files.
    #[arg(long)]
    pub check: bool,
}

/// Arguments for `gluon init`.
#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Target triple or `.json` target spec; repeatable. The first one
    /// builds the workspace. Default: `build.target` in
    /// `.cargo/config.toml`, else `targets` in `rust-toolchain.toml`.
    // The id matches the global `--target`, so clap does not add that
    // flag to `init` a second time.
    #[arg(id = "target", short = 't', long = "target", value_name = "TRIPLE")]
    pub targets: Vec<String>,
    /// Workspace package that holds the kernel. Default: the only
    /// package with a bin target.
    #[arg(long, value_name = "PACKAGE")]
    pub kernel: Option<String>,
    /// How QEMU boots the kernel. Default: `limine` with `--limine`,
    /// else `none`.
    #[arg(long, value_enum)]
    pub bootloader: Option<Bootloader>,
    /// Limine release to fetch. The SHA-256 is left empty, so the first
    /// build fails and prints it.
    #[arg(long, value_name = "VERSION")]
    pub limine: Option<String>,
    /// Overwrite an existing `gluon.rs`.
    #[arg(long)]
    pub force: bool,
    /// Print the file instead of writing it.
    #[arg(long)]
    pub dry_run: bool,
}

/// How the generated `gluon.rs` boots the kernel.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bootloader {
    /// Limine over UEFI: Limine's `BOOTX64.EFI`, `limine.conf` and the
    /// kernel on an ESP.
    Limine,
    /// The kernel is a UEFI application on an ESP.
    Uefi,
    /// Build only; the QEMU section is commented out.
    None,
}

/// Arguments for `gluon clean`.
#[derive(clap::Args, Debug)]
pub struct CleanArgs {
    /// Keep the custom sysroot directory (default: wipe the whole build directory).
    #[arg(long)]
    pub keep_sysroot: bool,
}

/// Arguments for `gluon configure`.
#[derive(clap::Args, Debug)]
pub struct ConfigureArgs {
    /// Output path for `rust-project.json` (default: `<project_root>/rust-project.json`).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

/// Arguments for `gluon run`.
#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Force UEFI boot (OVMF pflash). Overrides the profile's
    /// `qemu().boot_mode(...)` setting. Mutually exclusive with
    /// `--direct` and `--bios`.
    #[arg(long, conflicts_with_all = ["direct", "bios"])]
    pub uefi: bool,
    /// Force direct-kernel boot (`-kernel <path>`). Overrides the
    /// profile's `qemu().boot_mode(...)` setting. Mutually exclusive
    /// with `--bios`.
    #[arg(long, conflicts_with = "bios")]
    pub direct: bool,
    /// Force legacy-BIOS boot (SeaBIOS, no `-kernel`, no pflash). Boots
    /// from the disk attached via `qemu().disk_*(...)` (or the legacy
    /// `qemu().esp_image(...)` raw image). Overrides the profile's
    /// `qemu().boot_mode(...)` setting.
    #[arg(long)]
    pub bios: bool,
    /// Wall-clock timeout in seconds. The runner is killed on expiry and
    /// gluon exits with a non-zero status. Overrides the profile's
    /// `test_timeout`.
    #[arg(short = 'T', long)]
    pub timeout: Option<u64>,
    /// Do not start the runner; print the assembled argv and exit 0.
    /// Useful for sanity-checking a config and for CI integration
    /// tests.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the implicit build step. Starts the runner directly
    /// against whatever run binary is currently on disk.
    ///
    /// Intended for tight edit/run loops where the user has just
    /// built by hand and wants to skip gluon's fingerprint sweep. If
    /// the binary is stale or missing, the runner will report a
    /// clearer error than gluon could from the outside.
    #[arg(long)]
    pub no_build: bool,
    /// Start QEMU with a GDB server on TCP :1234 and halt the guest
    /// CPU before executing the first instruction (`-s -S` under
    /// the hood). Attach with `target remote :1234` from gdb. The
    /// hint line is printed to stderr before the spawn so CI logs
    /// pick it up too.
    #[arg(long)]
    pub gdb: bool,
    /// Arguments after `--` are appended verbatim to the runner's.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub extra: Vec<OsString>,
}

/// Arguments for `gluon test`.
#[derive(clap::Args, Debug)]
pub struct TestArgs {
    /// Passed to each host test as libtest's filter. Integration and
    /// boot tests run only when their name contains it.
    pub filter: Option<String>,
    /// Run the host tests. With none of `--host`, `--integration` and
    /// `--boot`, every kind runs.
    #[arg(long)]
    pub host: bool,
    /// Run the integration tests.
    #[arg(long)]
    pub integration: bool,
    /// Run the boot tests.
    #[arg(long)]
    pub boot: bool,
    /// Force UEFI boot. Mutually exclusive with `--direct`.
    #[arg(long, conflicts_with = "direct")]
    pub uefi: bool,
    /// Force direct-kernel boot (`-kernel <path>`).
    #[arg(long)]
    pub direct: bool,
    /// Per-test timeout in seconds. Default: the profile's
    /// `test_timeout`, then the QEMU runner's `.test_timeout(...)`,
    /// then 60.
    #[arg(short = 'T', long)]
    pub timeout: Option<u64>,
    /// Print each test's command instead of building and running it.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the build and run the test binaries already on disk.
    #[arg(long)]
    pub no_build: bool,
    /// Arguments after `--` are appended to the runner's for every test.
    #[arg(last = true, allow_hyphen_values = true)]
    pub extra: Vec<OsString>,
}

/// Arguments for `gluon vendor`.
#[derive(clap::Args, Debug)]
pub struct VendorArgs {
    /// Verify the existing vendor tree without modifying anything.
    ///
    /// Exits with a non-zero status if `gluon.lock` is missing, its
    /// fingerprint disagrees with the declared deps, or any vendored
    /// crate's on-disk checksum has drifted.
    #[arg(long)]
    pub check: bool,

    /// Ignore the fingerprint-match fast path and re-run `cargo
    /// vendor` unconditionally. Useful after hand-editing `vendor/`.
    #[arg(long)]
    pub force: bool,

    /// Pass `--offline` / `--frozen` through to cargo. Forbids any
    /// network access; expects the lockfile to already be up to date.
    #[arg(long)]
    pub offline: bool,
}

/// `gluon manifest` subcommands.
#[derive(Subcommand, Debug)]
pub enum ManifestCommand {
    /// Print every package the workspace reaches, with inherited fields,
    /// targets, features and dependency edges.
    Packages(ManifestArgs),
    /// Resolve features and print the unit graph. The global
    /// `-t/--target` takes a builtin triple or a path to a `.json` spec;
    /// without it the roots resolve for the host.
    Units(ManifestUnitsArgs),
}

/// Arguments for `gluon manifest units`.
#[derive(clap::Args, Debug)]
pub struct ManifestUnitsArgs {
    #[command(flatten)]
    pub workspace: ManifestArgs,
    /// Workspace package to resolve as a root; repeatable (default:
    /// every member that is not a proc macro).
    #[arg(long = "root", value_name = "PACKAGE")]
    pub roots: Vec<String>,
    /// Features to enable on every root, comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub features: Vec<String>,
    /// Do not enable the roots' `default` features.
    #[arg(long)]
    pub no_default_features: bool,
}

/// Where `gluon manifest` finds the workspace.
#[derive(clap::Args, Debug)]
pub struct ManifestArgs {
    /// Directory holding the workspace `Cargo.toml` (default: current
    /// directory).
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,
    /// Directory holding vendored registry sources. Without it, registry
    /// sources come from Cargo's registry cache, as in a build that names
    /// no vendor directory.
    #[arg(long, value_name = "DIR")]
    pub vendor_dir: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_bare_build() {
        let cli = Cli::try_parse_from(["gluon", "build"]).expect("parse");
        assert!(matches!(cli.command, Command::Build(_)));
        assert_eq!(cli.profile, None);
    }

    #[test]
    fn parses_check_with_profile_and_jobs() {
        let cli = Cli::try_parse_from(["gluon", "-p", "dev", "-j", "2", "check"]).expect("parse");
        assert_eq!(cli.profile.as_deref(), Some("dev"));
        assert_eq!(cli.jobs, Some(2));
        assert!(matches!(cli.command, Command::Check(_)));
    }

    #[test]
    fn parses_init_with_repeated_targets() {
        let cli = Cli::try_parse_from([
            "gluon",
            "init",
            "--target",
            "x86_64-unknown-none",
            "-t",
            "aarch64-unknown-none",
            "--bootloader",
            "limine",
            "--dry-run",
        ])
        .expect("parse");
        match cli.command {
            Command::Init(a) => {
                assert_eq!(a.targets, ["x86_64-unknown-none", "aarch64-unknown-none"]);
                assert_eq!(a.bootloader, Some(Bootloader::Limine));
                assert!(a.dry_run);
                assert!(!a.force);
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn global_target_before_init_reaches_init() {
        // `InitArgs::targets` shares the global flag's id, so clap does
        // not propagate a second `--target` into `init`, and a value
        // given before the subcommand still lands in `targets`.
        let cli =
            Cli::try_parse_from(["gluon", "-t", "x86_64-unknown-none", "init"]).expect("parse");
        match cli.command {
            Command::Init(a) => assert_eq!(a.targets, ["x86_64-unknown-none"]),
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn parses_external_subcommand() {
        let cli = Cli::try_parse_from(["gluon", "foo", "bar", "baz"]).expect("parse");
        match cli.command {
            Command::External(args) => {
                let as_str: Vec<_> = args
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(as_str, vec!["foo", "bar", "baz"]);
            }
            other => panic!("expected External, got {other:?}"),
        }
    }

    #[test]
    fn rejects_jobs_zero() {
        // -j 0 is meaningless and would deadlock the scheduler. Rejected
        // at parse time so the user gets a friendly error instead of a
        // confusing scheduler stall.
        let err =
            Cli::try_parse_from(["gluon", "-j", "0", "build"]).expect_err("-j 0 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("at least 1"),
            "error must explain the constraint, got: {msg}"
        );
    }

    #[test]
    fn rejects_jobs_non_numeric() {
        let err = Cli::try_parse_from(["gluon", "-j", "abc", "build"])
            .expect_err("-j abc must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("not a number"), "got: {msg}");
    }

    #[test]
    fn run_rejects_conflicting_boot_modes() {
        // Each flag overrides the profile's boot mode, so two at once has
        // no meaning and must fail at parse time rather than pick one.
        for pair in [
            ["--uefi", "--direct"],
            ["--uefi", "--bios"],
            ["--direct", "--bios"],
        ] {
            let err = Cli::try_parse_from(["gluon", "run", pair[0], pair[1]])
                .expect_err("conflicting boot modes must be rejected");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{pair:?}"
            );
        }
    }

    #[test]
    fn color_defaults_to_auto_and_parses_never() {
        let cli = Cli::try_parse_from(["gluon", "build"]).expect("parse");
        assert_eq!(cli.color, ColorWhen::Auto);
        let cli = Cli::try_parse_from(["gluon", "--color", "never", "build"]).expect("parse");
        assert_eq!(cli.color, ColorWhen::Never);
        assert!(Cli::try_parse_from(["gluon", "--color", "sometimes", "build"]).is_err());
    }
}
