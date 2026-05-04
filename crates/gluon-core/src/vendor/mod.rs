//! Vendoring: delegate dependency resolution to `cargo vendor`.
//!
//! Sub-project #3 — kernel-agnostic dependency fetching. Gluon does not
//! reimplement cargo's resolver; it writes a synthesised `Cargo.toml`
//! from the declared [`gluon_model::ExternalDepDef`]s, shells out to
//! `cargo vendor`, pins the result in `gluon.lock`, and auto-registers
//! each vendored crate into the live [`gluon_model::BuildModel`] so the
//! compile path sees a uniform set of crates regardless of origin.
//!
//! # Module layout
//!
//! - [`lockfile`] — `gluon.lock` on-disk type, TOML I/O, atomic save.
//! - [`fingerprint`] — content hash over the declared external-dep set,
//!   used as the "do we need to re-run `cargo vendor`?" signal.
//! - [`manifest_gen`] — emits the scratch `Cargo.toml` that `cargo
//!   vendor` reads.
//! - [`cargo_cmd`] — thin subprocess wrapper around `cargo vendor`.
//! - [`checksum`] — per-vendored-directory SHA-256 for tamper detection.
//!
//! Top-level entry points — [`vendor_sync`], [`vendor_check`], and
//! [`auto_register_vendored_deps`] (added in step 8) — live in this
//! `mod.rs`.

pub mod cargo_cmd;
pub mod checksum;
pub mod cross;
pub mod fingerprint;
pub mod lockfile;
pub mod manifest_gen;

pub use lockfile::{VendorLock, VendorLockPackage};

use crate::compile::BuildLayout;
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, DepSource, ExternalArtifactKey, ExternalDepDef, GitRef};
use std::fs;
use std::path::{Path, PathBuf};

/// Options for [`vendor_sync`].
#[derive(Debug, Clone, Copy, Default)]
pub struct VendorOptions {
    /// Bypass the fingerprint-match fast path and always re-run
    /// `cargo vendor`. Useful when the user has hand-poked `vendor/`
    /// and wants to restore it to a known state without having to
    /// delete the lockfile.
    pub force: bool,
    /// Pass `--offline`/`--frozen` to cargo. Useful in CI where
    /// network access is forbidden and the lockfile is expected to
    /// already be current.
    pub offline: bool,
}

/// Report produced by [`vendor_check`]. Empty means the vendor tree
/// is in sync with both the model and the lockfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorCheckReport {
    /// Fingerprint in `gluon.lock` does not match the live model.
    /// The user has edited `gluon.rs` since the last vendor run.
    pub fingerprint_mismatch: bool,
    /// Package names listed in `gluon.lock` whose vendored directory
    /// is missing from disk.
    pub missing: Vec<String>,
    /// Package names whose on-disk checksum disagrees with the
    /// checksum in `gluon.lock` — hand-editing, bitrot, or (if you're
    /// unlucky) a supply-chain attack.
    pub checksum_mismatch: Vec<String>,
    /// `gluon.lock` is missing but the model declares external deps.
    pub lock_missing: bool,
}

impl VendorCheckReport {
    /// `true` iff everything is in sync — no reason to re-vendor.
    pub fn is_clean(&self) -> bool {
        !self.fingerprint_mismatch
            && !self.lock_missing
            && self.missing.is_empty()
            && self.checksum_mismatch.is_empty()
    }

    /// Render the report as a human-readable diagnostic list. The CLI
    /// layer prints these and returns a non-zero exit code.
    pub fn to_diagnostics(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        if self.lock_missing {
            out.push(
                Diagnostic::error("gluon.lock is missing")
                    .with_note("run `gluon vendor` to generate it"),
            );
        }
        if self.fingerprint_mismatch {
            out.push(
                Diagnostic::error(
                    "vendor state is stale: declared dependencies differ from gluon.lock",
                )
                .with_note("run `gluon vendor` to refresh"),
            );
        }
        for name in &self.missing {
            out.push(
                Diagnostic::error(format!("vendored crate '{name}' is missing from vendor/"))
                    .with_note("run `gluon vendor` to re-populate"),
            );
        }
        for name in &self.checksum_mismatch {
            out.push(
                Diagnostic::error(format!(
                    "vendored crate '{name}' has been modified (checksum mismatch)"
                ))
                .with_note("run `gluon vendor` to restore"),
            );
        }
        out
    }
}

