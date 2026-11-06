//! `gluon manifest`: print what Gluon reads from a Cargo workspace and
//! how it resolves features. Builds resolve the workspace named by
//! `cfg.cargo_workspace` the same way, for the roots in `gluon.rs`.

use crate::cli::{ManifestArgs, ManifestCommand, ManifestUnitsArgs};
use anyhow::{Context, Result, bail};
use gluon_core::RustcInfo;
use gluon_core::manifest::dump_packages;
use gluon_core::manifest::platform::{self, ProbeTarget, TargetPlatform};
use gluon_core::manifest::resolve::{Domain, Root, dump_units, resolve};
use gluon_core::manifest::sources::RegistrySources;
use gluon_core::manifest::workspace::{PackageSet, load_workspace};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn run(cmd: &ManifestCommand, target: Option<&str>) -> Result<()> {
    match cmd {
        ManifestCommand::Packages(args) => {
            let set = load(args)?;
            print!("{}", dump_packages(&set));
            Ok(())
        }
        ManifestCommand::Units(args) => units(args, target),
    }
}

fn load(args: &ManifestArgs) -> Result<PackageSet> {
    let root = match &args.workspace {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    // Builds read Cargo's registry cache unless gluon.rs names a vendor
    // directory; this command does the same unless `--vendor-dir` does.
    let sources = match &args.vendor_dir {
        Some(dir) => RegistrySources::vendor_dir(&root, dir)?,
        None => RegistrySources::registry_cache(&root, None)?,
    };
    let set = load_workspace(&root, &sources)?;
    for w in &set.warnings {
        eprintln!("{w}");
    }
    Ok(set)
}

fn units(args: &ManifestUnitsArgs, target: Option<&str>) -> Result<()> {
    let set = load(&args.workspace)?;
    let info = RustcInfo::probe().context("failed to probe rustc")?;
    let host = platform::probe(&info, &ProbeTarget::Host, None)?;
    let mut platforms: BTreeMap<Domain, TargetPlatform> = BTreeMap::new();
    let domain = match target {
        None => Domain::Host,
        Some(t) => {
            // A `.json` argument is a custom spec path, as with rustc.
            let probe = if t.ends_with(".json") {
                ProbeTarget::Spec(std::fs::canonicalize(PathBuf::from(t))?)
            } else {
                ProbeTarget::Builtin(t.to_string())
            };
            let p = platform::probe(&info, &probe, None)?;
            let d = Domain::Target(p.name.clone());
            platforms.insert(d.clone(), p);
            d
        }
    };
    platforms.insert(Domain::Host, host);

    let names: Vec<String> = if args.roots.is_empty() {
        set.members
            .iter()
            .filter(|&&i| !set.packages[i].is_proc_macro() || domain == Domain::Host)
            .map(|&i| set.packages[i].name().to_string())
            .collect()
    } else {
        args.roots.clone()
    };
    let mut roots = Vec::new();
    for name in &names {
        let Some(package) = set.member(name) else {
            bail!(
                "`{name}` is not a member of the workspace at {}",
                set.root.display()
            );
        };
        roots.push(Root {
            package,
            domain: domain.clone(),
            features: args.features.clone(),
            default_features: !args.no_default_features,
        });
    }
    let graph = resolve(&set, &roots, &[], &platforms, &info.host_triple)?;
    print!("{}", dump_units(&graph));
    Ok(())
}
