//! Crates that a multi-entry build compiles more than once, and why.
//!
//! Each run entry resolves Cargo features on its own, as `cargo build
//! -p` does, so a crate that two entries reach with different features,
//! or through dependencies with different features, gets one artifact
//! per variant. Nothing in `quon.rs` shows that: a one-feature
//! difference deep in one board's tree forks every crate above it.
//!
//! [`find`] groups the entries' units by package, version, crate and
//! target, and keeps the groups with more than one identity
//! ([`Metadata`]). For each extra copy it names the cause against the
//! copy most entries use: the crate's own features if they differ,
//! otherwise the deepest dependencies whose copies differ, until one
//! whose features differ or that only one side has. Flags are not part
//! of a unit's identity, so they never make a copy.
//!
//! Build scripts are left out: their copies follow their package's.

use crate::unit::{BuildUnit, DepTarget, Metadata, Platform, UnitGraph, UnitId};
use quon_model::BuildModel;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// One run entry's units, as the build lowered them.
pub struct EntryUnits<'a> {
    pub name: &'a str,
    pub model: &'a BuildModel,
    pub units: &'a UnitGraph,
}

/// A crate compiled more than once for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyGroup {
    pub package: String,
    pub version: String,
    /// The target name, or `host`.
    pub platform: String,
    /// The most-used copy first, then by entry count and first entry.
    pub copies: Vec<Copy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// The run entries that use this copy, sorted.
    pub entries: Vec<String>,
    /// Why this copy differs from the first. Empty for the first.
    pub causes: Vec<Cause>,
}

/// Why a copy differs from the most-used one, at the crate where the
/// difference starts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cause {
    /// `package` has different features in this copy.
    Features {
        package: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// Only this copy depends on `package`, or only the first does.
    Dependency { package: String, added: bool },
    /// The copies differ in something else that names an artifact,
    /// such as the source or the config crate.
    Other { package: String },
}

/// Where one unit lives: the entry and its id there.
#[derive(Clone, Copy)]
struct At {
    entry: usize,
    unit: UnitId,
}

/// Group key: what makes two units the same crate on one target.
type Key = (String, String, String, String, String);

