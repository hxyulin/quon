//! `rust-project.json` generation for rust-analyzer.
//!
//! [`generate_rust_project_json`] is a pure function: it consumes an
//! [`AnalyzerInput`] (the model, the lowered unit table and the
//! build-script outputs of the last build) and produces a
//! [`serde_json::Value`] in rust-analyzer's `rust-project.json` format.
//! The caller (typically [`crate::configure`]) lowers the units, reads
//! the outputs with [`read_script_outputs`], and writes the file.
//!
//! ### One crate per unit
//!
//! Every unit except compiled build scripts becomes one entry: declared
//! and manifest crates, registry and vendored packages, and
//! `Dependency` crates. A package built for host and target
//! appears twice, with different `cfg` and `target`. Paths come from
//! the unit table, so they agree with the command builder.
//!
//! ### Build-script output
//!
//! A unit whose package has a build script gets the run's `rustc-cfg`
//! values in `cfg`, and `OUT_DIR` plus the `rustc-env` values in `env`,
//! from the run's fingerprint record. Before the first `quon build`
//! there is no record, and the fields are left out. Quon does not run
//! build scripts during `quon configure`.
//!
//! ### Schema reference
//!
//! See <https://rust-analyzer.github.io/manual.html#non-cargo-based-projects>.
//! The fields populated here are `sysroot`, `sysroot_src` (optional)
//! and `crates`
//! (each with `root_module`, `edition`, `deps`, `cfg`, `target`
//! (cross crates only), `env`, `is_workspace_member`, `is_proc_macro`,
//! `proc_macro_dylib_path` (proc macros only), `source`).

use crate::build_script::BuildScriptOutput;
use crate::cache::FingerprintStore;
use crate::cache::store::{FORMAT, Fingerprint};
use crate::compile::env::resolve_crate_env;
use crate::compile::{BuildLayout, RustcInfo};
use crate::scheduler::NodeData;
use crate::unit::{BuildUnit, DepTarget, Platform, RunId, UnitGraph, UnitId, UnitOrigin};
use crate::vendor::cross::context::VendorBuildContext;
use quon_model::{BuildModel, CrateType, ResolvedConfig};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Build-script outputs by run, as the last build recorded them.
pub type ScriptOutputs = BTreeMap<RunId, Arc<BuildScriptOutput>>;

/// Everything [`generate_rust_project_json`] reads.
#[derive(Debug, Clone, Copy)]
pub struct AnalyzerInput<'a> {
    pub model: &'a BuildModel,
    pub resolved: &'a ResolvedConfig,
    pub layout: &'a BuildLayout,
    pub rustc_info: &'a RustcInfo,
    /// The units of the build, from [`crate::unit::lower`].
    pub units: &'a UnitGraph,
    /// The vendor context `units` was lowered with. Gives vendored
    /// crates their package directory.
    pub vendor_ctx: &'a VendorBuildContext,
    pub script_outputs: &'a ScriptOutputs,
}

/// Read the output of every build-script run from its fingerprint
/// record. A run with no readable record, or one in another record
/// format, is left out.
///
/// The record is not checked for freshness: after an edit that would
/// rerun a script, the analyzer shows the output of the last run until
/// the next build.
pub fn read_script_outputs(
    units: &UnitGraph,
    store: &FingerprintStore,
    layout: &BuildLayout,
    model: &BuildModel,
    resolved: &ResolvedConfig,
) -> ScriptOutputs {
    units
        .runs()
        .filter_map(|(id, _)| {
            let key = units.run_key(id, layout, model, resolved).ok()?;
            let bytes = std::fs::read(store.record_path(&key)).ok()?;
            let fp: Fingerprint = serde_json::from_slice(&bytes).ok()?;
            match fp.data {
                Some(NodeData::BuildScript(out)) if fp.format == FORMAT && fp.key == key => {
                    Some((id, out))
                }
                _ => None,
            }
        })
        .collect()
}

/// Index of the generated config crate in the `crates` array.
const CONFIG_CRATE_INDEX: usize = 0;

