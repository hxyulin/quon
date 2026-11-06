//! External subcommand dispatch — quon's plugin model.
//!
//! When clap encounters an unknown subcommand (the `external_subcommand`
//! arm in `cli.rs`), we resolve it to `quon-<name>` on `$PATH` and
//! forward any remaining arguments. This lets a binary like `quon-foo`
//! be dropped onto `$PATH` and invoked as `quon foo` without any core
//! changes — mirroring git's and cargo's extension conventions.

use anyhow::{Result, anyhow};
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

/// Dispatch an unknown subcommand to `quon-<name>` on `$PATH`.
///
/// `args` is the raw vector clap produced for the `external_subcommand`
/// arm: the first element is the subcommand name, the rest are
/// positional arguments to forward.
pub fn run(args: Vec<OsString>, cwd: &Path) -> Result<()> {
    let mut iter = args.into_iter();
    let subname = iter
        .next()
        .ok_or_else(|| anyhow!("external subcommand invoked with no name"))?;
    let mut bin_name = OsString::from("quon-");
    bin_name.push(&subname);

    let status = Command::new(&bin_name)
        .args(iter)
        .current_dir(cwd)
        .status()
        .map_err(|e| anyhow!("failed to spawn `{}`: {e}", bin_name.to_string_lossy()))?;

    if !status.success() {
        // The plugin printed its own error; pass its status on.
        let code = status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .unwrap_or(1);
        return Err(crate::Exit(code).into());
    }
    Ok(())
}
