//! ESP assembly helper.
//!
//! An [`EspDef`] declares a list of `(source_crate, dest_path)` pairs. At
//! build time, after every referenced source crate has compiled, the
//! `EspBuild` scheduler node calls [`ensure_esp`] to copy each source
//! artifact into the configured destination inside the ESP directory.
//!
//! ### Output layout
//!
//! The ESP directory lives at
//! `<build>/cross/<target>/<profile>/esp/<name>/`, where `<target>` is
//! the profile's cross target. Each entry's `dest_path` is joined into
//! that directory, creating intermediate directories as needed. The
//! result is a valid on-disk FAT layout that QEMU's VVFAT driver can
//! mount via `-drive format=raw,file=fat:rw:<dir>`.
//!
//! ### Freshness
//!
//! Each `(esp_name, dest_path)` pair is keyed in the unified cache
//! as `"esp:<esp_name>:<dest_path>"`. Inputs are the single source
//! artefact's file fingerprint; aux_hash folds in the esp name,
//! dest path, and source kind tag so a crate→file rename in
//! `gluon.rs` would still invalidate. The helper walks every entry,
//! calls `Cache::is_composite_fresh` per entry, copies the stale
//! ones, and marks each Composite built. Same syscall budget as the
//! old per-entry stamp file; same incremental-copy granularity.

use crate::cache::{CompositeQuery, CompositeRecord, composite_aux_hash};
use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use gluon_model::{BuildModel, EspDef, Handle, ResolvedConfig};
use std::io::Write;
use std::path::PathBuf;

use crate::compile::ArtifactMap;

