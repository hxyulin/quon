//! `gluon manifest` — print what Gluon reads from a Cargo workspace.

use crate::cli::{ManifestArgs, ManifestCommand};
use anyhow::{Context, Result};
use gluon_core::manifest::sources::RegistrySources;
use gluon_core::manifest::{dump_packages, workspace::load_workspace};
use std::path::PathBuf;

pub fn run(cmd: &ManifestCommand) -> Result<()> {
    match cmd {
        ManifestCommand::Packages(args) => {
            let (root, vendor) = locate(args)?;
            let sources = RegistrySources::vendor_dir(&root, &vendor)?;
            let set = load_workspace(&root, &sources)?;
            for w in &set.warnings {
                eprintln!("{w}");
            }
            print!("{}", dump_packages(&set));
            Ok(())
        }
    }
}

fn locate(args: &ManifestArgs) -> Result<(PathBuf, PathBuf)> {
    let root = match &args.workspace {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let vendor = args
        .vendor_dir
        .clone()
        .unwrap_or_else(|| root.join("vendor"));
    Ok((root, vendor))
}
