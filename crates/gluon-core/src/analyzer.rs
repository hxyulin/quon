//! `rust-project.json` generation for rust-analyzer.
//!
//! [`generate_rust_project_json`] is a pure function: it consumes a
//! [`BuildModel`] + [`ResolvedConfig`] + [`BuildLayout`] + [`RustcInfo`]
//! and produces a [`serde_json::Value`] in rust-analyzer's
//! `rust-project.json` format. The caller (typically
//! [`crate::configure`]) is responsible for serialising and writing it
//! to disk.
//!
//! ### Why pure?
//!
//! Keeping the generator I/O-free makes it trivially unit-testable
//! against hand-built fixtures and lets non-CLI embedders (LSPs, editor
//! plugins) reuse the same JSON shape without paying the cost of
//! routing through the filesystem.
//!
//! ### Schema reference
//!
//! See <https://rust-analyzer.github.io/manual.html#non-cargo-based-projects>.
//! The fields populated here are the minimum subset rust-analyzer needs
//! to index a non-cargo project: `sysroot_src` (optional) and `crates`
//! (each with `root_module`, `edition`, `deps`, `cfg`, `target`
//! (cross crates only), `env`, `is_workspace_member`, `is_proc_macro`,
//! `source`).

use crate::compile::artifact_path::expected_crate_artifact_path;
use crate::compile::env::resolve_crate_env;
use crate::compile::{BuildLayout, RustcInfo};
use gluon_model::{BuildModel, CrateDef, CrateType, DepResolution, Handle, ResolvedConfig};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Generate a `rust-project.json` value for the given build.
///
/// The output always includes one entry per [`gluon_model::ResolvedCrateRef`]
/// in `resolved.crates` (host and cross alike) **plus** an entry for the
/// generated `<project>_config` crate. The config crate is emitted first
/// so its index in the `crates` array is fixed at `0` — downstream cross
/// crates that link against it can reference it by that stable index.
///
/// `sysroot_src` is omitted entirely (rather than set to `null`) when
/// [`RustcInfo::rust_src`] is `None`. rust-analyzer treats absence and
/// `null` differently: absence triggers its built-in fallback, while
/// `null` is a hard error in some versions.
pub fn generate_rust_project_json(
    model: &BuildModel,
    resolved: &ResolvedConfig,
    layout: &BuildLayout,
    rustc_info: &RustcInfo,
) -> Value {
    // Build the crate list in two passes: first assign indices so we
    // know each crate's position before populating its `deps` array,
    // then walk again to emit the JSON entries with resolved deps.
    //
    // Index 0 is reserved for the generated config crate. The first
    // entry in `resolved.crates` therefore lands at index 1.

    const CONFIG_CRATE_INDEX: usize = 0;

    // Pass 1: build the handle → index map.
    //
    // We dedupe by handle (not by name) because host and cross crates
    // may legitimately share a name but must remain distinct rust-
    // analyzer entries.
    let mut handle_to_index: BTreeMap<Handle<CrateDef>, usize> = BTreeMap::new();
    for (i, cref) in resolved.crates.iter().enumerate() {
        // The first .insert call wins. Subsequent ResolvedCrateRefs
        // with the same handle (which would be a resolver bug) are
        // silently coalesced rather than producing duplicate JSON
        // entries.
        handle_to_index.entry(cref.handle).or_insert(i + 1);
    }

    // Emit the config crate at index 0.
    let config_crate_dir = layout.generated_config_crate_dir();
    let config_crate_entry = json!({
        "root_module": config_crate_dir.join("src").join("lib.rs").to_string_lossy(),
        "edition": "2021",
        "deps": [],
        "cfg": [],
        "env": {},
        "is_workspace_member": true,
        "is_proc_macro": false,
        "source": {
            "include_dirs": [config_crate_dir.to_string_lossy()],
            "exclude_dirs": [layout.root().to_string_lossy()],
        },
    });

    let mut crates_array: Vec<Value> = Vec::with_capacity(resolved.crates.len() + 1);
    crates_array.push(config_crate_entry);

    // Pass 2: emit each ResolvedCrateRef as a JSON entry.
    for cref in &resolved.crates {
        let krate: &CrateDef = match model.crates.get(cref.handle) {
            Some(k) => k,
            // Defensive: a ResolvedCrateRef with no matching CrateDef
            // is a resolver bug, not an analyzer bug. Skip it rather
            // than panicking — the user's rust-analyzer view will be
            // partially broken but other crates will still be indexed.
            None => continue,
        };

        let crate_dir = resolved.project_root.join(&krate.path);
        let root_module = if let Some(root) = &krate.root {
            crate_dir.join(root)
        } else if krate.crate_type.is_executable() {
            crate_dir.join("src").join("main.rs")
        } else {
            crate_dir.join("src").join("lib.rs")
        };

        let edition = if krate.edition.is_empty() {
            "2021"
        } else {
            krate.edition.as_str()
        };

        // --- deps ---
        //
        // The map key in `CrateDef::deps` is the *extern* name (i.e.
        // the identifier the consuming crate uses in `extern crate
        // foo;` or `use foo::...`), which may differ from the target
        // crate's own name. We propagate the key, not the target's
        // CrateDef.name.
        //
        // Cross crates also depend on the generated config crate (the
        // DAG mirrors this — see scheduler::dag::build_dag rule 2).
        // Host crates do not.
        let mut deps: Vec<Value> = Vec::new();
        if !cref.host {
            deps.push(json!({
                "name": "config",
                "crate": CONFIG_CRATE_INDEX,
            }));
        }
        for (extern_name, dep) in &krate.deps {
            // Only project-resolved deps participate in the
            // rust-project.json crate graph — external (vendored) deps
            // surface to rust-analyzer through a different path and are
            // out of scope for MVP-M.
            let DepResolution::Project(target_handle) = dep.resolution else {
                continue;
            };
            let Some(&idx) = handle_to_index.get(&target_handle) else {
                // Resolved handle but the dep is not part of the
                // current build (e.g. vendored crate not in
                // resolved.crates). Skip it — see plan §6 landmines.
                continue;
            };
            deps.push(json!({
                "name": extern_name,
                "crate": idx,
            }));
        }

        // --- cfg ---
        //
        // Emit only the crate's explicit `cfg_flags`. Target-derived cfgs
        // (target_arch, target_os, target_feature, …) are intentionally
        // NOT merged here: we set the per-crate `target` field below, and
        // rust-analyzer then runs `rustc --print=cfg --target=<triple>`
        // itself to populate those. Duplicating them here would risk
        // diverging from rust-analyzer's own derivation on toolchain
        // updates and makes the JSON noisier for no benefit.
        let cfg: Vec<String> = krate.cfg_flags.clone();

        // --- target ---
        //
        // For cross crates we emit the triple (or custom-target JSON path)
        // so rust-analyzer evaluates `#[cfg(target_os = "none")]` and the
        // rest against the kernel's real target rather than the host. Host
        // crates omit the field; rust-analyzer then falls back to the host
        // triple, which is correct for them.
        //
        // `spec` is used rather than `name` because it is what rustc
        // accepts as `--target`: for builtin triples the two are equal,
        // but for custom targets `spec` is the JSON path and `name` is a
        // human label. The compile pipeline uses `spec` for the same
        // reason (see compile/command_builder.rs).
        let target_spec: Option<String> = if cref.host {
            None
        } else {
            model.targets.get(cref.target).map(|t| t.spec.clone())
        };

        // --- env ---
        //
        // Surface the same `artifact_env` mapping that rustc would see at
        // build time (`compile::command_builder` calls
        // `resolve_crate_env` against the ArtifactMap). At analyzer time
        // there is no ArtifactMap yet, so we derive the deterministic
        // future path of each upstream artifact from the build layout —
        // identical to where rustc will eventually write it. This makes
        // `env!("FOO")` resolve symmetrically under rust-analyzer and
        // under rustc.
        //
        // If resolution fails (a missing crate name, an upstream not in
        // the resolved set, etc.), fall back to an empty env map for
        // this crate rather than failing the whole `rust-project.json`
        // emit. The user will see other crates indexed correctly and
        // can fix the offending declaration; failing the whole file
        // would leave the IDE blind.
        let env_map: BTreeMap<String, String> = resolve_crate_env(krate, model, |h| {
            expected_crate_artifact_path(layout, model, resolved, h).ok()
        })
        .unwrap_or_default();
        let env_json: serde_json::Map<String, Value> = env_map
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();

        let mut entry = json!({
            "root_module": root_module.to_string_lossy(),
            "edition": edition,
            "deps": deps,
            "cfg": cfg,
            "env": Value::Object(env_json),
            "is_workspace_member": true,
            "is_proc_macro": krate.crate_type == CrateType::ProcMacro,
            "source": {
                "include_dirs": [crate_dir.to_string_lossy()],
                "exclude_dirs": [layout.root().to_string_lossy()],
            },
        });
        if let Some(spec) = target_spec {
            entry
                .as_object_mut()
                .expect("json! macro produces an object")
                .insert("target".to_string(), Value::String(spec));
        }
        crates_array.push(entry);
    }

    let mut top = serde_json::Map::new();
    if let Some(rust_src) = &rustc_info.rust_src {
        // rust-analyzer expects the `library/` subdirectory that actually
        // contains `core`, `alloc`, `std`. `RustcInfo::rust_src` stores the
        // parent (`<sysroot>/lib/rustlib/src/rust`) because the sysroot
        // builder walks into `library/<crate>/` per crate and needs the
        // parent path. We append `library` only here, on emission.
        top.insert(
            "sysroot_src".to_string(),
            Value::String(rust_src.join("library").to_string_lossy().into_owned()),
        );
    }
    top.insert("crates".to_string(), Value::Array(crates_array));
    Value::Object(top)
}

