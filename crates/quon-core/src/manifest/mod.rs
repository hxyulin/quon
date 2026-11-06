//! Cargo manifest support: reading `Cargo.toml` workspaces and resolving
//! features the way Cargo's resolver 2 does, so Quon can build crates
//! from their manifests instead of edges duplicated in `quon.rs`.
//!
//! - [`workspace`] loads the workspace, applies inheritance and maps
//!   every dependency to a package (path, or registry via [`lock`] and
//!   a [`sources::RegistrySources`] map).
//! - [`platform`] probes target cfgs and matches `[target.'...']` keys.
//! - [`resolve`] resolves features per domain into a [`UnitGraph`].
//! - [`lower`] expands `Package::new(..)` roots in the raw build
//!   model into crates, before intern.
//!
//! `quon manifest` prints what the loader and resolver produce.

pub mod lock;
pub mod lower;
pub mod platform;
pub mod resolve;
pub mod schema;
pub mod sources;
pub mod workspace;

pub use resolve::{Domain, Unit, UnitGraph, UnitId, UnitKind};

use std::fmt::Write as _;
use std::path::Path;
use workspace::{DepSource, DepTarget, PackageSet, PackageSource};

/// Render the package set as stable, line-oriented text. Paths are
/// relative to the workspace root so the output is the same on every
/// machine; fixtures compare against it.
pub fn dump_packages(set: &PackageSet) -> String {
    let root = &set.root;
    let rel = |p: &Path| -> String {
        p.strip_prefix(root)
            .map(|r| {
                let s = r.display().to_string();
                if s.is_empty() { ".".into() } else { s }
            })
            .unwrap_or_else(|_| p.display().to_string())
    };
    let mut out = String::new();
    for p in &set.packages {
        let dir = p.manifest_dir();
        let local = |q: &Path| {
            q.strip_prefix(dir)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| q.display().to_string())
        };
        let source = match &p.id.source {
            PackageSource::Path(d) => format!("path {}", rel(d)),
            PackageSource::Registry => "registry".into(),
        };
        let member = if p.is_member { " member" } else { "" };
        let _ = writeln!(out, "{} {} ({source}){member}", p.id.name, p.id.version);
        let _ = writeln!(out, "  edition {}", p.edition);
        let m = &p.metadata;
        let meta = [
            (
                "authors",
                (!m.authors.is_empty()).then(|| m.authors.join(":")),
            ),
            ("description", m.description.clone()),
            ("homepage", m.homepage.clone()),
            ("repository", m.repository.clone()),
            ("license", m.license.clone()),
            ("license-file", m.license_file.clone()),
            ("readme", m.readme.clone()),
            ("rust-version", m.rust_version.clone()),
        ];
        for (k, v) in meta {
            if let Some(v) = v {
                let _ = writeln!(out, "  {k} {v}");
            }
        }
        if let Some(lib) = &p.lib {
            let kind = if lib.proc_macro {
                "proc-macro".to_string()
            } else {
                lib.crate_types.join(",")
            };
            let _ = writeln!(
                out,
                "  lib {} {} ({kind}, edition {})",
                lib.name,
                local(&lib.path),
                lib.edition
            );
        }
        for b in &p.bins {
            let req = if b.required_features.is_empty() {
                String::new()
            } else {
                format!(" requires {}", b.required_features.join(","))
            };
            let _ = writeln!(out, "  bin {} {}{req}", b.name, local(&b.path));
        }
        if let Some(bs) = &p.build_script {
            let _ = writeln!(out, "  build-script {}", local(bs));
        }
        if let Some(l) = &p.links {
            let _ = writeln!(out, "  links {l}");
        }
        for (name, values) in &p.features {
            let values: Vec<String> = values.iter().map(|v| v.value.to_string()).collect();
            let _ = writeln!(out, "  feature {name} = [{}]", values.join(", "));
        }
        for d in &p.deps {
            let mut line = format!("  dep {} {}", d.kind.as_str(), d.name);
            if let Some(pl) = &d.platform {
                let _ = write!(line, " [{pl}]");
            }
            match &d.target {
                DepTarget::Resolved(i) => {
                    let t = &set.packages[*i].id;
                    let _ = write!(line, " -> {} {}", t.name, t.version);
                }
                DepTarget::Unresolved(reason) => {
                    let req = match &d.source {
                        DepSource::Registry(r) => format!(" {r}"),
                        DepSource::Path(..) | DepSource::Unsupported(_) => String::new(),
                    };
                    let reason = reason.replace(&format!("{}/", root.display()), "");
                    let _ = write!(line, " -> {}{req} unresolved: {reason}", d.package);
                }
            }
            if d.optional {
                line.push_str(" optional");
            }
            if !d.default_features {
                line.push_str(" no-default-features");
            }
            if !d.features.is_empty() {
                let _ = write!(line, " features={}", d.features.join(","));
            }
            out.push_str(&line);
            out.push('\n');
        }
        for (tool, lints) in &p.lints {
            for (name, l) in lints {
                let _ = writeln!(
                    out,
                    "  lint {tool}::{name} {} priority {}",
                    l.level, l.priority
                );
            }
        }
    }
    out
}

