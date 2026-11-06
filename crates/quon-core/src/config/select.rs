//! Picking one `q.run(..)` entry before Cargo expansion.
//!
//! A project with several run entries builds one entry at a time.
//! [`select_run`] rewrites the raw model into the shape of a project
//! with that entry alone: the other entries' `Package` roots are
//! dropped, and the root profiles take the entry's target, binary and
//! runner. Cargo features then resolve over the chosen entry's
//! packages only, as `cargo build -p <entry>` resolves them, so two
//! entries can enable features of one dependency that exclude each
//! other, such as two chip features of a HAL.
//!
//! Only `Package` roots of other entries are dropped. A plain `Crate`
//! takes no part in feature resolution, and the profile's reachability
//! filter already leaves out crates the entry does not reach.
//!
//! With zero or one entry, `quon-config` has already filled the
//! profiles, so selection only checks a requested name.

use crate::error::{Error, Result};
use quon_model::{Arena, RawBuildModel, RawProfileDef, RawRunDef};
use std::collections::BTreeSet;

/// Which run entry a command loads the model for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSelection {
    /// The entry with this name.
    Named(String),
    /// The project's only entry. Fails when it has several.
    Only,
    /// Every entry's package, with no run binary: for commands that
    /// build nothing, such as `quon fmt` and `quon clean`. Features
    /// unify across all entries.
    All,
    /// No entry's package and no run binary: for `quon test` running
    /// the host tests alone.
    None,
}

/// The names of the run entries, in declaration order. A project's
/// single entry may have no name.
pub fn run_entry_names(raw: &RawBuildModel) -> Vec<String> {
    raw.runs.iter().filter_map(|r| r.name.clone()).collect()
}

/// Rewrite `raw` for `selection`. Must run before Cargo expansion,
/// which reads the `Package` roots this drops.
pub fn select_run(raw: &mut RawBuildModel, selection: &RunSelection) -> Result<()> {
    let runs = std::mem::take(&mut raw.runs);
    let chosen = match selection {
        RunSelection::Named(name) => Some(find(&runs, name)?),
        RunSelection::Only if runs.len() > 1 => {
            return Err(Error::Config(format!(
                "quon.rs declares {} run entries ({}); name one on the command line, or \
                 set `RUN = \"<name>\"` in .quon-config",
                runs.len(),
                list(&runs)
            )));
        }
        RunSelection::Only => runs.first(),
        RunSelection::All | RunSelection::None => None,
    };

    match (selection, chosen) {
        // quon-config filled the profiles for a single entry.
        (_, Some(_)) if runs.len() == 1 => {}
        (_, Some(entry)) => {
            drop_packages(raw, &runs, Some(&entry.owner));
            for_root_profiles(raw, |p| {
                p.target = Some(entry.target.clone());
                p.run_binary = Some(entry.binary.clone());
                p.runner = p.runner.take().or_else(|| entry.runner.clone());
            });
        }
        (RunSelection::None, None) => {
            drop_packages(raw, &runs, None);
            fill_fallback_target(raw, &runs);
            for_root_profiles(raw, |p| p.run_binary = None);
        }
        (_, None) => fill_fallback_target(raw, &runs),
    }
    Ok(())
}

fn find<'a>(runs: &'a [RawRunDef], name: &str) -> Result<&'a RawRunDef> {
    runs.iter()
        .find(|r| r.name.as_deref() == Some(name))
        .ok_or_else(|| {
            let known = if runs.iter().any(|r| r.name.is_some()) {
                format!("the entries are {}", list(runs))
            } else {
                "quon.rs names no run entry; name one with q.run(..).name(\"<name>\")".into()
            };
            Error::Config(format!("no run entry named '{name}'; {known}"))
        })
}