/// Like [`generate_rust_project_json`] but also emits a crate entry for
/// `gluon.rs` (the user's build script) when one is present, so
/// rust-analyzer indexes the build script alongside the kernel sources.
///
/// `gluon_rs_path` is the absolute path to the user's `gluon.rs`.
/// `gluon_config_lib_rs` is the path to the bundled `gluon-config` crate's
/// `lib.rs` source file so rust-analyzer can index it as a normal workspace
/// dep (rather than pointing at a `.rlib`, which the schema does not
/// support). Both paths come from the gluon workspace resolved via
/// `script_runner::rlib::workspace_dir`.
///
/// When either argument is `None` the output is identical to
/// [`generate_rust_project_json`].
pub fn generate_rust_project_json_with_script(
    model: &BuildModel,
    resolved: &ResolvedConfig,
    layout: &BuildLayout,
    rustc_info: &RustcInfo,
    gluon_rs_path: Option<&Path>,
    gluon_config_lib_rs: Option<&Path>,
) -> Value {
    // Start with the existing emitter.
    let mut json = generate_rust_project_json(model, resolved, layout, rustc_info);

    // If no gluon.rs is present, return unchanged.
    let (Some(rs_path), Some(lib_rs)) = (gluon_rs_path, gluon_config_lib_rs) else {
        return json;
    };

    let crates = json["crates"]
        .as_array_mut()
        .expect("generate_rust_project_json always emits a crates array");

    // Append the gluon-config source crate so rust-analyzer can resolve
    // its types when indexing the user's gluon.rs. We mark it as a
    // non-workspace member (it belongs to the gluon toolchain, not the
    // user's project). The `source` block's `include_dirs` points at the
    // crate's own source directory so rust-analyzer scans its modules.
    let config_lib_rs_dir = lib_rs
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let config_crate_index = crates.len();
    crates.push(json!({
        "root_module": lib_rs.to_string_lossy(),
        "edition": "2024",
        "deps": [],
        "cfg": [],
        "env": {},
        "is_workspace_member": false,
        "is_proc_macro": false,
        "source": {
            "include_dirs": [config_lib_rs_dir],
            "exclude_dirs": [],
        },
    }));

    // Append the gluon.rs build-script crate. Its only dep is the
    // gluon-config crate entry we just appended. The `source` block's
    // `include_dirs` covers the project root so rust-analyzer discovers
    // any modules the script might declare.
    let rs_parent = rs_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    crates.push(json!({
        "root_module": rs_path.to_string_lossy(),
        "edition": "2024",
        "deps": [
            { "name": "gluon_config", "crate": config_crate_index }
        ],
        "cfg": [],
        "env": {},
        "is_workspace_member": true,
        "is_proc_macro": false,
        "source": {
            "include_dirs": [rs_parent],
            "exclude_dirs": [layout.root().to_string_lossy()],
        },
    }));

    json
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, GroupDef, ProjectDef, ResolvedConfig,
        ResolvedCrateRef, ResolvedProfile, TargetDef, TargetSpec,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    fn fake_rustc_info(rust_src: Option<PathBuf>) -> RustcInfo {
        RustcInfo {
            rustc_path: PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src,
            mtime_ns: 0,
        }
    }

    /// Build a fixture model with a single cross target and a single
    /// "g" group covering it. Returns the target handle plus the
    /// group handle so callers can wire `CrateDef.group` explicitly.
    fn make_fixture_full() -> (BuildModel, Handle<TargetDef>, Handle<GroupDef>) {
        let mut model = BuildModel::default();
        let (target_handle, _) = model.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: Some("abort".into()),
                span: None,
            },
        );
        let (group_handle, _) = model.groups.insert(
            "g".into(),
            GroupDef {
                name: "g".into(),
                target: TargetSpec::Cross(target_handle),
                ..Default::default()
            },
        );
        (model, target_handle, group_handle)
    }

    /// Compatibility wrapper for tests that don't need the group
    /// handle. Discards the group handle and returns just `(model, th)`.
    fn make_fixture() -> (BuildModel, Handle<TargetDef>) {
        let (model, th, _) = make_fixture_full();
        (model, th)
    }

    /// Default values for newly-mandatory `CrateDef` fields.
    /// `make_fixture()` always installs the "g" group as the first
    /// element of `model.groups`, so its handle is `Handle::new(0)`.
    /// Tests use `..test_crate_defaults()` instead of
    /// `..Default::default()` since `CrateDef` no longer derives
    /// `Default`. The placeholder `Handle::new(0)` only points at a
    /// real group when `make_fixture()` was used.
    fn test_crate_defaults() -> CrateDef {
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
            group: Handle::new(0),
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        }
    }

    fn make_resolved(
        target_handle: Handle<TargetDef>,
        refs: Vec<ResolvedCrateRef>,
    ) -> ResolvedConfig {
        ResolvedConfig {
            project: ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: target_handle,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: BTreeMap::new(),
            crates: refs,
            build_dir: "/tmp/build".into(),
            project_root: "/tmp/proj".into(),
        }
    }

    fn layout() -> BuildLayout {
        BuildLayout::new("/tmp/build", "demo")
    }

    #[test]
    fn single_cross_crate_emits_config_plus_one() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);

        let crates = json["crates"].as_array().expect("crates array");
        assert_eq!(crates.len(), 2, "config + 1 user crate");
        // Index 0 is the config crate.
        assert!(
            crates[0]["root_module"]
                .as_str()
                .unwrap()
                .contains("demo_config"),
            "first entry must be config crate, got: {:?}",
            crates[0]
        );
        // Cross crate has a dep on the config crate at index 0.
        let kernel = &crates[1];
        assert_eq!(kernel["is_proc_macro"], false);
        let deps = kernel["deps"].as_array().unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0]["crate"], 0);
        assert_eq!(deps[0]["name"], "config");
        // Bin → main.rs.
        assert!(
            kernel["root_module"]
                .as_str()
                .unwrap()
                .ends_with("crates/kernel/src/main.rs")
        );
    }

    #[test]
    fn single_host_crate_has_no_config_dep() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "build-tool".into(),
            CrateDef {
                name: "build-tool".into(),
                path: "crates/build-tool".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);

        let crates = json["crates"].as_array().unwrap();
        assert_eq!(crates.len(), 2);
        let host = &crates[1];
        assert_eq!(host["deps"].as_array().unwrap().len(), 0);
        assert!(
            host["root_module"]
                .as_str()
                .unwrap()
                .ends_with("crates/build-tool/src/lib.rs")
        );
    }

    #[test]
    fn mixed_host_and_cross_both_appear() {
        let (mut model, th) = make_fixture();
        let (host_h, _) = model.crates.insert(
            "host-tool".into(),
            CrateDef {
                name: "host-tool".into(),
                path: "crates/host-tool".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let (cross_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: host_h,
                    target: th,
                    host: true,
                },
                ResolvedCrateRef {
                    handle: cross_h,
                    target: th,
                    host: false,
                },
            ],
        );
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        let crates = json["crates"].as_array().unwrap();
        assert_eq!(crates.len(), 3, "config + host + cross");
        assert_eq!(crates[1]["deps"].as_array().unwrap().len(), 0); // host
        assert_eq!(crates[2]["deps"].as_array().unwrap().len(), 1); // cross
    }

    #[test]
    fn proc_macro_crate_marked_correctly() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "macros".into(),
            CrateDef {
                name: "macros".into(),
                path: "crates/macros".into(),
                edition: "2021".into(),
                crate_type: CrateType::ProcMacro,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert_eq!(json["crates"][1]["is_proc_macro"], true);
    }

    #[test]
    fn missing_rust_src_omits_sysroot_src_key() {
        let (model, _th) = make_fixture();
        let resolved = make_resolved(_th, Vec::new());
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert!(
            json.get("sysroot_src").is_none(),
            "sysroot_src must be absent (not null) when rust_src is None, got: {json}"
        );
    }

    #[test]
    fn rust_src_present_emits_sysroot_src_with_library_suffix() {
        // rust-analyzer expects the `library/` subdir that holds core,
        // alloc, std — not the parent `src/rust` directory. Without this
        // suffix, stdlib completions come up shallow or empty.
        let (model, _th) = make_fixture();
        let resolved = make_resolved(_th, Vec::new());
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/path/to/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert_eq!(json["sysroot_src"], "/path/to/rust-src/library");
    }

    #[test]
    fn cross_crate_emits_target_field() {
        // Cross crates must carry a `target` field so rust-analyzer
        // evaluates cfg gates against the kernel's triple rather than
        // the host. Without this, `#[cfg(target_os = "none")]` blocks
        // are treated as inactive and get no completions.
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        // `spec` of the fixture target is the triple itself.
        assert_eq!(
            json["crates"][1]["target"], "x86_64-unknown-none",
            "cross crates must carry target field, got: {}",
            json["crates"][1]
        );
    }

    #[test]
    fn host_crate_omits_target_field() {
        // Host crates compile for the build machine; rust-analyzer's
        // default (host triple) is already correct. Emit absence, not
        // null — the latter is a hard error in some rust-analyzer
        // versions (see module docs).
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "host-tool".into(),
            CrateDef {
                name: "host-tool".into(),
                path: "crates/host-tool".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert!(
            json["crates"][1].get("target").is_none(),
            "host crates must omit target key entirely, got: {}",
            json["crates"][1]
        );
    }

    #[test]
    fn cross_crate_cfg_only_reflects_explicit_cfg_flags() {
        // Target-derived cfgs (target_os=…, target_arch=…) must NOT be
        // merged into the cfg array — rust-analyzer derives them itself
        // from the `target` field. Only the crate's own `cfg_flags`
        // should appear.
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                cfg_flags: vec!["feature=\"smp\"".into()],
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        let cfg = json["crates"][1]["cfg"].as_array().unwrap();
        assert_eq!(cfg.len(), 1, "expected only user cfg_flags, got: {cfg:?}");
        assert_eq!(cfg[0], "feature=\"smp\"");
    }

    #[test]
    fn dep_map_key_used_as_extern_name_not_crate_name() {
        let (mut model, th) = make_fixture();
        let (target_ch, _) = model.crates.insert(
            "real-name".into(),
            CrateDef {
                name: "real-name".into(),
                path: "crates/real-name".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        // Consumer crate has a dep keyed by "alias_name" pointing at
        // "real-name"'s handle.
        let mut deps = BTreeMap::new();
        deps.insert(
            "alias_name".to_string(),
            DepDef {
                resolution: DepResolution::Project(target_ch),
                features: Vec::new(),
                version: None,
                optional: false,
                default_features: true,
                span: None,
            },
        );
        let (consumer_ch, _) = model.crates.insert(
            "consumer".into(),
            CrateDef {
                name: "consumer".into(),
                path: "crates/consumer".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: target_ch,
                    target: th,
                    host: true,
                },
                ResolvedCrateRef {
                    handle: consumer_ch,
                    target: th,
                    host: true,
                },
            ],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        // consumer is the second user crate → index 2.
        let consumer = &json["crates"][2];
        let deps = consumer["deps"].as_array().unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0]["name"], "alias_name",
            "extern name must be the dep map key, not the target crate's name"
        );
        assert_eq!(deps[0]["crate"], 1);
    }

    #[test]
    fn vendor_dep_with_no_resolved_target_is_skipped() {
        let (mut model, th) = make_fixture();
        // Insert a "phantom" CrateDef so we can borrow its handle for
        // the dep, but DO NOT add it to resolved.crates — simulating
        // a vendor dep not yet wired up.
        let (phantom, _) = model.crates.insert(
            "phantom".into(),
            CrateDef {
                name: "phantom".into(),
                path: "vendor/phantom".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let mut deps = BTreeMap::new();
        deps.insert(
            "phantom".to_string(),
            DepDef {
                resolution: DepResolution::Project(phantom),
                features: Vec::new(),
                version: None,
                optional: false,
                default_features: true,
                span: None,
            },
        );
        let (consumer_ch, _) = model.crates.insert(
            "consumer".into(),
            CrateDef {
                name: "consumer".into(),
                path: "crates/consumer".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: consumer_ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        // Consumer is at index 1; its deps array must be empty
        // because the phantom dep was filtered out (not erroring).
        let consumer = &json["crates"][1];
        assert!(
            consumer["deps"].as_array().unwrap().is_empty(),
            "vendor dep must be silently skipped, got: {}",
            consumer["deps"]
        );
    }

    #[test]
    fn artifact_env_flows_into_rust_project_json() {
        // A consumer crate with `artifact_env(KERNEL_PATH: kernel)` must
        // appear in `rust-project.json` with `env.KERNEL_PATH` pointing
        // at the layout-derived future path of the kernel artifact.
        // Symmetry with `command_builder` is what makes `env!()` resolve
        // identically under rust-analyzer and rustc.
        let (mut model, th) = make_fixture();

        let (kernel_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Cross(th),
                ..test_crate_defaults()
            },
        );

        let mut artifact_env = BTreeMap::new();
        artifact_env.insert("KERNEL_PATH".to_string(), "kernel".to_string());
        let (loader_h, _) = model.crates.insert(
            "loader".into(),
            CrateDef {
                name: "loader".into(),
                path: "crates/loader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                artifact_env,
                ..test_crate_defaults()
            },
        );

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: loader_h,
                    target: th,
                    host: false,
                },
            ],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);

        // Index 0 is the config crate; index 1 is `kernel`; index 2 is `loader`.
        let loader = &json["crates"][2];
        let env = loader["env"].as_object().expect("env must be a JSON object");
        let kernel_path = env
            .get("KERNEL_PATH")
            .and_then(Value::as_str)
            .expect("KERNEL_PATH must be present");
        // The path must be the layout-derived future location of the
        // kernel rlib. Cross lib → cross_artifact_dir, libname-gluon-name.rlib.
        assert!(
            kernel_path.contains("/cross/x86_64-unknown-none/dev/kernel/"),
            "KERNEL_PATH should point at the layout-derived kernel dir, got: {kernel_path}"
        );
        assert!(
            kernel_path.ends_with("/libkernel-gluon-kernel.rlib"),
            "KERNEL_PATH should end at the deterministic rlib filename, got: {kernel_path}"
        );

        // The kernel crate itself has no artifact_env, so its env is empty.
        let kernel_entry = &json["crates"][1];
        assert!(
            kernel_entry["env"].as_object().unwrap().is_empty(),
            "kernel has no artifact_env so its env must be empty, got: {}",
            kernel_entry["env"]
        );
    }

    #[test]
    fn rust_project_json_includes_gluon_rs_entry_when_present() {
        let (model, th) = make_fixture();
        let resolved = make_resolved(th, vec![]);
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));

        let gluon_rs = PathBuf::from("/tmp/proj/gluon.rs");
        let lib_rs = PathBuf::from("/some/workspace/crates/gluon-config/src/lib.rs");

        let json = generate_rust_project_json_with_script(
            &model,
            &resolved,
            &layout(),
            &info,
            Some(&gluon_rs),
            Some(&lib_rs),
        );

        let crates = json["crates"].as_array().unwrap();

        // The gluon.rs entry must be present.
        let script_entry = crates
            .iter()
            .find(|c| {
                c["root_module"]
                    .as_str()
                    .map(|s| s.ends_with("gluon.rs"))
                    .unwrap_or(false)
            })
            .expect("gluon.rs crate entry must be present");

        let deps = script_entry["deps"].as_array().unwrap();
        assert!(
            deps.iter().any(|d| d["name"] == "gluon_config"),
            "gluon.rs entry must depend on gluon_config: {script_entry:?}"
        );

        // The dep's "crate" index must point at an entry whose root_module
        // is the gluon-config lib.rs.
        let dep_idx = deps
            .iter()
            .find(|d| d["name"] == "gluon_config")
            .and_then(|d| d["crate"].as_u64())
            .expect("gluon_config dep has crate index");
        let dep_entry = &crates[dep_idx as usize];
        assert!(
            dep_entry["root_module"]
                .as_str()
                .map(|s| s.contains("gluon-config"))
                .unwrap_or(false),
            "dep crate root_module must point at gluon-config lib.rs: {dep_entry:?}"
        );

        // The gluon-config entry must not be a workspace member.
        assert_eq!(
            dep_entry["is_workspace_member"], false,
            "gluon-config phantom entry must not be a workspace member"
        );

        // The gluon.rs entry must be a workspace member.
        assert_eq!(
            script_entry["is_workspace_member"], true,
            "gluon.rs entry must be a workspace member"
        );
    }

    #[test]
    fn rust_project_json_omits_gluon_rs_entry_when_absent() {
        // Existing behaviour preserved — generate_rust_project_json (no
        // script args) and generate_rust_project_json_with_script(None, None)
        // produce identical output.
        let (model, th) = make_fixture();
        let resolved = make_resolved(th, vec![]);
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));

        let v1 = generate_rust_project_json(&model, &resolved, &layout(), &info);
        let v2 = generate_rust_project_json_with_script(
            &model, &resolved, &layout(), &info, None, None,
        );
        assert_eq!(
            v1, v2,
            "with_script(None, None) must produce output identical to the no-script variant"
        );
    }
}
