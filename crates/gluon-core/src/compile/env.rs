//! Resolve a crate's `artifact_env` declarations into a concrete
//! `BTreeMap<String, String>` of environment variables.
//!
//! This is the single source of truth for the env vars gluon hands a
//! crate at compile time. Two consumers call it:
//!
//! 1. `compile::command_builder` — at rustc-spawn time, with a closure
//!    that reads from the just-built [`crate::compile::ArtifactMap`].
//!    The path each env var resolves to is the actual on-disk artifact
//!    of an upstream crate that the scheduler has already produced.
//!
//! 2. `crate::analyzer` — at `rust-project.json` generation time, with
//!    a closure that derives the deterministic future path from
//!    [`crate::compile::artifact_path::expected_crate_artifact_path`].
//!    No build has happened yet, so we are predicting the path; the
//!    layout makes this safe.
//!
//! By centralising the resolution here we guarantee both call sites
//! produce *exactly the same* env-var name/value pairs in the same
//! BTreeMap iteration order. That symmetry is what lets `env!()` in the
//! consuming source resolve identically under rustc and under
//! rust-analyzer — which is the whole point.

use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateDef, Handle};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Resolve `crate_def.artifact_env` into `(env_var_name → absolute_path)` pairs.
///
/// `lookup_artifact` maps a referenced-crate handle to the on-disk path
/// of that crate's primary artifact. For build-time callers this is
/// `|h| artifacts.get(h).map(Path::to_path_buf)`; for analyzer-time
/// callers it is the layout-derived expected path.
///
/// Returns the env vars in `BTreeMap` order — the same order
/// `crate_def.artifact_env` itself iterates in. Cache-key stability and
/// cross-call symmetry both depend on this ordering being deterministic.
///
/// Each path is canonicalised (symlinks resolved) when possible. When
/// canonicalisation fails — which is normal at analyzer time, since the
/// artifact may not have been written yet — the unresolved absolute
/// path is used as-is. This matches the previous inline behaviour in
/// `command_builder`.
pub fn resolve_crate_env<F>(
    crate_def: &CrateDef,
    model: &BuildModel,
    mut lookup_artifact: F,
) -> Result<BTreeMap<String, String>>
where
    F: FnMut(Handle<CrateDef>) -> Option<PathBuf>,
{
    let mut out = BTreeMap::new();
    for (env_key, dep_name) in &crate_def.artifact_env {
        let dep_handle = model.crates.lookup(dep_name).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' has artifact_env entry '{}={}', but no crate named '{}' \
                 exists in the build model (validate pass should have caught this)",
                crate_def.name, env_key, dep_name, dep_name,
            ))
        })?;
        let artifact_path = lookup_artifact(dep_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' artifact_env references '{}' but its artifact path \
                 could not be resolved (build-time: scheduler did not run the \
                 producer first; analyzer-time: producer is not in the resolved \
                 crate set for the active profile)",
                crate_def.name, dep_name,
            ))
        })?;
        let abs_path = std::fs::canonicalize(&artifact_path).unwrap_or(artifact_path);
        out.insert(env_key.clone(), abs_path.to_string_lossy().into_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{BuildModel, CrateDef, CrateType, GroupDef, Handle, TargetSpec};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Ensure a "g" group exists in `model`, returning its handle.
    fn ensure_group(model: &mut BuildModel) -> Handle<GroupDef> {
        if let Some(h) = model.groups.lookup("g") {
            return h;
        }
        let (h, _) = model.groups.insert(
            "g".into(),
            GroupDef {
                name: "g".into(),
                target: TargetSpec::Host,
                ..Default::default()
            },
        );
        h
    }

    /// Build the newly-mandatory `CrateDef` fields with safe defaults.
    /// Tests use `..test_crate_defaults(group)` instead of
    /// `..Default::default()`.
    fn test_crate_defaults(group: Handle<GroupDef>) -> CrateDef {
        CrateDef {
            name: String::new(),
            path: String::new(),
            edition: "2024".into(),
            crate_type: CrateType::Lib,
            target: TargetSpec::Host,
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            features: Vec::new(),
            root: None,
            linker_script: None,
            group,
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
            cargo: None,
        }
    }

    fn crate_def_with_env(group: Handle<GroupDef>, env: &[(&str, &str)]) -> CrateDef {
        let mut c = CrateDef {
            name: "consumer".into(),
            path: "consumer".into(),
            edition: "2021".into(),
            crate_type: CrateType::Lib,
            ..test_crate_defaults(group)
        };
        for (k, v) in env {
            c.artifact_env.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn resolves_in_btree_order() {
        let mut model = BuildModel::default();
        let g = ensure_group(&mut model);
        let (h_a, _) = model.crates.insert(
            "alpha".into(),
            CrateDef {
                name: "alpha".into(),
                path: "alpha".into(),
                ..test_crate_defaults(g)
            },
        );
        let (h_b, _) = model.crates.insert(
            "bravo".into(),
            CrateDef {
                name: "bravo".into(),
                path: "bravo".into(),
                ..test_crate_defaults(g)
            },
        );
        // Insert env entries in non-alphabetic order; resolution must
        // still emit them in BTreeMap (alphabetic by key) order.
        let consumer = crate_def_with_env(g, &[("Z_VAR", "alpha"), ("A_VAR", "bravo")]);

        let resolved = resolve_crate_env(&consumer, &model, |h| {
            if h == h_a {
                Some(PathBuf::from("/non/existent/alpha.rlib"))
            } else if h == h_b {
                Some(PathBuf::from("/non/existent/bravo.rlib"))
            } else {
                None
            }
        })
        .unwrap();

        let keys: Vec<&str> = resolved.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["A_VAR", "Z_VAR"],
            "iteration order must be BTreeMap-stable"
        );
        assert_eq!(resolved.get("A_VAR").unwrap(), "/non/existent/bravo.rlib");
        assert_eq!(resolved.get("Z_VAR").unwrap(), "/non/existent/alpha.rlib");
    }

    #[test]
    fn unknown_crate_name_errors() {
        let mut model = BuildModel::default();
        let g = ensure_group(&mut model);
        let consumer = crate_def_with_env(g, &[("KEY", "ghost")]);
        let err = resolve_crate_env(&consumer, &model, |_| None);
        assert!(
            err.is_err(),
            "missing crate name must error, not silently drop"
        );
    }

    #[test]
    fn lookup_returning_none_errors() {
        let mut model = BuildModel::default();
        let g = ensure_group(&mut model);
        let (h, _) = model.crates.insert(
            "produced".into(),
            CrateDef {
                name: "produced".into(),
                path: "produced".into(),
                ..test_crate_defaults(g)
            },
        );
        let consumer = crate_def_with_env(g, &[("KEY", "produced")]);
        let err = resolve_crate_env(&consumer, &model, |_| None);
        assert!(
            err.is_err(),
            "lookup returning None for a known crate must error (scheduler bug at build time, \
             missing-from-resolved at analyzer time)"
        );
        // Touch h to silence unused-binding warning.
        let _ = h;
    }

    #[test]
    fn canonicalize_falls_back_to_input_path_when_path_does_not_exist() {
        let mut model = BuildModel::default();
        let g = ensure_group(&mut model);
        let (h, _) = model.crates.insert(
            "produced".into(),
            CrateDef {
                name: "produced".into(),
                path: "produced".into(),
                ..test_crate_defaults(g)
            },
        );
        let consumer = crate_def_with_env(g, &[("KEY", "produced")]);
        let phantom = PathBuf::from("/this/does/not/exist/anywhere.rlib");
        let resolved = resolve_crate_env(&consumer, &model, |_| Some(phantom.clone())).unwrap();
        assert_eq!(
            resolved.get("KEY").unwrap(),
            phantom.to_string_lossy().as_ref(),
            "missing-on-disk paths must be passed through verbatim"
        );
        let _ = h;
    }
}