/// The members of the Cargo workspace at `root`, for
/// `CargoWorkspace::members` in `quon.rs`, sorted by name.
///
/// Registry sources are not looked up: the answer names only member
/// packages and their dependencies' package names, and the loader
/// records an unmapped registry edge instead of failing. A stale
/// `vendor/` or a missing `Cargo.lock` therefore cannot stop the script.
pub fn script_members(root: &Path) -> crate::error::Result<Vec<quon_model::WorkspaceMember>> {
    let sources = sources::RegistrySources::new(Default::default(), "");
    let set = workspace::load_workspace(root, &sources)?;
    let members = set.members.iter().map(|&i| {
        let package = &set.packages[i];
        let dir = package.manifest_dir();
        // Members live under the root, which the loader canonicalized,
        // as it did their manifest paths.
        let relative = dir.strip_prefix(&set.root).unwrap_or(dir);
        let mut path = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if path.is_empty() {
            path = ".".into();
        }
        let dependencies = package
            .deps
            .iter()
            .filter(|d| d.kind == workspace::DepKind::Normal)
            .map(|d| d.package.clone())
            .collect();
        quon_model::WorkspaceMember::new(package.name().to_owned(), path, dependencies)
    });
    // `set.members` is already sorted by name, but the order is part
    // of what quon.rs sees, so it should not rest on the loader's.
    let mut members: Vec<_> = members.collect();
    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn package(name: &str, deps: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n{deps}")
    }

    #[test]
    fn script_members_lists_members_by_name_with_relative_paths_and_normal_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            root,
            "Cargo.toml",
            &format!(
                "[workspace]\nmembers = [\"crates/*\"]\n\n\
                 [workspace.dependencies]\nhal = {{ path = \"crates/hal-impl\", package = \"hal-impl\" }}\n\n{}",
                package("app", "")
            ),
        );
        write(root, "src/main.rs", "fn main() {}\n");
        write(root, "crates/hal-impl/Cargo.toml", &package("hal-impl", ""));
        write(root, "crates/hal-impl/src/lib.rs", "");
        write(
            root,
            "crates/zeta/Cargo.toml",
            &package(
                "zeta",
                "[dependencies]\nhal.workspace = true\n\n\
                 [dev-dependencies]\nhelper = { path = \"../helper\" }\n",
            ),
        );
        write(root, "crates/zeta/src/lib.rs", "");
        write(root, "crates/helper/Cargo.toml", &package("helper", ""));
        write(root, "crates/helper/src/lib.rs", "");

        let members = script_members(root).unwrap();
        let summary: Vec<_> = members
            .iter()
            .map(|m| {
                let deps: Vec<_> = m.dependencies.iter().map(String::as_str).collect();
                (m.name.as_str(), m.path.as_str(), deps)
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("app", ".", vec![]),
                ("hal-impl", "crates/hal-impl", vec![]),
                ("helper", "crates/helper", vec![]),
                // The renamed `hal` is listed by its package name, and the
                // dev-dependency is left out.
                ("zeta", "crates/zeta", vec!["hal-impl"]),
            ]
        );
    }
}