/// Read the `[package]` `name` and `version` from a `Cargo.toml`.
///
/// Used both to extract a vendored crate's recorded version after
/// `cargo vendor` has written it, and to read a path dep's version
/// at lockfile-construction time. Returns a diagnostic on parse
/// failure with the path attached.
fn read_cargo_package_name_version(manifest_path: &Path) -> Result<(String, String)> {
    let bytes = fs::read(manifest_path).map_err(|e| Error::Io {
        path: manifest_path.to_path_buf(),
        source: e,
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "Cargo.toml at {} is not valid UTF-8: {e}",
            manifest_path.display()
        ))])
    })?;
    let parsed: toml::Value = toml::from_str(text).map_err(|e| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "failed to parse {}: {e}",
            manifest_path.display()
        ))])
    })?;

    let pkg = parsed.get("package").ok_or_else(|| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "{} has no [package] table",
            manifest_path.display()
        ))])
    })?;
    let name = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::Diagnostics(vec![Diagnostic::error(format!(
                "{} has no package.name",
                manifest_path.display()
            ))])
        })?
        .to_string();
    let version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::Diagnostics(vec![Diagnostic::error(format!(
                "{} has no package.version",
                manifest_path.display()
            ))])
        })?
        .to_string();
    Ok((name, version))
}

