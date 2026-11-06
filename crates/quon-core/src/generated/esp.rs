//! ESP assembly helper.
//!
//! An [`EspDef`] declares a list of `(source, dest_path)` pairs. A
//! source is a crate's primary output, a host file, or a pinned
//! download (fetched here through [`crate::download`]). At build time,
//! after every referenced source crate has compiled, the `EspBuild`
//! scheduler node calls [`ensure_esp`] to copy each source into the
//! configured destination inside the ESP directory.
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
//! Each entry has its own fingerprint record, keyed
//! `esp:<target>/<profile>:<esp_name>:<dest_path>`. The ESP directory
//! is per target and profile, so is the key: building two profiles in
//! turn (as `quon test` does for boot tests) finds each copy fresh. The declared input is the
//! source file; the command hashes the ESP name, dest path and source
//! kind tag, so a crate→file switch in `quon.rs` invalidates. A host
//! file's content is part of its declared-input fingerprint, so an
//! edited `limine.conf` is copied again.
//! The helper checks every entry, copies the stale ones, and records
//! each one after its copy.

use crate::cache::{Freshness, FreshnessQuery, RecordInput, hash_parts};
use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use quon_model::{BuildModel, EspDef, EspEntrySource, Handle, ResolvedConfig};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use crate::compile::extern_map::ArtifactLookup;

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
    artifacts: &dyn ArtifactLookup,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
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
    let Some(target) = profile.cross_target().and_then(|t| model.targets.get(t)) else {
        return Err(Error::Compile(format!(
            "esp '{}': profile '{}' builds for the host, and an ESP is assembled for the \
             profile's target; build it with a profile that names one",
            esp.name, profile.name
        )));
    };
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
        /// Folded into the fingerprint command; see the module docs.
        kind_tag: &'static [u8],
    }
    let mut entries: Vec<ResolvedEntry<'_>> = Vec::with_capacity(esp.entries.len());
    for entry in &esp.entries {
        let (source_path, source_name, kind_tag) = match &entry.source {
            EspEntrySource::Crate(h) => {
                let source_name = model
                    .crates
                    .get(*h)
                    .map(|c| c.name.as_str())
                    .unwrap_or("<unknown>");
                let art = artifacts.get(*h).ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: EspBuild('{}') source crate '{}' has no entry in \
                         ArtifactMap (DAG edge missing or ran out of order?)",
                        esp.name, source_name
                    ))
                })?;
                (art.to_path_buf(), source_name, &b"crate"[..])
            }
            EspEntrySource::File(path) => {
                let abs = resolved.project_root.join(path);
                if !abs.is_file() {
                    return Err(Error::Compile(format!(
                        "Esp::new(\"{}\").add(\"{}\", \"{}\"): host file does not exist \
                         at {} (paths are relative to the project root)",
                        esp.name,
                        path,
                        entry.dest_path,
                        abs.display()
                    )));
                }
                (abs, path.as_str(), &b"file"[..])
            }
            EspEntrySource::Download(h) => {
                let def = model.downloads.get(*h).ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: EspBuild('{}') download handle {h:?} not in model.downloads",
                        esp.name
                    ))
                })?;
                let (path, fetched) = crate::download::ensure_download(&ctx.layout, def)?;
                if fetched {
                    let _ = writeln!(stdout, "  Downloaded {} ({})", def.name, def.url);
                }
                (path, def.name.as_str(), &b"download"[..])
            }
        };
        entries.push(ResolvedEntry {
            dest_path: &entry.dest_path,
            source_path,
            source_name,
            kind_tag,
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
    let toolchain = ctx.toolchain();
    let no_env = BTreeMap::new();

    for e in &entries {
        let dest_abs = esp_dir.join(e.dest_path);
        let key = format!(
            "esp:{}/{}:{}:{}",
            target.name, profile.name, esp.name, e.dest_path
        );
        // The command folds in the ESP name, dest path and source kind
        // tag, so a crate→file switch in quon.rs invalidates even if
        // the on-disk source path happens to coincide.
        let command = hash_parts(
            "quon.esp-entry.v1",
            &[esp.name.as_bytes(), e.dest_path.as_bytes(), e.kind_tag],
        );
        let declared = [e.source_path.clone()];
        let outputs = [dest_abs.clone()];
        let query = FreshnessQuery {
            key: &key,
            command,
            toolchain,
            declared: &declared,
            outputs: &outputs,
            command_env: &no_env,
        };
        match ctx.store.check(&query) {
            Freshness::Fresh(_) => continue,
            Freshness::Stale(reason) => ctx.report_stale(
                stderr,
                &format!("esp {} entry {}", esp.name, e.dest_path),
                &reason,
            ),
        }

        if let Some(parent) = dest_abs.parent() {
            std::fs::create_dir_all(parent).map_err(|err| Error::Io {
                path: parent.to_path_buf(),
                source: err,
            })?;
        }
        let started_at = std::time::SystemTime::now();
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
        // Each entry's record is written right after its copy, so a
        // failure on a later entry keeps the earlier ones.
        ctx.store.record(RecordInput {
            key: &key,
            command,
            toolchain,
            declared: &declared,
            discovered: &[],
            env: BTreeMap::new(),
            outputs: &outputs,
            started_at,
            stderr: &[],
            data: None,
        })?;
    }

    // If nothing changed, report this as a cache hit so the summary
    // counters stay accurate.
    Ok((esp_dir, copied == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{ArtifactMap, BuildLayout, RustcInfo};
    use quon_model::{
        BuildModel, CrateDef, CrateType, EspEntry, EspEntrySource, GroupDef, ProjectDef,
        ResolvedConfig, ResolvedProfile, TargetDef, TargetSpec,
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
        CompileCtx::new(layout, Arc::new(fake_rustc_info()))
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
                build_std: false,
                panic_strategy: None,
                rustc_flags: Vec::new(),
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
            cargo: None,
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
                        source: EspEntrySource::Crate(ch_a),
                        dest_path: "EFI/BOOT/A.EFI".into(),
                    },
                    EspEntry {
                        source: EspEntrySource::Crate(ch_b),
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
                target: TargetSpec::Cross(th),
                opt_level: quon_model::OptLevel::Zero,
                debug_info: false,
                debug_assertions: true,
                overflow_checks: true,
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
                build_std: false,
                panic_strategy: None,
                rustc_flags: Vec::new(),
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
                cargo: None,
            },
        );
        let (eh, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source: EspEntrySource::Crate(ch),
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
                target: TargetSpec::Cross(th),
                opt_level: quon_model::OptLevel::Zero,
                debug_info: false,
                debug_assertions: true,
                overflow_checks: true,
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
        let (esp_dir, was_cached) = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(!was_cached, "cold run must not report cached");
        let dest = esp_dir.join("EFI/BOOT/BOOTX64.EFI");
        assert!(dest.exists(), "dest file must exist: {}", dest.display());
        let dest_bytes = std::fs::read(&dest).unwrap();
        assert_eq!(dest_bytes, b"EFI\x00bootloader bytes");
        // Each entry has its own record, not a sidecar stamp file.
        assert!(
            ctx.store
                .record_path("esp:x86_64-uefi/dev:default:EFI/BOOT/BOOTX64.EFI")
                .exists(),
            "the ESP entry must have its own record"
        );
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
        let (esp_dir, _) = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();

        // Mutate source content AND bump mtime. Filesystems can report
        // the same mtime for two writes within the same second, so we
        // also change the file size — the stamp keys on both.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&source_path, b"v2-with-more-bytes").unwrap();

        let (_, cached) = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
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
        let (esp_dir, _) = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();

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
        let (esp_dir, cold_cached) = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
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

        let (_, second_cached) = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
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
        let err = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("ArtifactMap") || msg.contains("DAG edge"),
            "error must mention the missing-artifact condition: {msg}"
        );
    }

    #[test]
    fn ensure_esp_copies_host_file_and_recopies_after_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_path = tmp.path().join("bootloader.efi");
        std::fs::write(&source_path, b"efi").unwrap();
        std::fs::write(tmp.path().join("limine.conf"), b"timeout: 0\n").unwrap();

        let (mut model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(model.crates.lookup("bootloader").unwrap(), source_path);
        model.esps.get_mut(eh).unwrap().entries.push(EspEntry {
            source: EspEntrySource::File("limine.conf".into()),
            dest_path: "boot/limine.conf".into(),
        });

        let run = || {
            ensure_esp(
                &ctx,
                &model,
                &resolved,
                eh,
                &artifacts,
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap()
        };
        let (esp_dir, _) = run();
        let dest = esp_dir.join("boot/limine.conf");
        assert_eq!(std::fs::read(&dest).unwrap(), b"timeout: 0\n");
        assert!(run().1, "unchanged inputs must be cached");

        std::fs::write(
            tmp.path().join("limine.conf"),
            b"timeout: 5\nverbose: yes\n",
        )
        .unwrap();
        assert!(!run().1, "an edited host file must be copied again");
        assert_eq!(std::fs::read(&dest).unwrap(), b"timeout: 5\nverbose: yes\n");
    }

    #[test]
    fn ensure_esp_missing_host_file_names_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_path = tmp.path().join("bootloader.efi");
        std::fs::write(&source_path, b"efi").unwrap();
        let (mut model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(model.crates.lookup("bootloader").unwrap(), source_path);
        model.esps.get_mut(eh).unwrap().entries.push(EspEntry {
            source: EspEntrySource::File("missing.conf".into()),
            dest_path: "missing.conf".into(),
        });
        let err = ensure_esp(
            &ctx,
            &model,
            &resolved,
            eh,
            &artifacts,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(".add(\"missing.conf\""), "{err}");
    }
}
