//! External subcommand dispatch — quon's plugin model.
//!
//! When clap encounters an unknown subcommand (the `external_subcommand`
//! arm in `cli.rs`), we resolve it to `quon-<name>` on `$PATH` and
//! forward any remaining arguments. This lets a binary like `quon-foo`
//! be dropped onto `$PATH` and invoked as `quon foo` without any core
//! changes — mirroring git's and cargo's extension conventions.

use anyhow::{Result, anyhow};
use std::ffi::OsString;
use std::process::Command;

/// Dispatch an unknown subcommand to `quon-<name>` on `$PATH`.
///
/// `args` is the raw vector clap produced for the `external_subcommand`
/// arm: the first element is the subcommand name, the rest are
/// positional arguments to forward.
pub fn run(args: Vec<OsString>) -> Result<()> {
    let mut iter = args.into_iter();
    let subname = iter
        .next()
        .ok_or_else(|| anyhow!("external subcommand invoked with no name"))?;
    let mut bin_name = OsString::from("quon-");
    bin_name.push(&subname);

    let status = Command::new(&bin_name)
        .args(iter)
        .status()
        .map_err(|e| anyhow!("failed to spawn `{}`: {e}", bin_name.to_string_lossy()))?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