/// Resolve the vendored directory for a declared dep by scanning
/// `vendor_dir` for a subdirectory whose `Cargo.toml` matches the
/// declared dep name.
///
/// We walk the dir rather than computing `<name>-<version>` because
/// cargo's naming scheme is not stable across source types — git
/// deps in particular use a hash suffix rather than the declared
/// ref. For the small number of crates a gluon project is likely to
/// declare, an O(N) scan is fine.
fn find_vendored_crate_dir(vendor_dir: &Path, name: &str) -> Result<Option<PathBuf>> {
    if !vendor_dir.exists() {
        return Ok(None);
    }
    let entries = fs::read_dir(vendor_dir).map_err(|e| Error::Io {
        path: vendor_dir.to_path_buf(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io {
            path: vendor_dir.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let cargo_toml = path.join("Cargo.toml");
        if !cargo_toml.exists() {
            continue;
        }
        match read_cargo_package_name_version(&cargo_toml) {
            Ok((pkg_name, _ver)) if pkg_name == name => return Ok(Some(path)),
            Ok(_) => continue,
            // Don't fail the whole scan on a single unreadable
            // Cargo.toml — cargo vendor occasionally writes workspace
            // fragments whose manifests we don't care about. Log and
            // move on.
            Err(_) => continue,
        }
    }
    Ok(None)
}

/// Encode a [`DepSource`] as the string stored in
/// [`VendorLockPackage::source`].
fn encode_source_string(source: &DepSource) -> String {
    match source {
        DepSource::CratesIo { .. } => "crates-io".to_string(),
        DepSource::Git { url, reference } => {
            let suffix = match reference {
                GitRef::Rev(s) => format!("rev={s}"),
                GitRef::Tag(s) => format!("tag={s}"),
                GitRef::Branch(s) => format!("branch={s}"),
                GitRef::Default => "default".to_string(),
            };
            format!("git+{url}#{suffix}")
        }
        DepSource::Path { path } => format!("path+{path}"),
    }
}

/// Synchronise `vendor/` with the declared `external_deps` set.
///
/// Fast path: if `gluon.lock` exists, its fingerprint matches the
/// current model, and every locked package's on-disk checksum still
/// matches, we do nothing and return the existing lock. `opts.force`
/// bypasses the fast path.
///
/// Slow path: generate the scratch `Cargo.toml` under
/// `layout.vendor_workspace_dir()`, run `cargo vendor` against it,
/// compute per-package checksums, and write the fresh `gluon.lock`.
///
/// Path deps are handled without invoking cargo — their entries in
/// the lock are derived directly from the declared path plus the
/// `[package]` table in the local `Cargo.toml`.
pub fn vendor_sync(
    model: &BuildModel,
    layout: &BuildLayout,
    project_root: &Path,
    opts: VendorOptions,
) -> Result<VendorLock> {
    let fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
    let lock_path = layout.gluon_lock(project_root);
    let vendor_dir = layout.vendor_dir(project_root);

    // Fast path: the lock exists, the fingerprint matches, and
    // vendor_check reports clean. We return early without running
    // cargo vendor at all.
    if !opts.force
        && let Ok(Some(existing)) = VendorLock::load(&lock_path)
        && existing.fingerprint == fingerprint
    {
        // Verify on-disk state against the existing lock so a user
        // who just rebased onto a branch with a fresh gluon.lock but
        // no vendor/ still triggers the slow path.
        let report = verify_lock_against_disk(&existing, &vendor_dir)?;
        if report.is_clean() {
            return Ok(existing);
        }
    }

    // Slow path.
    if model.external_deps.is_empty() {
        // No declared deps — write an empty lock so future fast-path
        // checks succeed without re-reading an empty vendor dir. Also
        // skip the `cargo vendor` invocation entirely; there is
        // nothing to resolve.
        let lock = VendorLock::empty(fingerprint);
        lock.save_atomic(&lock_path)?;
        return Ok(lock);
    }

    // Generate scratch workspace for the non-Path deps. If every
    // declared dep is Path-sourced, the generated manifest will have
    // no dependencies and we can skip running cargo vendor.
    let workspace_dir = layout.vendor_workspace_dir();
    manifest_gen::write_vendor_workspace(
        &workspace_dir,
        &model.external_deps,
        gluon_model::TargetSpec::Host,
    )?;

    let has_non_path_dep = model
        .external_deps
        .iter()
        .any(|(_, d)| !matches!(d.source, DepSource::Path { .. }));
    if has_non_path_dep {
        cargo_cmd::run_cargo_vendor(
            &workspace_dir,
            &vendor_dir,
            cargo_cmd::VendorFlags {
                offline: opts.offline,
            },
        )?;
    }

    // Build lockfile entries from the declared set. Transitive deps
    // that cargo pulled into vendor/ are not recorded in gluon.lock
    // for MVP — see the plan's "Out of scope" section.
    let mut packages: Vec<VendorLockPackage> = Vec::new();
    let mut diags: Vec<Diagnostic> = Vec::new();

    // Iterate in sorted order so the output is deterministic.
    let mut entries: Vec<(&str, &ExternalDepDef)> = model
        .external_deps
        .names()
        .filter_map(|(n, h)| model.external_deps.get(h).map(|d| (n, d)))
        .collect();
    entries.sort_by_key(|(n, _)| *n);

    for (name, dep) in entries {
        match &dep.source {
            DepSource::Path { path } => {
                let abs = resolve_path_dep(project_root, path);
                let manifest = abs.join("Cargo.toml");
                if !manifest.exists() {
                    diags.push(
                        Diagnostic::error(format!(
                            "path dependency '{name}' has no Cargo.toml at {}",
                            manifest.display()
                        ))
                        .with_optional_span(dep.span.clone()),
                    );
                    continue;
                }
                match read_cargo_package_name_version(&manifest) {
                    Ok((_pkg_name, version)) => {
                        packages.push(VendorLockPackage {
                            name: name.to_string(),
                            version,
                            source: encode_source_string(&dep.source),
                            checksum: None,
                            // Filled in by the cargo-build pass below.
                            artifact_path: None,
                        });
                    }
                    Err(e) => {
                        // Propagate read errors as vendor-level
                        // diagnostics so the user knows which dep is
                        // broken.
                        if let Error::Diagnostics(mut ds) = e {
                            diags.append(&mut ds);
                        } else {
                            diags.push(Diagnostic::error(format!("path dependency '{name}': {e}")));
                        }
                    }
                }
            }
            DepSource::CratesIo { .. } | DepSource::Git { .. } => {
                let dir = find_vendored_crate_dir(&vendor_dir, name)?;
                let dir = match dir {
                    Some(d) => d,
                    None => {
                        diags.push(
                            Diagnostic::error(format!(
                                "cargo vendor did not produce a directory for '{name}'"
                            ))
                            .with_optional_span(dep.span.clone())
                            .with_note(format!(
                                "expected to find it under {}",
                                vendor_dir.display()
                            )),
                        );
                        continue;
                    }
                };
                let (_, version) = read_cargo_package_name_version(&dir.join("Cargo.toml"))?;
                let checksum = checksum::checksum_vendored_dir(&dir)?;
                packages.push(VendorLockPackage {
                    name: name.to_string(),
                    version,
                    source: encode_source_string(&dep.source),
                    checksum: Some(checksum),
                    // Filled in by the cargo-build pass below.
                    artifact_path: None,
                });
            }
        }
    }

    if !diags.is_empty() {
        return Err(Error::Diagnostics(diags));
    }

    // Cargo-as-builder pass. Compile the scratch workspace so we
    // capture the rlib / dylib paths cargo produced — this is what
    // gluon's compile path will `--extern` against. Punted to a
    // follow-up issue: path-only models (which never run cargo
    // vendor) do not yet trigger a cargo build either, so consumer
    // crates that depend on a path dep will fail with a "missing
    // artifact" diagnostic at compile time. Lepton-os doesn't use
    // path-sourced external deps, so this is fine for v1.
    if has_non_path_dep {
        let cargo_target_dir = workspace_dir.join("target");
        let artifacts = cargo_cmd::run_cargo_build_vendor(
            &workspace_dir,
            &cargo_target_dir,
            &vendor_dir,
            cargo_cmd::VendorFlags {
                offline: opts.offline,
            },
        )?;
        attach_artifact_paths(&mut packages, &artifacts);
    }

    let lock = VendorLock {
        version: VendorLock::CURRENT_VERSION,
        fingerprint,
        packages,
    };
    lock.save_atomic(&lock_path)?;
    Ok(lock)
}

/// Attach cargo's emitted artifact paths to lockfile packages.
///
/// Cargo's `target.name` normalises hyphens to underscores
/// (`proc-macro2` → `proc_macro2`), but our lockfile keeps the
/// original hyphen-bearing crate name. Match by normalising both
/// sides to underscores. Packages whose artifact wasn't found
/// (typically path deps that the cargo workspace doesn't include)
/// are left with `artifact_path = None`.
fn attach_artifact_paths(
    packages: &mut [VendorLockPackage],
    artifacts: &std::collections::BTreeMap<String, std::path::PathBuf>,
) {
    for pkg in packages.iter_mut() {
        let normalised = pkg.name.replace('-', "_");
        if let Some(p) = artifacts.get(&normalised) {
            pkg.artifact_path = Some(p.clone());
        }
    }
}

/// Verify `gluon.lock` against the live model and the on-disk
/// `vendor/` tree without modifying anything.
///
/// Returns a [`VendorCheckReport`]. Use [`VendorCheckReport::is_clean`]
/// to collapse it to a yes/no.
pub fn vendor_check(
    model: &BuildModel,
    layout: &BuildLayout,
    project_root: &Path,
) -> Result<VendorCheckReport> {
    let lock_path = layout.gluon_lock(project_root);
    let vendor_dir = layout.vendor_dir(project_root);

    let Some(lock) = VendorLock::load(&lock_path)? else {
        // If there are no declared deps, missing lock is fine.
        if model.external_deps.is_empty() {
            return Ok(VendorCheckReport::default());
        }
        return Ok(VendorCheckReport {
            lock_missing: true,
            ..Default::default()
        });
    };

    let mut report = VendorCheckReport::default();
    let expected_fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
    if lock.fingerprint != expected_fingerprint {
        report.fingerprint_mismatch = true;
    }

    let disk = verify_lock_against_disk(&lock, &vendor_dir)?;
    report.missing = disk.missing;
    report.checksum_mismatch = disk.checksum_mismatch;

    Ok(report)
}

/// Walk the packages in `lock` and verify their on-disk state
/// against the recorded checksums.
///
/// Path deps (no checksum) are only existence-checked. Missing dirs
/// land in `missing`; differing checksums in `checksum_mismatch`.
/// Does not touch the fingerprint or the lock-missing flags — those
/// are caller concerns.
fn verify_lock_against_disk(lock: &VendorLock, vendor_dir: &Path) -> Result<VendorCheckReport> {
    let mut report = VendorCheckReport::default();
    for pkg in &lock.packages {
        if pkg.source.starts_with("path+") {
            // Path deps don't live under vendor/. We could resolve
            // the path and check it exists, but the lock entry
            // doesn't carry the absolute project root, so we leave
            // full path validation to auto_register_vendored_deps.
            continue;
        }
        let dir = match find_vendored_crate_dir(vendor_dir, &pkg.name)? {
            Some(d) => d,
            None => {
                report.missing.push(pkg.name.clone());
                continue;
            }
        };
        if let Some(expected) = &pkg.checksum {
            let actual = checksum::checksum_vendored_dir(&dir)?;
            if actual != *expected {
                report.checksum_mismatch.push(pkg.name.clone());
            }
        }
    }
    Ok(report)
}

/// Resolve a declared path-dep string to an absolute path rooted at
/// `project_root`. Absolute inputs are returned as-is.
fn resolve_path_dep(project_root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    }
}

/// Read `gluon.lock` and populate `model.external_artifacts` with the
/// rlib / dylib path that cargo produced for each pinned external
/// dep.
///
/// This is the link between the cargo-as-builder vendor pipeline (run
/// by `gluon vendor`) and gluon's compile pipeline. The compile path
/// (`compile/command_builder.rs`) consults `model.external_artifacts`
/// when it sees a `DepResolution::External(handle)` edge, looks the
/// dep's name up in the map, and emits `--extern <alias>=<path>` for
/// the consumer's rustc invocation.
///
/// # Contract
///
/// - No-op if `model.external_deps` is empty **and** `gluon.lock`
///   does not exist.
/// - Errors if `external_deps` is non-empty and `gluon.lock` is
///   missing, with an actionable diagnostic pointing the user at
///   `gluon vendor`.
/// - Errors if `gluon.lock`'s fingerprint disagrees with the live
///   model's.
/// - Errors if any pinned package lacks an `artifact_path` (means the
///   lockfile predates the cargo-as-builder pipeline) or the recorded
///   path is missing on disk (build/ was wiped). Both diagnostics
///   point the user at `gluon vendor`.
///
/// This function is idempotent: re-running it just re-inserts the
/// same map entries. Cheap to call defensively on every model load.
///
/// # Ordering
///
/// Must be called **after** the build model is loaded (intern pass
/// already ran on user-declared deps) and **before** any compile or
/// resolve pass that consults `model.external_artifacts`.
pub fn auto_register_vendored_deps(
    model: &mut BuildModel,
    layout: &BuildLayout,
    project_root: &Path,
) -> Result<()> {
    let lock_path = layout.gluon_lock(project_root);

    let maybe_lock = VendorLock::load(&lock_path)?;
    match (&maybe_lock, model.external_deps.is_empty()) {
        (None, true) => return Ok(()), // nothing to do
        (None, false) => {
            return Err(Error::Diagnostics(vec![
                Diagnostic::error("external dependencies are declared but no gluon.lock was found")
                    .with_note("run `gluon vendor` to generate gluon.lock and populate vendor/"),
            ]));
        }
        (Some(_), _) => { /* fall through */ }
    }
    let lock = maybe_lock.unwrap();

    let expected_fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
    if lock.fingerprint != expected_fingerprint {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(
                "vendor state is stale: gluon.lock does not match the declared dependencies",
            )
            .with_note("run `gluon vendor` to refresh"),
        ]));
    }

    if lock.packages.is_empty() {
        return Ok(());
    }

    // Populate the artifact map from the lockfile. The compile path
    // looks up `external_deps[h].name` against this map to wire
    // `--extern <alias>=<path>` for each consumer crate's
    // `DepResolution::External(h)` edge — see
    // `compile/command_builder.rs`.
    //
    // Path deps don't go through cargo build in v1, so they have no
    // artifact_path; we silently skip them here. Anything that tries
    // to consume them at compile time will surface a clearer "no
    // artifact for X" diagnostic from `command_builder`. Crates-io
    // and git entries, by contrast, *must* have an artifact_path —
    // a missing one means the lockfile predates the cargo-as-builder
    // pipeline and the user needs to re-run `gluon vendor`.
    let mut diags: Vec<Diagnostic> = Vec::new();
    for pkg in &lock.packages {
        let is_path_source = pkg.source.starts_with("path+");
        let Some(path) = pkg.artifact_path.as_ref() else {
            if !is_path_source {
                diags.push(
                    Diagnostic::error(format!(
                        "gluon.lock entry '{}' has no artifact_path",
                        pkg.name
                    ))
                    .with_note(
                        "this usually means the lockfile predates the cargo-as-builder \
                         vendor pipeline",
                    )
                    .with_note("run `gluon vendor` to refresh"),
                );
            }
            continue;
        };
        if !path.exists() {
            diags.push(
                Diagnostic::error(format!(
                    "gluon.lock entry '{}' points at missing artifact: {}",
                    pkg.name,
                    path.display(),
                ))
                .with_note(
                    "the cargo build/ directory was probably wiped — \
                     run `gluon vendor` to rebuild",
                ),
            );
            continue;
        }
        // The lockfile entry preserves the user-declared dep name
        // (hyphens intact). The compile path normalises to underscores
        // before lookup — match that here so both sides agree on the
        // key shape. v1 lockfile entries have no per-package target
        // field, so they all key under `Host`; the v2 schema bump
        // (cross-target vendored builds) will plumb the package's
        // declared target through here.
        let key = ExternalArtifactKey {
            target: gluon_model::TargetSpec::Host,
            cargo_target_name: pkg.name.replace('-', "_"),
        };
        model.external_artifacts.insert(key, path.clone());
    }

    if !diags.is_empty() {
        return Err(Error::Diagnostics(diags));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::Arena;
    use std::collections::BTreeMap;

    #[test]
    fn attach_artifact_paths_normalises_hyphens() {
        let mut packages = vec![
            VendorLockPackage {
                name: "syn".into(),
                version: "2.0.117".into(),
                source: "crates-io".into(),
                checksum: Some("sha256:a".into()),
                artifact_path: None,
            },
            VendorLockPackage {
                name: "proc-macro2".into(),
                version: "1.0.106".into(),
                source: "crates-io".into(),
                checksum: Some("sha256:b".into()),
                artifact_path: None,
            },
            VendorLockPackage {
                name: "missing-from-cargo".into(),
                version: "1.0.0".into(),
                source: "path+../missing".into(),
                checksum: None,
                artifact_path: None,
            },
        ];
        let mut artifacts: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
        artifacts.insert("syn".into(), "/tmp/libsyn-1.rlib".into());
        artifacts.insert("proc_macro2".into(), "/tmp/libproc_macro2-2.dylib".into());

        attach_artifact_paths(&mut packages, &artifacts);

        assert_eq!(
            packages[0].artifact_path,
            Some("/tmp/libsyn-1.rlib".into())
        );
        assert_eq!(
            packages[1].artifact_path,
            Some("/tmp/libproc_macro2-2.dylib".into()),
            "hyphen in lockfile name must match underscore in cargo target name"
        );
        assert_eq!(
            packages[2].artifact_path, None,
            "package not in cargo output stays None — caller decides what that means"
        );
    }

    fn minimal_layout(tmp: &Path) -> BuildLayout {
        BuildLayout::new(tmp.join("build"), "demo")
    }

    fn external_dep(name: &str, source: DepSource) -> ExternalDepDef {
        ExternalDepDef {
            name: name.into(),
            source,
            features: Vec::new(),
            default_features: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            target: gluon_model::TargetSpec::Host,
            span: None,
        }
    }

    fn model_with_deps(deps: Vec<ExternalDepDef>) -> BuildModel {
        let mut m = BuildModel::default();
        let mut arena: Arena<ExternalDepDef> = Arena::new();
        for d in deps {
            let name = d.name.clone();
            arena.insert(name, d);
        }
        m.external_deps = arena;
        m
    }

    #[test]
    fn empty_model_writes_empty_lock_and_no_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        std::fs::create_dir_all(layout.root()).unwrap();

        let model = BuildModel::default();
        let lock = vendor_sync(&model, &layout, &project, VendorOptions::default()).unwrap();
        assert_eq!(lock.packages.len(), 0);
        assert!(project.join("gluon.lock").exists());
        // We skip cargo entirely and never create the scratch
        // workspace for the empty case.
        assert!(!layout.vendor_workspace_dir().exists());
    }

    #[test]
    fn path_only_model_writes_lock_without_running_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        std::fs::create_dir_all(layout.root()).unwrap();

        // Synthesize a local crate on disk.
        let local_dir = project.join("helper");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("Cargo.toml"),
            b"[package]\nname=\"helper\"\nversion=\"0.7.1\"\nedition=\"2021\"\n",
        )
        .unwrap();

        let model = model_with_deps(vec![external_dep(
            "helper",
            DepSource::Path {
                path: "helper".into(),
            },
        )]);
        let lock = vendor_sync(&model, &layout, &project, VendorOptions::default()).unwrap();
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "helper");
        assert_eq!(lock.packages[0].version, "0.7.1");
        assert_eq!(lock.packages[0].source, "path+helper");
        assert!(lock.packages[0].checksum.is_none());
    }

    #[test]
    fn path_dep_with_missing_cargo_toml_is_diagnosed() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        std::fs::create_dir_all(layout.root()).unwrap();

        let model = model_with_deps(vec![external_dep(
            "ghost",
            DepSource::Path {
                path: "does/not/exist".into(),
            },
        )]);
        let err = vendor_sync(&model, &layout, &project, VendorOptions::default())
            .expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("has no Cargo.toml"), "msg: {msg}");
        assert!(msg.contains("ghost"), "msg: {msg}");
    }

    #[test]
    fn vendor_check_reports_missing_lock_when_deps_declared() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        let model = model_with_deps(vec![external_dep(
            "log",
            DepSource::CratesIo {
                version: "0.4".into(),
            },
        )]);
        let report = vendor_check(&model, &layout, &project).unwrap();
        assert!(report.lock_missing);
        assert!(!report.is_clean());
    }

    #[test]
    fn vendor_check_clean_for_empty_model_with_no_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        let model = BuildModel::default();
        let report = vendor_check(&model, &layout, &project).unwrap();
        assert!(report.is_clean());
    }

    #[test]
    fn vendor_sync_fast_path_reuses_existing_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        std::fs::create_dir_all(layout.root()).unwrap();

        // Path-only model so we never need cargo.
        let local_dir = project.join("p");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("Cargo.toml"),
            b"[package]\nname=\"p\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        let model = model_with_deps(vec![external_dep(
            "p",
            DepSource::Path { path: "p".into() },
        )]);

        let l1 = vendor_sync(&model, &layout, &project, VendorOptions::default()).unwrap();
        // Second call — should hit the fast path and produce an
        // identical lock.
        let l2 = vendor_sync(&model, &layout, &project, VendorOptions::default()).unwrap();
        assert_eq!(l1, l2);
    }

    #[test]
    fn vendor_check_detects_fingerprint_mismatch_after_model_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        std::fs::create_dir_all(layout.root()).unwrap();

        // Vendor an initial path dep.
        let local_dir = project.join("p");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("Cargo.toml"),
            b"[package]\nname=\"p\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        let model_old = model_with_deps(vec![external_dep(
            "p",
            DepSource::Path { path: "p".into() },
        )]);
        vendor_sync(&model_old, &layout, &project, VendorOptions::default()).unwrap();

        // New model adds a second dep — fingerprint must now disagree.
        let local_q = project.join("q");
        std::fs::create_dir_all(&local_q).unwrap();
        std::fs::write(
            local_q.join("Cargo.toml"),
            b"[package]\nname=\"q\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        let model_new = model_with_deps(vec![
            external_dep("p", DepSource::Path { path: "p".into() }),
            external_dep("q", DepSource::Path { path: "q".into() }),
        ]);
        let report = vendor_check(&model_new, &layout, &project).unwrap();
        assert!(report.fingerprint_mismatch);
        assert!(!report.is_clean());
    }

    #[test]
    fn encode_source_roundtrips_all_variants() {
        assert_eq!(
            encode_source_string(&DepSource::CratesIo {
                version: "1".into()
            }),
            "crates-io"
        );
        assert_eq!(
            encode_source_string(&DepSource::Git {
                url: "https://x/y.git".into(),
                reference: GitRef::Rev("abc".into())
            }),
            "git+https://x/y.git#rev=abc"
        );
        assert_eq!(
            encode_source_string(&DepSource::Git {
                url: "https://x/y.git".into(),
                reference: GitRef::Default
            }),
            "git+https://x/y.git#default"
        );
        assert_eq!(
            encode_source_string(&DepSource::Path {
                path: "../x".into()
            }),
            "path+../x"
        );
    }

    #[test]
    fn find_vendored_crate_dir_matches_by_package_name() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path();
        // Cargo uses `<name>-<version>` naming for crates.io sources.
        let d = vendor.join("bitflags-2.11.0");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("Cargo.toml"),
            b"[package]\nname=\"bitflags\"\nversion=\"2.11.0\"\nedition=\"2021\"\n",
        )
        .unwrap();

        let found = find_vendored_crate_dir(vendor, "bitflags").unwrap();
        assert_eq!(found, Some(d));

        let missing = find_vendored_crate_dir(vendor, "nope").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn resolve_path_dep_handles_absolute_and_relative() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            resolve_path_dep(root, "vendor/foo"),
            PathBuf::from("/tmp/project/vendor/foo")
        );
        assert_eq!(
            resolve_path_dep(root, "/abs/dep"),
            PathBuf::from("/abs/dep")
        );
    }

    // ------------------------------------------------------------------
    // auto_register_vendored_deps
    // ------------------------------------------------------------------

    fn setup_path_only_project(tmp: &Path) -> (PathBuf, BuildLayout, BuildModel) {
        let project = tmp.to_path_buf();
        let layout = minimal_layout(&project);
        std::fs::create_dir_all(layout.root()).unwrap();

        let local_dir = project.join("helper");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("Cargo.toml"),
            b"[package]\nname=\"helper\"\nversion=\"0.7.1\"\nedition=\"2021\"\n",
        )
        .unwrap();

        let model = model_with_deps(vec![external_dep(
            "helper",
            DepSource::Path {
                path: "helper".into(),
            },
        )]);
        (project, layout, model)
    }

    #[test]
    fn auto_register_is_noop_on_empty_model_with_no_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = minimal_layout(tmp.path());
        let mut model = BuildModel::default();
        auto_register_vendored_deps(&mut model, &layout, tmp.path()).unwrap();
        assert_eq!(model.crates.len(), 0);
        assert_eq!(model.groups.len(), 0);
    }

    #[test]
    fn auto_register_errors_when_deps_declared_but_no_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);

        let mut model = model_with_deps(vec![external_dep(
            "log",
            DepSource::CratesIo {
                version: "0.4".into(),
            },
        )]);
        let err = auto_register_vendored_deps(&mut model, &layout, &project).expect_err("fail");
        let msg = err.to_string();
        assert!(msg.contains("no gluon.lock was found"), "msg: {msg}");
        assert!(msg.contains("run `gluon vendor`"), "msg: {msg}");
    }

    #[test]
    fn auto_register_errors_on_fingerprint_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, layout, mut model) = setup_path_only_project(tmp.path());
        vendor_sync(&model, &layout, &project, VendorOptions::default()).unwrap();

        // Mutate model so fingerprint no longer matches.
        let local_q = project.join("q");
        std::fs::create_dir_all(&local_q).unwrap();
        std::fs::write(
            local_q.join("Cargo.toml"),
            b"[package]\nname=\"q\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        model.external_deps.insert(
            "q".into(),
            external_dep("q", DepSource::Path { path: "q".into() }),
        );

        let err = auto_register_vendored_deps(&mut model, &layout, &project).expect_err("fail");
        assert!(err.to_string().contains("stale"), "{err}");
    }

    // Path deps don't go through cargo build in v1, so the artifact
    // map stays empty for path-only models. Consumer crates that
    // actually try to use the dep at compile time will surface the
    // gap with a clearer "no artifact for X" diagnostic — see
    // `command_builder.rs`. Tracking this separately so the empty-map
    // case doesn't get conflated with a bug.
    #[test]
    fn auto_register_path_only_leaves_artifact_map_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, layout, mut model) = setup_path_only_project(tmp.path());
        vendor_sync(&model, &layout, &project, VendorOptions::default()).unwrap();

        auto_register_vendored_deps(&mut model, &layout, &project).unwrap();

        assert!(
            model.external_artifacts.is_empty(),
            "v1 doesn't compile path deps, so no artifact_path is set: {:?}",
            model.external_artifacts,
        );
    }

    // The artifact-population happy path. Synthesise a lockfile with a
    // crates-io-style entry whose `artifact_path` points at a real file
    // on disk; auto_register must copy the path into
    // model.external_artifacts under the user-declared name.
    #[test]
    fn auto_register_populates_external_artifacts_from_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);

        // Materialise a fake artifact on disk so the existence check
        // doesn't trip.
        let fake_artifact = project.join("fake-libsyn.rlib");
        std::fs::write(&fake_artifact, b"<not a real rlib>").unwrap();

        let mut model = model_with_deps(vec![external_dep(
            "syn",
            DepSource::CratesIo {
                version: "2.0".into(),
            },
        )]);
        let fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
        let lock = VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint,
            packages: vec![VendorLockPackage {
                name: "syn".into(),
                version: "2.0.117".into(),
                source: "crates-io".into(),
                checksum: Some("sha256:x".into()),
                artifact_path: Some(fake_artifact.clone()),
            }],
        };
        let lock_path = layout.gluon_lock(&project);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        lock.save_atomic(&lock_path).unwrap();

        auto_register_vendored_deps(&mut model, &layout, &project).unwrap();

        assert_eq!(model.external_artifacts.len(), 1);
        assert_eq!(
            model.external_artifacts.get(&ExternalArtifactKey {
                target: gluon_model::TargetSpec::Host,
                cargo_target_name: "syn".into(),
            }),
            Some(&fake_artifact),
        );
    }

    // Old lockfile (no artifact_path) → diagnostic. The auto_register
    // must not silently soldier on with an empty map; the consumer's
    // compile would then fail with a confusing "missing extern" error.
    #[test]
    fn auto_register_errors_when_artifact_path_missing_from_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);

        let mut model = model_with_deps(vec![external_dep(
            "syn",
            DepSource::CratesIo {
                version: "2.0".into(),
            },
        )]);
        let fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
        let lock = VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint,
            packages: vec![VendorLockPackage {
                name: "syn".into(),
                version: "2.0.117".into(),
                source: "crates-io".into(),
                checksum: Some("sha256:x".into()),
                artifact_path: None,
            }],
        };
        let lock_path = layout.gluon_lock(&project);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        lock.save_atomic(&lock_path).unwrap();

        let err = auto_register_vendored_deps(&mut model, &layout, &project).expect_err("fail");
        let msg = err.to_string();
        assert!(msg.contains("no artifact_path"), "msg: {msg}");
        assert!(msg.contains("gluon vendor"), "msg: {msg}");
    }

    // The recorded path was wiped (e.g. user ran `rm -rf build/` or
    // similar). Distinct error from the missing-field case so the
    // diagnostic can suggest the right fix.
    #[test]
    fn auto_register_errors_when_artifact_file_missing_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);

        let mut model = model_with_deps(vec![external_dep(
            "syn",
            DepSource::CratesIo {
                version: "2.0".into(),
            },
        )]);
        let fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
        let lock = VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint,
            packages: vec![VendorLockPackage {
                name: "syn".into(),
                version: "2.0.117".into(),
                source: "crates-io".into(),
                checksum: Some("sha256:x".into()),
                artifact_path: Some(project.join("does-not-exist.rlib")),
            }],
        };
        let lock_path = layout.gluon_lock(&project);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        lock.save_atomic(&lock_path).unwrap();

        let err = auto_register_vendored_deps(&mut model, &layout, &project).expect_err("fail");
        let msg = err.to_string();
        assert!(msg.contains("missing artifact"), "msg: {msg}");
        assert!(msg.contains("gluon vendor"), "msg: {msg}");
    }

    // Defensive call pattern: callers may auto_register on every model
    // load. Re-running must produce the same artifact map.
    #[test]
    fn auto_register_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().to_path_buf();
        let layout = minimal_layout(&project);
        let fake_artifact = project.join("fake-libsyn.rlib");
        std::fs::write(&fake_artifact, b"x").unwrap();

        let mut model = model_with_deps(vec![external_dep(
            "syn",
            DepSource::CratesIo {
                version: "2.0".into(),
            },
        )]);
        let fingerprint = fingerprint::fingerprint_external_deps(&model.external_deps);
        let lock = VendorLock {
            version: VendorLock::CURRENT_VERSION,
            fingerprint,
            packages: vec![VendorLockPackage {
                name: "syn".into(),
                version: "2.0.117".into(),
                source: "crates-io".into(),
                checksum: Some("sha256:x".into()),
                artifact_path: Some(fake_artifact.clone()),
            }],
        };
        let lock_path = layout.gluon_lock(&project);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        lock.save_atomic(&lock_path).unwrap();

        auto_register_vendored_deps(&mut model, &layout, &project).unwrap();
        let before = model.external_artifacts.clone();
        auto_register_vendored_deps(&mut model, &layout, &project).unwrap();
        assert_eq!(model.external_artifacts, before);
    }
}