/// Every crate with more than one copy across `entries`, sorted by
/// package, version, crate and target. See the module docs.
pub fn find(entries: &[EntryUnits<'_>]) -> Vec<CopyGroup> {
    // Key -> identity -> (entries using it, one place it lives).
    let mut groups: BTreeMap<Key, BTreeMap<Metadata, (BTreeSet<String>, At)>> = BTreeMap::new();
    for (e, entry) in entries.iter().enumerate() {
        for (id, unit) in entry.units.iter() {
            if unit.is_build_script {
                continue;
            }
            let key = (
                unit.package.name.clone(),
                unit.package.version.clone(),
                unit.crate_name.clone(),
                format!("{:?}", unit.kind),
                platform_name(entry.model, unit),
            );
            let copy = groups
                .entry(key)
                .or_default()
                .entry(unit.metadata)
                .or_insert_with(|| (BTreeSet::new(), At { entry: e, unit: id }));
            copy.0.insert(entry.name.to_owned());
        }
    }

    let mut memo = BTreeMap::new();
    let mut out = Vec::new();
    for ((package, version, _, _, platform), copies) in groups {
        if copies.len() < 2 {
            continue;
        }
        let mut copies: Vec<(BTreeSet<String>, At)> = copies.into_values().collect();
        copies.sort_by(|a, b| {
            b.0.len()
                .cmp(&a.0.len())
                .then_with(|| a.0.iter().next().cmp(&b.0.iter().next()))
        });
        let base = copies[0].1;
        let copies = copies
            .iter()
            .enumerate()
            .map(|(i, (names, at))| Copy {
                entries: names.iter().cloned().collect(),
                causes: if i == 0 {
                    Vec::new()
                } else {
                    causes(entries, *at, base, &mut memo).into_iter().collect()
                },
            })
            .collect();
        out.push(CopyGroup {
            package,
            version,
            platform,
            copies,
        });
    }
    out
}

fn unit<'a>(entries: &'a [EntryUnits<'_>], at: At) -> &'a BuildUnit {
    entries[at.entry].units.get(at.unit)
}

fn platform_name(model: &BuildModel, unit: &BuildUnit) -> String {
    match unit.platform {
        Platform::Host => "host".into(),
        Platform::Cross(t) => model
            .targets
            .get(t)
            .map_or_else(|| "?".into(), |t| t.name.clone()),
    }
}

/// Why `copy` differs from `base`, two units of one crate. Memoized by
/// the pair of identities, since a shared dependency is reached through
/// many paths.
fn causes(
    entries: &[EntryUnits<'_>],
    copy: At,
    base: At,
    memo: &mut BTreeMap<(Metadata, Metadata), BTreeSet<Cause>>,
) -> BTreeSet<Cause> {
    let (c, b) = (unit(entries, copy), unit(entries, base));
    let pair = (c.metadata, b.metadata);
    if let Some(found) = memo.get(&pair) {
        return found.clone();
    }
    let mut found = BTreeSet::new();
    if c.features != b.features {
        let (cf, bf): (BTreeSet<_>, BTreeSet<_>) =
            (c.features.iter().collect(), b.features.iter().collect());
        found.insert(Cause::Features {
            package: c.package.name.clone(),
            added: cf.difference(&bf).map(|f| (*f).clone()).collect(),
            removed: bf.difference(&cf).map(|f| (*f).clone()).collect(),
        });
    } else {
        // Same features, so the difference is below: compare the
        // dependencies by package, where an extern name could be
        // renamed per copy.
        let deps = |at: At| -> BTreeMap<String, At> {
            unit(entries, at)
                .deps
                .iter()
                .filter_map(|d| match d.target {
                    DepTarget::Unit(id) => {
                        let to = At {
                            entry: at.entry,
                            unit: id,
                        };
                        Some((unit(entries, to).package.name.clone(), to))
                    }
                    DepTarget::Prebuilt { .. } | DepTarget::Missing { .. } => None,
                })
                .collect()
        };
        let (cd, bd) = (deps(copy), deps(base));
        for (name, &to) in &cd {
            match bd.get(name) {
                Some(&from) if unit(entries, to).metadata != unit(entries, from).metadata => {
                    found.extend(causes(entries, to, from, memo));
                }
                Some(_) => {}
                None => {
                    found.insert(Cause::Dependency {
                        package: name.clone(),
                        added: true,
                    });
                }
            }
        }
        for name in bd.keys().filter(|n| !cd.contains_key(*n)) {
            found.insert(Cause::Dependency {
                package: name.clone(),
                added: false,
            });
        }
        if found.is_empty() {
            found.insert(Cause::Other {
                package: c.package.name.clone(),
            });
        }
    }
    memo.insert(pair, found.clone());
    found
}

/// The groups as text, one block per crate, for `quon explain copies`.
#[must_use]
pub fn render(groups: &[CopyGroup]) -> String {
    let mut out = String::new();
    for g in groups {
        let _ = writeln!(
            out,
            "{} {} ({}): {} copies",
            g.package,
            g.version,
            g.platform,
            g.copies.len()
        );
        for copy in &g.copies {
            let n = copy.entries.len();
            let noun = if n == 1 { "entry" } else { "entries" };
            let _ = writeln!(out, "  {n} {noun}: {}", copy.entries.join(", "));
            for cause in &copy.causes {
                let _ = writeln!(out, "    {}", describe(cause));
            }
        }
    }
    out
}

fn describe(cause: &Cause) -> String {
    match cause {
        Cause::Features {
            package,
            added,
            removed,
        } => {
            let changes: Vec<String> = added
                .iter()
                .map(|f| format!("+{f}"))
                .chain(removed.iter().map(|f| format!("-{f}")))
                .collect();
            format!("{package}: features {}", changes.join(", "))
        }
        Cause::Dependency {
            package,
            added: true,
        } => format!("{package}: a dependency only this copy has"),
        Cause::Dependency {
            package,
            added: false,
        } => format!("{package}: a dependency only the first copy has"),
        Cause::Other { package } => {
            format!("{package}: differs in source or config crate, not in features")
        }
    }
}
