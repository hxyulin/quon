//! Cargo manifest support: reading `Cargo.toml` workspaces and resolving
//! features the way Cargo's resolver 2 does, so Gluon can build crates
//! from their manifests instead of edges duplicated in `gluon.rs`.
//!
//! - [`workspace`] loads the workspace, applies inheritance and maps
//!   every dependency to a package (path, or registry via [`lock`] and
//!   a [`sources::RegistrySources`] map).
//! - [`platform`] probes target cfgs and matches `[target.'...']` keys.
//! - [`resolve`] resolves features per domain into a [`UnitGraph`].
//!
//! Builds do not use this module yet; `gluon manifest` prints what it
//! reads.

pub mod lock;
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
                        DepSource::Path(..) => String::new(),
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
