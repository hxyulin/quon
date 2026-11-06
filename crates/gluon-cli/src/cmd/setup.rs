//! `gluon setup`: build the shared gluon-config library ahead of time.

use anyhow::{Context, Result};
use gluon_core::RustcInfo;

/// Build the library for the rustc that `$RUSTC`, or `rustc` in the
/// current directory, resolves to, and print the library's directory
/// on stdout. Under rustup that is the toolchain a project in this
/// directory uses.
pub fn run() -> Result<()> {
    let rustc = RustcInfo::probe().context("failed to probe rustc")?;
    let lib = gluon_core::script_runner::ensure_config_lib(&rustc)
        .context("failed to build the gluon-config library")?;
    println!("{}", lib.dir.display());
    Ok(())
}