/// Generate a `rust-project.json` value for the given build.
///
/// The generated `<project>_config` crate comes first, so its index is
/// fixed at `0`. Units follow in unit-id order: declared and manifest
/// crates in `resolved.crates` order, then vendored crates.
///
/// `sysroot_src` is omitted entirely (rather than set to `null`) when
/// [`RustcInfo::rust_src`] is `None`. rust-analyzer treats absence and
/// `null` differently: absence triggers its built-in fallback, while
/// `null` is a hard error in some versions.
pub fn generate_rust_project_json(input: &AnalyzerInput<'_>) -> Value {
    let AnalyzerInput {
        layout,
        rustc_info,
        units,
        ..
    } = *input;

    // Compiled build scripts get no entry; every other unit gets the
    // next index after the config crate.
    let index_of: BTreeMap<UnitId, usize> = units
        .iter()
        .filter(|(_, u)| !u.is_build_script)
        .enumerate()
        .map(|(i, (id, _))| (id, i + 1))
        .collect();

    let config_crate_dir = layout.generated_config_crate_dir();
    let mut crates_array: Vec<Value> = Vec::with_capacity(index_of.len() + 1);
    crates_array.push(json!({
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
    }));

    for &id in index_of.keys() {
        crates_array.push(unit_entry(input, id, &index_of));
    }

    let mut top = serde_json::Map::new();
    // rust-analyzer starts its proc-macro server from
    // `<sysroot>/libexec`; without `sysroot` it expands no proc macros.
    top.insert(
        "sysroot".to_string(),
        Value::String(rustc_info.sysroot.to_string_lossy().into_owned()),
    );
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

/// The `crates` entry of unit `id`.
fn unit_entry(input: &AnalyzerInput<'_>, id: UnitId, index_of: &BTreeMap<UnitId, usize>) -> Value {
    let AnalyzerInput {
        model,
        resolved,
        layout,
        units,
        vendor_ctx,
        script_outputs,
        ..
    } = *input;
    let unit: &BuildUnit = units.get(id);
    let krate = match unit.origin {
        UnitOrigin::Crate(h) => model.crates.get(h),
        UnitOrigin::Vendor(_) => None,
    };

    // The directory rust-analyzer treats as the crate's own: the crate
    // directory for declared and manifest crates (under
    // `$CARGO_HOME/registry/src` or a vendor directory for registry
    // packages), the package directory for `Dependency` crates.
    let crate_dir: PathBuf = match (unit.origin, krate) {
        (_, Some(k)) => resolved.project_root.join(&k.path),
        (UnitOrigin::Vendor(v), None) => vendor_ctx
            .get(v)
            .map(|b| b.manifest_dir.clone())
            .unwrap_or_else(|| unit.src.parent().map(Path::to_path_buf).unwrap_or_default()),
        (UnitOrigin::Crate(_), None) => {
            unit.src.parent().map(Path::to_path_buf).unwrap_or_default()
        }
    };

    let edition = if unit.edition.is_empty() {
        "2021"
    } else {
        unit.edition.as_str()
    };

    // --- deps ---
    //
    // The unit's `--extern` deps, under their extern names. Deps on
    // artifacts outside the unit table (prebuilt by cargo, or missing
    // until `quon vendor` runs) have no entry to point at.
    let mut deps: Vec<Value> = Vec::new();
    if unit.config_crate {
        deps.push(json!({ "name": "config", "crate": CONFIG_CRATE_INDEX }));
    }
    for dep in &unit.deps {
        let DepTarget::Unit(target) = dep.target else {
            continue;
        };
        if let Some(&idx) = index_of.get(&target) {
            deps.push(json!({ "name": dep.extern_name, "crate": idx }));
        }
    }

    let run_output = units
        .build_script_of(id)
        .and_then(|run| Some((run, script_outputs.get(&run)?)));

    // --- cfg ---
    //
    // Features, then Quon's `cfg_flag`s, then the build script's
    // `rustc-cfg` values, in the order rustc receives them.
    // Target-derived cfgs (target_arch, target_os, ...) are left out:
    // rust-analyzer derives them from the `target` field below with
    // `rustc --print=cfg`.
    let mut cfg: Vec<String> = unit
        .features
        .iter()
        .map(|f| format!("feature=\"{f}\""))
        .chain(unit.settings.cfg_flags.iter().cloned())
        .collect();
    // rustc sets `cfg(test)` itself under `--test`.
    if unit.kind == CrateType::Test {
        cfg.push("test".into());
    }
    if let Some((_, out)) = run_output {
        cfg.extend(out.cfgs.iter().cloned());
    }

    // --- target ---
    //
    // `spec` rather than `name`, because it is what rustc accepts as
    // `--target`: for custom targets `spec` is the JSON path. Host units
    // omit the field; rust-analyzer then uses the host triple.
    let target_spec: Option<String> = match unit.platform {
        Platform::Host => None,
        Platform::Cross(t) => model.targets.get(t).map(|t| t.spec.clone()),
    };

    // --- env ---
    //
    // The env rustc sees: `artifact_env` paths from the unit table,
    // `CARGO_*` for manifest units, and `OUT_DIR` plus `rustc-env` from
    // the build script. An `artifact_env` entry that does not resolve
    // drops that crate's `artifact_env` rather than failing the file.
    let mut include_dirs = vec![crate_dir.to_string_lossy().into_owned()];
    let mut env_map: BTreeMap<String, String> = BTreeMap::new();
    if let Some(k) = krate.filter(|_| !unit.is_build_script) {
        env_map = resolve_crate_env(k, model, |h| {
            let u = units.unit_of(h)?;
            Some(units.paths(u, layout, model, resolved).ok()?.output_path)
        })
        .unwrap_or_default();
    }
    if let Some(cargo) = &unit.settings.cargo {
        env_map.extend(cargo.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    if let Some((run, out)) = run_output
        && let Ok(dir) = units.run_dir(run, layout, model, resolved)
    {
        let out_dir = dir.join("out").to_string_lossy().into_owned();
        // The build directory is excluded below; including `OUT_DIR`
        // lets `include!(concat!(env!("OUT_DIR"), ...))` load its file.
        // rust-analyzer lets the longer of the two prefixes win.
        include_dirs.push(out_dir.clone());
        env_map.insert("OUT_DIR".into(), out_dir);
        env_map.extend(out.env.iter().cloned());
    }
    let env_json: serde_json::Map<String, Value> = env_map
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();

    let is_proc_macro = unit.kind == CrateType::ProcMacro;
    let mut entry = json!({
        "root_module": unit.src.to_string_lossy(),
        "edition": edition,
        "deps": deps,
        "cfg": cfg,
        "env": Value::Object(env_json),
        // Declared crates and Cargo workspace members are members;
        // dependencies from outside the workspace are not.
        "is_workspace_member": krate.is_some_and(|k| k.cargo.is_none() || k.is_project_crate),
        "is_proc_macro": is_proc_macro,
        "source": {
            "include_dirs": include_dirs,
            "exclude_dirs": [layout.root().to_string_lossy()],
        },
    });
    let obj = entry
        .as_object_mut()
        .expect("json! macro produces an object");
    if let Some(spec) = target_spec {
        obj.insert("target".to_string(), Value::String(spec));
    }
    // The dylib exists only after a build; until then rust-analyzer
    // reports the macros as unexpanded.
    if is_proc_macro && let Ok(paths) = units.paths(id, layout, model, resolved) {
        obj.insert(
            "proc_macro_dylib_path".to_string(),
            Value::String(paths.output_path.to_string_lossy().into_owned()),
        );
    }
    entry
}

/// Like [`generate_rust_project_json`] but also emits a crate entry for
/// `quon.rs` (the user's build script) when one is present, so
/// rust-analyzer indexes the build script alongside the kernel sources.
///
/// `quon_rs_path` is the absolute path to the user's `quon.rs`.
/// `quon_config_lib_rs` is the path to the bundled `quon-config` crate's
/// `lib.rs` source file so rust-analyzer can index it as a normal workspace
/// dep (rather than pointing at a `.rlib`, which the schema does not
/// support). It comes from the manifest of the shared quon-config
/// library (`script_runner::rlib::RlibPaths::quon_config_src`).
///
/// When either argument is `None` the output is identical to
/// [`generate_rust_project_json`].
pub fn generate_rust_project_json_with_script(
    input: &AnalyzerInput<'_>,
    quon_rs_path: Option<&Path>,
    quon_config_lib_rs: Option<&Path>,
) -> Value {
    let layout = input.layout;
    let mut json = generate_rust_project_json(input);

    // If no quon.rs is present, return unchanged.
    let (Some(rs_path), Some(lib_rs)) = (quon_rs_path, quon_config_lib_rs) else {
        return json;
    };

    let crates = json["crates"]
        .as_array_mut()
        .expect("generate_rust_project_json always emits a crates array");

    // Append the quon-config source crate so rust-analyzer can resolve
    // its types when indexing the user's quon.rs. We mark it as a
    // non-workspace member (it belongs to the quon toolchain, not the
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

    // Append the quon.rs build-script crate. Its only dep is the
    // quon-config crate entry we just appended. The `source` block's
    // `include_dirs` covers the project root so rust-analyzer discovers
    // any modules the script might declare. `target_kind: lib` because
    // quon.rs has no `main`: quon compiles it as a module of a
    // generated crate root.
    let rs_parent = rs_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    crates.push(json!({
        "root_module": rs_path.to_string_lossy(),
        "edition": "2024",
        "deps": [
            { "name": "quon_config", "crate": config_crate_index }
        ],
        "cfg": [],
        "env": {},
        "is_workspace_member": true,
        "is_proc_macro": false,
        "source": {
            "include_dirs": [rs_parent],
            "exclude_dirs": [layout.root().to_string_lossy()],
        },
        "build": {
            "label": "quon.rs",
            "build_file": rs_path.to_string_lossy(),
            "target_kind": "lib",
        },
    }));

    json
}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_model::{
        BuildModel, CrateDef, CrateType, DepDef, DepResolution, GroupDef, Handle, PanicStrategy,
        ProjectDef, ResolvedConfig, ResolvedCrateRef, ResolvedProfile, TargetDef, TargetSpec,
    };

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
                build_std: false,
                panic_strategy: Some(PanicStrategy::Abort),
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
            cargo: None,
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
                target: TargetSpec::Cross(target_handle),
                opt_level: 0,
                debug_info: false,
                lto: None,
                run_binary: None,
                runner: None,
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

    /// Lower `model` without vendored crates and call `f` with the
    /// analyzer input.
    fn with_input<T>(
        model: &BuildModel,
        resolved: &ResolvedConfig,
        info: &RustcInfo,
        script_outputs: &ScriptOutputs,
        f: impl FnOnce(&AnalyzerInput<'_>) -> T,
    ) -> T {
        with_vendor(
            model,
            resolved,
            info,
            &VendorBuildContext::new(),
            script_outputs,
            f,
        )
    }

    /// [`with_input`] with vendored crates from `vendor_ctx`.
    fn with_vendor<T>(
        model: &BuildModel,
        resolved: &ResolvedConfig,
        info: &RustcInfo,
        vendor_ctx: &VendorBuildContext,
        script_outputs: &ScriptOutputs,
        f: impl FnOnce(&AnalyzerInput<'_>) -> T,
    ) -> T {
        let units = crate::unit::lower(model, resolved, vendor_ctx).expect("lower");
        f(&AnalyzerInput {
            model,
            resolved,
            layout: &layout(),
            rustc_info: info,
            units: &units,
            vendor_ctx,
            script_outputs,
        })
    }

    fn gen_json(model: &BuildModel, resolved: &ResolvedConfig, info: &RustcInfo) -> Value {
        with_input(
            model,
            resolved,
            info,
            &ScriptOutputs::new(),
            generate_rust_project_json,
        )
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
        let json = gen_json(&model, &resolved, &info);

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
        let json = gen_json(&model, &resolved, &info);

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
    fn missing_rust_src_omits_sysroot_src_key() {
        let (model, _th) = make_fixture();
        let resolved = make_resolved(_th, Vec::new());
        let info = Arc::new(fake_rustc_info(None));
        let json = gen_json(&model, &resolved, &info);
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
        let json = gen_json(&model, &resolved, &info);
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
        let json = gen_json(&model, &resolved, &info);
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
        let json = gen_json(&model, &resolved, &info);
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
        let json = gen_json(&model, &resolved, &info);
        let cfg = json["crates"][1]["cfg"].as_array().unwrap();
        assert_eq!(cfg.len(), 1, "expected only user cfg_flags, got: {cfg:?}");
        assert_eq!(cfg[0], "feature=\"smp\"");
    }

    /// A manifest unit gets its feature cfgs, its `CARGO_*` env, and no
    /// config crate dep when its group does not ask for one.
    #[test]
    fn manifest_unit_gets_features_env_and_no_config_dep() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "util".into(),
            CrateDef {
                name: "util".into(),
                path: "crates/util".into(),
                target: TargetSpec::Cross(th),
                features: vec!["default".into(), "extra".into()],
                cfg_flags: vec!["fast".into()],
                is_project_crate: false,
                cargo: Some(quon_model::CargoUnitDef {
                    rustc_name: "util".into(),
                    env: BTreeMap::from([("CARGO_PKG_NAME".into(), "util".into())]),
                    ..Default::default()
                }),
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
        let json = gen_json(&model, &resolved, &info);
        let entry = &json["crates"][1];
        assert_eq!(
            entry["cfg"],
            json!(["feature=\"default\"", "feature=\"extra\"", "fast"])
        );
        assert_eq!(entry["env"]["CARGO_PKG_NAME"], "util");
        assert_eq!(entry["deps"], json!([]), "group `g` has config off");
        assert_eq!(entry["is_workspace_member"], false);
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
        let json = gen_json(&model, &resolved, &info);
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
        let json = gen_json(&model, &resolved, &info);
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
        let json = gen_json(&model, &resolved, &info);

        // Index 0 is the config crate; index 1 is `kernel`; index 2 is `loader`.
        let loader = &json["crates"][2];
        let env = loader["env"]
            .as_object()
            .expect("env must be a JSON object");
        let kernel_path = env
            .get("KERNEL_PATH")
            .and_then(Value::as_str)
            .expect("KERNEL_PATH must be present");
        // The path must be the future location of the kernel rlib, as
        // the unit table (and so the compile step) computes it.
        let units = crate::unit::lower(
            &model,
            &resolved,
            &crate::vendor::cross::context::VendorBuildContext::new(),
        )
        .unwrap();
        let expected = units
            .paths(
                units.unit_of(kernel_h).unwrap(),
                &layout(),
                &model,
                &resolved,
            )
            .unwrap()
            .output_path;
        assert_eq!(Path::new(kernel_path), expected);
        assert!(
            kernel_path.contains("/cross/x86_64-unknown-none/dev/deps/libkernel-"),
            "KERNEL_PATH should point at the cross deps dir, got: {kernel_path}"
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
    fn rust_project_json_includes_quon_rs_entry_when_present() {
        let (model, th) = make_fixture();
        let resolved = make_resolved(th, vec![]);
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));

        let quon_rs = PathBuf::from("/tmp/proj/quon.rs");
        let lib_rs = PathBuf::from("/some/workspace/crates/quon-config/src/lib.rs");

        let json = with_input(&model, &resolved, &info, &ScriptOutputs::new(), |input| {
            generate_rust_project_json_with_script(input, Some(&quon_rs), Some(&lib_rs))
        });

        let crates = json["crates"].as_array().unwrap();

        // The quon.rs entry must be present.
        let script_entry = crates
            .iter()
            .find(|c| {
                c["root_module"]
                    .as_str()
                    .map(|s| s.ends_with("quon.rs"))
                    .unwrap_or(false)
            })
            .expect("quon.rs crate entry must be present");

        let deps = script_entry["deps"].as_array().unwrap();
        assert!(
            deps.iter().any(|d| d["name"] == "quon_config"),
            "quon.rs entry must depend on quon_config: {script_entry:?}"
        );

        // The dep's "crate" index must point at an entry whose root_module
        // is the quon-config lib.rs.
        let dep_idx = deps
            .iter()
            .find(|d| d["name"] == "quon_config")
            .and_then(|d| d["crate"].as_u64())
            .expect("quon_config dep has crate index");
        let dep_entry = &crates[dep_idx as usize];
        assert!(
            dep_entry["root_module"]
                .as_str()
                .map(|s| s.contains("quon-config"))
                .unwrap_or(false),
            "dep crate root_module must point at quon-config lib.rs: {dep_entry:?}"
        );

        // The quon-config entry must not be a workspace member.
        assert_eq!(
            dep_entry["is_workspace_member"], false,
            "quon-config phantom entry must not be a workspace member"
        );

        // The quon.rs entry must be a workspace member.
        assert_eq!(
            script_entry["is_workspace_member"], true,
            "quon.rs entry must be a workspace member"
        );
        assert_eq!(script_entry["build"]["target_kind"], "lib");
    }

    #[test]
    fn rust_project_json_omits_quon_rs_entry_when_absent() {
        // Existing behaviour preserved — generate_rust_project_json (no
        // script args) and generate_rust_project_json_with_script(None, None)
        // produce identical output.
        let (model, th) = make_fixture();
        let resolved = make_resolved(th, vec![]);
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));

        let v1 = gen_json(&model, &resolved, &info);
        let v2 = with_input(&model, &resolved, &info, &ScriptOutputs::new(), |input| {
            generate_rust_project_json_with_script(input, None, None)
        });
        assert_eq!(
            v1, v2,
            "with_script(None, None) must produce output identical to the no-script variant"
        );
    }

    #[test]
    fn proc_macro_entry_points_at_its_dylib() {
        let (mut model, th) = make_fixture();
        let (h, _) = model.crates.insert(
            "derive".into(),
            CrateDef {
                name: "derive".into(),
                path: "crates/derive".into(),
                crate_type: CrateType::ProcMacro,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: h,
                target: th,
                host: true,
            }],
        );
        let info = fake_rustc_info(None);
        let json = gen_json(&model, &resolved, &info);
        let units = crate::unit::lower(&model, &resolved, &VendorBuildContext::new()).unwrap();
        let expected = units
            .paths(units.unit_of(h).unwrap(), &layout(), &model, &resolved)
            .unwrap()
            .output_path;
        let entry = &json["crates"][1];
        assert_eq!(entry["is_proc_macro"], true);
        assert_eq!(
            entry["proc_macro_dylib_path"].as_str().map(PathBuf::from),
            Some(expected)
        );
        // A library gets no dylib path.
        let config = &json["crates"][0];
        assert!(config.get("proc_macro_dylib_path").is_none());
    }

    mod vendored {
        use super::*;
        use crate::vendor::cross::graph::{VendorBuildScript, VendorCrateBuild, VendorDepEdge};
        use crate::vendor::cross::metadata::{CrateId, CrateKind};

        fn build(name: &str, target: TargetSpec, kind: CrateKind) -> VendorCrateBuild {
            VendorCrateBuild {
                crate_id: CrateId {
                    name: name.into(),
                    version: "1.0.0".into(),
                },
                cargo_target_name: name.replace('-', "_"),
                target,
                kind,
                manifest_dir: PathBuf::from(format!("/registry/{name}-1.0.0")),
                lib_path: PathBuf::from(format!("/registry/{name}-1.0.0/src/lib.rs")),
                edition: "2021".into(),
                features: vec!["std".into()],
                deps: vec![],
                build_script: None,
            }
        }

        /// A cross `zc` with a build script that uses the host proc
        /// macro `zc-derive`.
        fn fixture() -> (BuildModel, ResolvedConfig, VendorBuildContext) {
            let (model, th) = make_fixture();
            let resolved = make_resolved(th, vec![]);
            let mut vctx = VendorBuildContext::new();
            vctx.add(build("zc-derive", TargetSpec::Host, CrateKind::ProcMacro))
                .unwrap();
            let mut zc = build("zc", TargetSpec::Cross(th), CrateKind::Lib);
            zc.deps.push(VendorDepEdge {
                extern_name: "zc_derive".into(),
                crate_id: CrateId {
                    name: "zc-derive".into(),
                    version: "1.0.0".into(),
                },
                target: TargetSpec::Host,
            });
            zc.build_script = Some(VendorBuildScript {
                path: PathBuf::from("/registry/zc-1.0.0/build.rs"),
                edition: "2021".into(),
                build_deps: vec![],
                package: crate::build_script::PackageInfo {
                    name: "zc".into(),
                    version: "1.0.0".into(),
                    manifest_dir: PathBuf::from("/registry/zc-1.0.0"),
                    manifest_path: PathBuf::from("/registry/zc-1.0.0/Cargo.toml"),
                    ..Default::default()
                },
            });
            vctx.add(zc).unwrap();
            (model, resolved, vctx)
        }

        fn output() -> BuildScriptOutput {
            BuildScriptOutput {
                cfgs: vec!["zc_has_simd".into()],
                env: vec![("ZC_GENERATED".into(), "yes".into())],
                ..BuildScriptOutput::default()
            }
        }

        #[test]
        fn vendored_crates_get_entries_with_their_package_dir() {
            let (model, resolved, vctx) = fixture();
            let info = fake_rustc_info(None);
            let json = with_vendor(
                &model,
                &resolved,
                &info,
                &vctx,
                &ScriptOutputs::new(),
                generate_rust_project_json,
            );
            let crates = json["crates"].as_array().unwrap();
            // Config crate, zc-derive, zc. The compiled script has no
            // entry.
            assert_eq!(crates.len(), 3, "{crates:#?}");
            let derive = &crates[1];
            assert_eq!(derive["is_proc_macro"], true);
            assert!(derive["proc_macro_dylib_path"].is_string());
            assert!(derive.get("target").is_none());

            let zc = &crates[2];
            assert_eq!(zc["root_module"], "/registry/zc-1.0.0/src/lib.rs");
            assert_eq!(zc["source"]["include_dirs"][0], "/registry/zc-1.0.0");
            assert_eq!(zc["is_workspace_member"], false);
            assert_eq!(zc["target"], "x86_64-unknown-none");
            assert_eq!(zc["deps"], json!([{ "name": "zc_derive", "crate": 1 }]));
            // No build has run: no script cfgs and no OUT_DIR.
            assert_eq!(zc["cfg"], json!(["feature=\"std\""]));
            assert_eq!(zc["env"], json!({}));
        }

        #[test]
        fn recorded_script_output_adds_cfgs_out_dir_and_env() {
            let (model, resolved, vctx) = fixture();
            let info = fake_rustc_info(None);
            let units = crate::unit::lower(&model, &resolved, &vctx).unwrap();
            let (run, _) = units.runs().next().expect("zc has a run");
            let out_dir = units
                .run_dir(run, &layout(), &model, &resolved)
                .unwrap()
                .join("out");
            let outputs: ScriptOutputs = [(run, Arc::new(output()))].into();
            let json = with_vendor(&model, &resolved, &info, &vctx, &outputs, |i| {
                generate_rust_project_json(i)
            });
            let zc = &json["crates"][2];
            assert_eq!(zc["cfg"], json!(["feature=\"std\"", "zc_has_simd"]));
            assert_eq!(
                zc["source"]["include_dirs"],
                json!(["/registry/zc-1.0.0", out_dir.to_string_lossy()])
            );
            assert_eq!(
                zc["env"],
                json!({
                    "OUT_DIR": out_dir.to_string_lossy(),
                    "ZC_GENERATED": "yes",
                })
            );
        }

        #[test]
        fn script_outputs_are_read_from_run_records() {
            let (model, resolved, vctx) = fixture();
            let units = crate::unit::lower(&model, &resolved, &vctx).unwrap();
            let (run, _) = units.runs().next().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let store = FingerprintStore::new(tmp.path(), vec![]);
            assert!(read_script_outputs(&units, &store, &layout(), &model, &resolved).is_empty());

            let key = units.run_key(run, &layout(), &model, &resolved).unwrap();
            store
                .record(crate::cache::RecordInput {
                    key: &key,
                    command: [0; 32],
                    toolchain: [0; 32],
                    declared: &[],
                    discovered: &[],
                    env: BTreeMap::new(),
                    outputs: &[],
                    started_at: std::time::SystemTime::now(),
                    stderr: b"",
                    data: Some(NodeData::BuildScript(Arc::new(output()))),
                })
                .unwrap();
            let read = read_script_outputs(&units, &store, &layout(), &model, &resolved);
            assert_eq!(read.len(), 1);
            assert_eq!(*read[&run], output());
        }
    }
}