fn list(runs: &[RawRunDef]) -> String {
    runs.iter()
        .filter_map(|r| r.name.as_deref())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Drop the `Package` roots that run entries own, except `keep`, the
/// chosen entry's. Two entries may share a package, as two bins of it
/// do; its root then stays.
fn drop_packages(raw: &mut RawBuildModel, runs: &[RawRunDef], keep: Option<&str>) {
    let dropped: BTreeSet<&str> = runs
        .iter()
        .map(|r| r.owner.as_str())
        .filter(|o| Some(*o) != keep)
        .collect();
    let mut crates = Arena::new();
    for (_, c) in raw.crates.iter() {
        if c.manifest.is_some() && dropped.contains(c.name.as_str()) {
            continue;
        }
        crates.insert(c.name.clone(), c.clone());
    }
    raw.crates = crates;
}

/// With several entries and none chosen, the root profiles build for
/// the first entry's target, so a command that only needs some target,
/// such as `quon fmt`, can still resolve a profile.
fn fill_fallback_target(raw: &mut RawBuildModel, runs: &[RawRunDef]) {
    let Some(first) = runs.first() else { return };
    for_root_profiles(raw, |p| {
        p.target.get_or_insert_with(|| first.target.clone());
    });
}

/// Call `f` on each profile that inherits nothing. Those hold the run
/// fields; profiles that inherit take them from these.
fn for_root_profiles(raw: &mut RawBuildModel, mut f: impl FnMut(&mut RawProfileDef)) {
    let roots: Vec<_> = raw
        .profiles
        .iter()
        .filter(|(_, p)| p.inherits.is_none())
        .map(|(h, _)| h)
        .collect();
    for h in roots {
        if let Some(p) = raw.profiles.get_mut(h) {
            f(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_config::prelude::*;

    /// Two boards on one target, each a package of its own, and a
    /// shared library package that is no entry.
    fn boards() -> RawBuildModel {
        let mut q = Quon::new();
        q.project("fleet", "0.1.0");
        q.cargo_workspace(".");
        let m7 = Target::builtin("thumbv7em-none-eabihf");
        let m4 = Target::builtin("thumbv7em-none-eabi");
        let probe = q.add(Command::new(["probe-rs", "run", "{binary}"]));
        let sim = q.add(Command::new(["sim", "{binary}"]));
        q.add(Package::new("shared").target(&m7));
        let gimbal = q.add(Package::new("gimbal").target(&m7));
        let chassis = q.add(Package::new("chassis").target(&m4));
        q.run(&gimbal).name("gimbal").runner(&probe);
        q.run(&chassis).name("chassis");
        let dev = q.profile("dev");
        q.add(Profile::new("sim").inherits(&dev).runner(&sim));
        q.into_raw().expect("quon errors")
    }

    fn crates(raw: &RawBuildModel) -> Vec<&str> {
        raw.crates.iter().map(|(_, c)| c.name.as_str()).collect()
    }

    fn profile<'a>(raw: &'a RawBuildModel, name: &str) -> &'a RawProfileDef {
        raw.profiles
            .get(raw.profiles.lookup(name).unwrap())
            .unwrap()
    }

    fn select(selection: RunSelection) -> RawBuildModel {
        let mut raw = boards();
        select_run(&mut raw, &selection).expect("select");
        raw
    }

    #[test]
    fn a_named_entry_keeps_its_package_and_fills_the_root_profiles() {
        let raw = select(RunSelection::Named("chassis".into()));
        assert_eq!(crates(&raw), ["shared", "chassis"]);
        let dev = profile(&raw, "dev");
        assert_eq!(dev.target.as_deref(), Some("thumbv7em-none-eabi"));
        assert_eq!(dev.run_binary.as_deref(), Some("chassis"));
        assert_eq!(dev.runner, None);
        let release = profile(&raw, "release");
        assert_eq!(release.run_binary, None, "inherits from dev");
        assert!(raw.runs.is_empty());
    }

    #[test]
    fn a_profile_runner_overrides_the_entrys() {
        let raw = select(RunSelection::Named("gimbal".into()));
        assert_eq!(profile(&raw, "dev").runner.as_deref(), Some("command"));
        assert_eq!(profile(&raw, "sim").runner.as_deref(), Some("command-2"));
    }

    #[test]
    fn all_keeps_every_package_without_a_run_binary() {
        let raw = select(RunSelection::All);
        assert_eq!(crates(&raw), ["shared", "gimbal", "chassis"]);
        let dev = profile(&raw, "dev");
        assert_eq!(dev.target.as_deref(), Some("thumbv7em-none-eabihf"));
        assert_eq!(dev.run_binary, None);
    }

    #[test]
    fn none_drops_every_entrys_package() {
        let raw = select(RunSelection::None);
        assert_eq!(crates(&raw), ["shared"]);
        assert_eq!(profile(&raw, "dev").run_binary, None);
    }

    #[test]
    fn only_and_unknown_names_list_the_entries() {
        let mut raw = boards();
        let err = select_run(&mut raw, &RunSelection::Only).unwrap_err();
        assert!(
            err.to_string()
                .contains("quon.rs declares 2 run entries (gimbal, chassis)"),
            "{err}"
        );
        let mut raw = boards();
        let err = select_run(&mut raw, &RunSelection::Named("hero".into())).unwrap_err();
        assert!(
            err.to_string()
                .contains("no run entry named 'hero'; the entries are gimbal, chassis"),
            "{err}"
        );
    }

    #[test]
    fn a_single_entry_is_left_as_quon_config_lowered_it() {
        let mut q = Quon::new();
        q.project("k", "0.1.0");
        q.cargo_workspace(".");
        let kernel = q.add(Package::new("kernel"));
        q.run(&kernel);
        let before = q.into_raw().unwrap();
        for selection in [RunSelection::Only, RunSelection::All] {
            let mut raw = before.clone();
            select_run(&mut raw, &selection).unwrap();
            assert_eq!(crates(&raw), ["kernel"]);
            assert_eq!(profile(&raw, "dev").run_binary.as_deref(), Some("kernel"));
        }
        let mut raw = before.clone();
        let err = select_run(&mut raw, &RunSelection::Named("kernel".into())).unwrap_err();
        assert!(
            err.to_string().contains("quon.rs names no run entry"),
            "{err}"
        );
    }

    #[test]
    fn two_entries_of_one_package_keep_it() {
        let mut q = Quon::new();
        q.project("t", "0.1.0");
        q.cargo_workspace(".");
        let tools = q.add(Package::new("tools"));
        q.run(&tools.bin("a")).name("a");
        q.run(&tools.bin("b")).name("b");
        let mut raw = q.into_raw().unwrap();
        select_run(&mut raw, &RunSelection::Named("b".into())).unwrap();
        assert_eq!(crates(&raw), ["tools"]);
        assert_eq!(profile(&raw, "dev").run_binary.as_deref(), Some("b"));
    }
}