/// Assemble the ESP directory for one [`EspDef`], returning the
/// directory path and a `was_cached` flag indicating whether the
/// previous run's output was reused as-is.
///
/// Caller invariants:
/// - The `EspBuild` DAG node guarantees every `source_crate` handle in
///   `esp.entries` has already been compiled, so `artifacts.get(h)`
///   must return `Some` for every entry. A missing entry is a scheduler
///   bug and produces a clear error.
/// - Entries whose source crate did not resolve at intern time were
///   already dropped with a diagnostic, so every entry here carries a
///   live handle.
pub fn ensure_esp(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    esp_handle: Handle<EspDef>,
    artifacts: &ArtifactMap,
    stdout: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let esp = model.esps.get(esp_handle).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: ESP handle {:?} not found in model.esps",
            esp_handle
        ))
    })?;

    // The ESP directory lives under the profile's cross target, even
    // though individual source crates may target something different
    // (e.g. bootloader = x86_64-unknown-uefi, kernel = x86_64-unknown-none).
    // Keying by profile.target keeps the output path stable.
    let profile = &resolved.profile;
    let target = model.targets.get(profile.target).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: profile '{}' has target handle {:?} not in model.targets",
            profile.name, profile.target
        ))
    })?;
    let esp_dir = ctx.layout.esp_dir(target, profile, &esp.name);

    // Resolve every entry to its built artifact. Intern has already
    // dropped any dangling references, so each entry's `source_crate`
    // handle is live; a missing artifact here is a scheduler bug
    // (DAG edge missing or nodes ran out of order) and we surface
    // it loudly rather than silently producing a partial ESP.
    struct ResolvedEntry<'a> {
        dest_path: &'a str,
        source_path: PathBuf,
        source_name: &'a str,
    }
    let mut entries: Vec<ResolvedEntry<'_>> = Vec::with_capacity(esp.entries.len());
    for entry in &esp.entries {
        let h = entry.source_crate;
        let source_name = model
            .crates
            .get(h)
            .map(|c| c.name.as_str())
            .unwrap_or("<unknown>");
        let art = artifacts.get(h).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: EspBuild('{}') source crate '{}' has no entry in \
                 ArtifactMap (DAG edge missing or ran out of order?)",
                esp.name, source_name
            ))
        })?;
        entries.push(ResolvedEntry {
            dest_path: &entry.dest_path,
            source_path: art.to_path_buf(),
            source_name,
        });
    }

    // Ensure the ESP root exists up-front. Both the per-entry copy and
    // the cache record live underneath it; failing here means the
    // filesystem is uncooperative and there's nothing further to try.
    std::fs::create_dir_all(&esp_dir).map_err(|e| Error::Io {
        path: esp_dir.clone(),
        source: e,
    })?;

    let mut copied = 0usize;

    for e in &entries {
        let dest_abs = esp_dir.join(e.dest_path);
        let cache_key = format!("esp:{}:{}", esp.name, e.dest_path);
        // aux_hash folds in the esp name, dest path, and source kind
        // tag so a crate→file rename in gluon.rs would still invalidate
        // even if the on-disk source path happened to coincide. ESP
        // entries are crate-sourced today; if other source kinds appear,
        // derive a per-source-kind tag here.
        let aux = composite_aux_hash(&[esp.name.as_bytes(), e.dest_path.as_bytes(), b"crate"]);
        let inputs = vec![e.source_path.clone()];

        // Take the cache mutex only for the freshness query; release
        // before any I/O so a slow copy can't starve other scheduler
        // threads that need cache access.
        {
            let mut cache = ctx
                .cache
                .lock()
                .map_err(|_| Error::Compile("cache mutex poisoned".into()))?;
            let q = CompositeQuery {
                key: &cache_key,
                inputs: &inputs,
                aux_hash: Some(aux),
                output_path: &dest_abs,
            };
            if cache.is_composite_fresh(&q) {
                continue;
            }
        }

        if let Some(parent) = dest_abs.parent() {
            std::fs::create_dir_all(parent).map_err(|err| Error::Io {
                path: parent.to_path_buf(),
                source: err,
            })?;
        }
        std::fs::copy(&e.source_path, &dest_abs).map_err(|err| Error::Io {
            path: e.source_path.clone(),
            source: err,
        })?;
        copied += 1;
        let _ = writeln!(
            stdout,
            "    Copied {} -> esp/{}/{}",
            e.source_name, esp.name, e.dest_path
        );

        // Mark this entry built (in-memory only). The single
        // `cache.save()` after the loop persists everything atomically;
        // a mid-loop crash forfeits the run, mirroring the rustc-half
        // all-or-nothing semantics. The per-entry mark vs deferred
        // save split keeps the per-entry path cheap (no fsync per copy)
        // while preserving the "stamp written after the work" invariant
        // at the granularity of the whole `ensure_esp` call.
        {
            let mut cache = ctx
                .cache
                .lock()
                .map_err(|_| Error::Compile("cache mutex poisoned".into()))?;
            cache.mark_composite_built(CompositeRecord {
                key: cache_key,
                inputs,
                aux_hash: Some(aux),
                output_path: dest_abs,
            })?;
        }
    }

    // One save at the end — atomic-write semantics make this cheap.
    {
        let mut cache = ctx
            .cache
            .lock()
            .map_err(|_| Error::Compile("cache mutex poisoned".into()))?;
        cache.save()?;
    }

    // If nothing changed, report this as a cache hit so the summary
    // counters stay accurate.
    Ok((esp_dir, copied == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::compile::{BuildLayout, RustcInfo};
    use gluon_model::{
        BuildModel, CrateDef, CrateType, EspEntry, GroupDef, ProjectDef, ResolvedConfig,
        ResolvedProfile, TargetDef, TargetSpec,
    };
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;

    fn fake_rustc_info() -> RustcInfo {
        RustcInfo {
            rustc_path: PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    fn make_ctx(tmp: &Path) -> CompileCtx {
        let layout = BuildLayout::new(tmp.join("build"), "t");
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        let cache = Cache::load(tmp.join("build/cache-manifest.json")).0;
        CompileCtx::new(layout, Arc::new(fake_rustc_info()), cache)
    }

    fn make_model_with_esp_two_entries(
        tmp: &Path,
        source_a: &Path,
        source_b: &Path,
    ) -> (BuildModel, ResolvedConfig, Handle<EspDef>) {
        let mut model = BuildModel::default();
        let (th, _) = model.targets.insert(
            "x86_64-uefi".into(),
            TargetDef {
                name: "x86_64-uefi".into(),
                spec: "x86_64-unknown-uefi".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let (group_h, _) = model.groups.insert(
            "uefi".into(),
            GroupDef {
                name: "uefi".into(),
                target: TargetSpec::Cross(th),
                ..Default::default()
            },
        );
        let mk_crate = |name: &str| CrateDef {
            name: name.into(),
            path: format!("crates/{name}"),
            edition: "2021".into(),
            crate_type: CrateType::Bin,
            target: TargetSpec::Cross(th),
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            features: Vec::new(),
            root: None,
            linker_script: None,
            group: group_h,
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        };
        let (ch_a, _) = model
            .crates
            .insert("bootloader-a".into(), mk_crate("bootloader-a"));
        let (ch_b, _) = model
            .crates
            .insert("bootloader-b".into(), mk_crate("bootloader-b"));
        let (eh, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![
                    EspEntry {
                        source_crate: ch_a,
                        dest_path: "EFI/BOOT/A.EFI".into(),
                    },
                    EspEntry {
                        source_crate: ch_b,
                        dest_path: "EFI/BOOT/B.EFI".into(),
                    },
                ],
                span: None,
            },
        );

        let _ = (source_a, source_b);

        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "t".into(),
                version: "0.1.0".into(),
                ..Default::default()
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: th,
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
            crates: Vec::new(),
            build_dir: tmp.join("build"),
            project_root: tmp.to_path_buf(),
        };

        (model, resolved, eh)
    }

    fn make_model_with_esp(
        tmp: &Path,
        source_artifact: &Path,
    ) -> (BuildModel, ResolvedConfig, Handle<EspDef>) {
        let mut model = BuildModel::default();
        let (th, _) = model.targets.insert(
            "x86_64-uefi".into(),
            TargetDef {
                name: "x86_64-uefi".into(),
                spec: "x86_64-unknown-uefi".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let (group_h, _) = model.groups.insert(
            "uefi".into(),
            GroupDef {
                name: "uefi".into(),
                target: TargetSpec::Cross(th),
                ..Default::default()
            },
        );
        let (ch, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                deps: BTreeMap::new(),
                dev_deps: BTreeMap::new(),
                features: Vec::new(),
                root: None,
                linker_script: None,
                group: group_h,
                is_project_crate: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                requires_config: Vec::new(),
                artifact_deps: Vec::new(),
                artifact_env: BTreeMap::new(),
                span: None,
            },
        );
        let (eh, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source_crate: ch,
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                span: None,
            },
        );

        let _ = source_artifact;

        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "t".into(),
                version: "0.1.0".into(),
                ..Default::default()
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: th,
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
            crates: Vec::new(),
            build_dir: tmp.join("build"),
            project_root: tmp.to_path_buf(),
        };

        (model, resolved, eh)
    }

    #[test]
    fn ensure_esp_cold_run_copies_source_artifact_to_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());

        // Create a source "bootloader binary" on disk.
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"EFI\x00bootloader bytes").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);

        // Populate ArtifactMap: resolve bootloader's handle to our fake
        // source path.
        let mut artifacts = ArtifactMap::new();
        let bl_h = model.crates.lookup("bootloader").unwrap();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        let (esp_dir, was_cached) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();

        assert!(!was_cached, "cold run must not report cached");
        let dest = esp_dir.join("EFI/BOOT/BOOTX64.EFI");
        assert!(dest.exists(), "dest file must exist: {}", dest.display());
        let dest_bytes = std::fs::read(&dest).unwrap();
        assert_eq!(dest_bytes, b"EFI\x00bootloader bytes");
        // Freshness lives in the unified cache manifest (per-entry
        // Composite key `esp:<esp_name>:<dest_path>`), not in a sidecar
        // stamp file.
        let cache = ctx.cache.lock().unwrap();
        let keys: Vec<&String> = cache.composite_keys().collect();
        assert!(
            keys.iter()
                .any(|k| k.as_str() == "esp:default:EFI/BOOT/BOOTX64.EFI"),
            "composite manifest must contain the per-entry ESP key; got {keys:?}"
        );
    }

    #[test]
    fn ensure_esp_second_run_is_cached_when_source_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"bytes").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let bl_h = model.crates.lookup("bootloader").unwrap();
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        // First run: cold
        let (_dir, cached1) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(!cached1);

        // Second run: source unchanged → cached
        let (_dir, cached2) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(cached2, "second run with unchanged source must be cached");
    }

    #[test]
    fn ensure_esp_recopies_when_source_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"v1").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let bl_h = model.crates.lookup("bootloader").unwrap();
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        let (esp_dir, _) = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();

        // Mutate source content AND bump mtime. Filesystems can report
        // the same mtime for two writes within the same second, so we
        // also change the file size — the stamp keys on both.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&source_path, b"v2-with-more-bytes").unwrap();

        let (_, cached) = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(!cached, "changed source must trigger a re-copy");
        let dest_bytes = std::fs::read(esp_dir.join("EFI/BOOT/BOOTX64.EFI")).unwrap();
        assert_eq!(dest_bytes, b"v2-with-more-bytes");
    }

    #[test]
    fn ensure_esp_writes_no_stamp_json_file() {
        // After Stage 5, ESP freshness lives in the cache manifest
        // as per-entry Composite entries — no sidecar stamp file
        // should be produced.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"bytes").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let bl_h = model.crates.lookup("bootloader").unwrap();
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        let (esp_dir, _) = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();

        let stamp = esp_dir.join(".esp-stamp.json");
        assert!(
            !stamp.exists(),
            "ensure_esp must not write {}; that data lives in the cache manifest now",
            stamp.display()
        );
    }

    #[test]
    fn ensure_esp_re_copies_only_changed_entry() {
        // Two-entry ESP: change one source, assert the other entry's
        // dest file is left untouched (not re-copied). This pins the
        // per-entry incremental-copy behaviour the granular-Composite
        // design is meant to preserve.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let src_a = source_dir.join("a.efi");
        let src_b = source_dir.join("b.efi");
        std::fs::write(&src_a, b"alpha-1").unwrap();
        std::fs::write(&src_b, b"beta-1").unwrap();

        let (model, resolved, eh) = make_model_with_esp_two_entries(tmp.path(), &src_a, &src_b);
        let h_a = model.crates.lookup("bootloader-a").unwrap();
        let h_b = model.crates.lookup("bootloader-b").unwrap();
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(h_a, src_a.clone());
        artifacts.insert(h_b, src_b.clone());

        let mut out = Vec::<u8>::new();
        // Cold run.
        let (esp_dir, cold_cached) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(!cold_cached);

        // Capture dest_b's mtime before the second run.
        let dest_b = esp_dir.join("EFI/BOOT/B.EFI");
        let mtime_before = std::fs::metadata(&dest_b)
            .expect("stat dest_b")
            .modified()
            .expect("mtime");

        // Mutate src_a only (and bump size to defeat the fast path).
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&src_a, b"alpha-2-with-more-bytes").unwrap();

        let (_, second_cached) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(
            !second_cached,
            "esp must report not-cached when one entry changed"
        );

        let dest_a_bytes = std::fs::read(esp_dir.join("EFI/BOOT/A.EFI")).unwrap();
        assert_eq!(dest_a_bytes, b"alpha-2-with-more-bytes");

        // dest_b's mtime should not have advanced — it was not re-copied.
        let mtime_after = std::fs::metadata(&dest_b)
            .expect("stat dest_b")
            .modified()
            .expect("mtime");
        assert_eq!(
            mtime_before, mtime_after,
            "dest_b must not be re-copied when only src_a changed (incremental copy lost)"
        );
    }

    #[test]
    fn ensure_esp_missing_source_handle_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_path = tmp.path().join("bootloader.efi");
        std::fs::write(&source_path, b"x").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        // EMPTY artifact map — simulates a scheduler bug where the
        // bootloader node didn't run first.
        let artifacts = ArtifactMap::new();

        let mut out = Vec::<u8>::new();
        let err = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("ArtifactMap") || msg.contains("DAG edge"),
            "error must mention the missing-artifact condition: {msg}"
        );
    }
}
