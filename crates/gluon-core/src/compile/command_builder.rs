//! Rustc command assembly for per-crate compilation.
//!
//! This module contains `build_rustc_command`, which assembles a
//! [`RustcCommandBuilder`] for a single user crate without spawning rustc.
//! It was extracted from `compile_crate.rs` to keep that module focused on
//! cache integration and process management.

use super::artifact_shape::CrateArtifactShape;
use super::compile_crate::CompileCrateInput;
use super::compile_utils::{normalize_crate_name, sanitise_crate_name};
use super::{CompileCtx, Emit, RustcCommandBuilder};
use crate::error::{Error, Result};
use gluon_model::{CrateDef, CrateType, DepResolution};
use std::path::PathBuf;

/// Assemble a [`RustcCommandBuilder`] for the given crate without spawning
/// rustc. Also returns the expected output artifact path and the depfile
/// path.
///
/// Exposed as `pub(crate)` so tests can inspect the assembled flags without
/// needing a real rustc binary. Production code calls [`compile`], which
/// wraps this with cache checking and process spawning.
///
/// Returns `(builder, output_path, depfile_path)`.
pub(crate) fn build_rustc_command(
    ctx: &CompileCtx,
    input: &CompileCrateInput<'_>,
) -> Result<(RustcCommandBuilder, PathBuf, PathBuf)> {
    let layout = &ctx.layout;
    let model = input.model;
    let resolved = input.resolved;
    let crate_ref = input.crate_ref;
    let artifacts = input.artifacts;

    let crate_def: &CrateDef = model.crates.get(crate_ref.handle).ok_or_else(|| {
        Error::Compile(format!(
            "crate handle {:?} not found in build model",
            crate_ref.handle
        ))
    })?;

    let crate_name = &crate_def.name;
    // rustc rejects `-` in `--crate-name`. Cargo silently normalizes
    // `my-crate` → `my_crate`; we mirror that so cargo-style names in
    // `gluon.rs` Just Work. The original `crate_name` is kept for error
    // messages and cache keys (it's what the user typed). The normalized
    // form (`crate_name_rustc`) is what we hand to rustc and what we
    // splice into on-disk filenames so they line up with what rustc
    // actually writes (rustc derives the filename from `--crate-name` +
    // `extra-filename`). The post-normalization collision check (rejecting
    // e.g. both `foo-bar` and `foo_bar`) is done in `gluon_model::intern`.
    let crate_name_rustc = normalize_crate_name(crate_name);

    // Resolve the source input file. `CrateDef::path` is a directory path
    // relative to the project root; `CrateDef::root` optionally overrides
    // the entry-point file inside that directory.
    let project_root = &resolved.project_root;
    let crate_dir = project_root.join(&crate_def.path);
    let src_file = if let Some(root_override) = &crate_def.root {
        crate_dir.join(root_override)
    } else if crate_def.crate_type.is_executable() {
        crate_dir.join("src/main.rs")
    } else {
        crate_dir.join("src/lib.rs")
    };

    // Determine the output directory and artifact paths via
    // [`CrateArtifactShape`] — the single source of truth for crate
    // path arithmetic, also consumed by [`crate::compile::artifact_path`]
    // for analyzer-time lookups. Keep `extra_filename` available
    // locally; it is still emitted as `-C extra-filename=...` further
    // down for non-executable / host crates.
    //
    // Why bins skip extra-filename: rustc never hash-suffixes binary
    // names, and adding extra-filename produces an ugly
    // `hello-gluon-hello` final binary. The shape constructor encodes
    // the same exception (cross Bin / RawBinary use `cross_final_dir`
    // with bare crate names); the `-C extra-filename` emit below is
    // gated on `!crate_type.is_executable() || crate_ref.host`.
    let extra_filename = format!("-gluon-{crate_name_rustc}");

    let shape = CrateArtifactShape::for_crate(layout, model, resolved, crate_def, crate_ref)?;
    let out_dir = shape.out_dir;
    let output_path = shape.output_path;
    let depfile_path = shape.depfile_path;

    // --- Builder assembly ---
    //
    // The program path is selected by `ctx.driver`. For
    // `DriverKind::Rustc` (the historical default) this is just the
    // probed rustc path. For `Check` it is also rustc, but we will
    // override the `--emit` kinds below to suppress codegen. For
    // `Clippy` it resolves `clippy-driver` (env, sibling-of-rustc, or
    // PATH lookup), which is rustc-CLI-compatible so every flag below
    // applies unchanged.
    let program = ctx.driver().program(&ctx.rustc_info);
    let mut builder = RustcCommandBuilder::new(&program);

    // Input source file — positional arg, must come first.
    builder.input(&src_file);

    // Crate identity flags.
    builder
        .crate_name(&crate_name_rustc)
        .crate_type(crate_def.crate_type)
        .edition(&crate_def.edition);

    // Look up whether this crate's group has `no_sysroot=true`. Such
    // crates skip the custom sysroot, the implicit core/alloc/compiler_builtins
    // externs, and the auto-injected config crate. They still get
    // `--target` (the target spec is needed to compile correctly), but
    // are responsible for declaring `#![no_core]` (or equivalent) and
    // providing their own runtime primitives.
    let group_no_sysroot = model
        .groups
        .get(crate_def.group)
        .map(|g| g.no_sysroot)
        .unwrap_or(false);

    if crate_ref.host {
        // Host crates: no sysroot, no target. rustc uses its built-in sysroot.
        builder.out_dir(&out_dir);
    } else if group_no_sysroot {
        // No-sysroot cross crates: emit `--target` only. No sysroot,
        // no implicit externs, no config crate. The crate is on its
        // own — `#![no_core]` is mandatory.
        let target_handle = crate_ref.target;
        let target = model.targets.get(target_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, target_handle
            ))
        })?;
        builder
            .target(&target.spec, target.builtin)
            .out_dir(&out_dir);
    } else {
        // Cross crates: sysroot and target are required.
        let sysroot = input.sysroot_dir.ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' is a cross crate but no sysroot_dir was provided",
                crate_name
            ))
        })?;

        let target_handle = crate_ref.target;
        let target = model.targets.get(target_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, target_handle
            ))
        })?;

        builder
            .sysroot(sysroot)
            .target(&target.spec, target.builtin)
            .out_dir(&out_dir);

        // Inject implicit sysroot crates. These are the crates that rustc
        // normally injects from its own built-in sysroot; since we are
        // using a custom sysroot we must wire them explicitly. The names
        // must match what `build_sysroot_crate` writes (same extra-filename
        // convention: `-gluon-<name>`).
        let sysroot_lib = layout.sysroot_lib_dir(target);
        builder
            .extern_crate("core", &sysroot_lib.join("libcore-gluon-core.rlib"))
            .extern_crate(
                "compiler_builtins",
                &sysroot_lib.join("libcompiler_builtins-gluon-compiler_builtins.rlib"),
            )
            .extern_crate("alloc", &sysroot_lib.join("liballoc-gluon-alloc.rlib"));

        // Inject the generated config crate for this crate's target.
        // Each cross target has its own config crate rlib so there is
        // no target-mismatch risk.
        if let Some(config_path) = artifacts.config_crate(crate_ref.target) {
            // The config crate name is `<project>_config` by default, or the
            // override in `ProjectDef::config_crate_name`. We sanitise the
            // project name (lowercase, non-[a-z0-9_] replaced by `_`) and
            // append `_config`.
            let config_extern_name = resolved
                .project
                .config_crate_name
                .as_deref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| sanitise_crate_name(&resolved.project.name) + "_config");
            builder.extern_crate(&config_extern_name, config_path);
        }
    }

    // Emit flags. The depfile path is threaded in explicitly so rustc writes
    // it to a location we control, rather than deriving it from --out-dir +
    // crate name + extra-filename. This keeps the "where does the .d land?"
    // knowledge in one place (the `depfile_path` bound above).
    //
    // The driver may force a metadata-only emit (`Check` and `Clippy`
    // both do this) — in that case it returns `Some(slice)` from
    // `emit_override` and we use it instead of the per-crate-type
    // selection. The depfile is still emitted so the cache freshness
    // logic continues to track source dependencies; it is correct on a
    // metadata-only build because rustc still produces accurate
    // dep-info even when codegen is suppressed.
    //
    // **Proc-macros are exempt from the override.** A proc-macro crate
    // must produce a real dylib because the dependent crate's compile
    // pass dlopens it at compile time. Forcing `--emit=metadata` here
    // would write only an `.rmeta` and the next crate's rustc would
    // fail with "extern location for X does not exist". `cargo check`
    // has exactly the same behavior — it always builds proc-macros
    // fully, regardless of the check vs build distinction. We mirror
    // that.
    let default_emit: &[Emit] = if crate_def.crate_type.is_executable() {
        // Executables (Bin, RawBinary) don't produce metadata that other
        // crates would `--extern` against. Skipping `--emit=metadata`
        // saves a few hundred ms per binary build.
        &[Emit::Link, Emit::DepInfo]
    } else {
        &[Emit::Link, Emit::Metadata, Emit::DepInfo]
    };
    let emit_kinds: &[Emit] = if crate_def.crate_type == CrateType::ProcMacro {
        default_emit
    } else {
        ctx.driver().emit_override().unwrap_or(default_emit)
    };
    builder.emit_with_dep_info_path(emit_kinds, &depfile_path);

    // Explicit dependencies (BTreeMap order = deterministic argv order).
    //
    // Two flavours:
    //
    // - `DepResolution::Project(h)` — sibling project crates. Their
    //   artifacts come from `ArtifactMap`, populated by the scheduler
    //   as each gluon-managed compile completes.
    //
    // - `DepResolution::External(eh)` — vendored crates compiled by
    //   cargo via the cargo-as-builder vendor pipeline (see
    //   `vendor/cargo_cmd.rs::run_cargo_build_vendor` and
    //   `vendor/mod.rs::auto_register_vendored_deps`). Their artifact
    //   paths live in `model.external_artifacts`, populated at
    //   model-load time from `gluon.lock`. We look up the vendored
    //   artifact by the external dep's *crate name* (not the alias),
    //   matching how `auto_register` keys the map.
    //
    //   v1 limitation: vendored deps assume host-target compatibility.
    //   A cross-target consumer that pulls a host-built rlib would
    //   fail to link with an opaque rustc error. We don't pre-flight
    //   that case here yet — the lepton-os usage path is host-only
    //   (proc-macros and their deps), so the live failure mode is
    //   acceptable until v2 wires per-target vendored builds.
    let mut external_dep_dirs: std::collections::BTreeSet<PathBuf> = Default::default();
    for (extern_name, dep_def) in &crate_def.deps {
        let artifact_path: PathBuf = match dep_def.resolution {
            DepResolution::Project(h) => artifacts
                .get(h)
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "crate '{}' depends on '{}' but its artifact is not yet \
                         available in ArtifactMap (scheduler bug — dep must be \
                         built before depender)",
                        crate_name, extern_name,
                    ))
                })?
                .to_path_buf(),
            DepResolution::External(eh) => {
                let ext = model.external_deps.get(eh).ok_or_else(|| {
                    Error::Compile(format!(
                        "crate '{}': external dep handle for '{}' is dangling \
                         (intern bug)",
                        crate_name, extern_name,
                    ))
                })?;
                // Lookup key matches what `auto_register_vendored_deps`
                // wrote: the dep's declared target (Host for v1's
                // cargo-as-builder pipeline, Cross(_) for v2's
                // gluon-driven pipeline) plus the cargo-normalised
                // crate name (hyphens → underscores). The dep's
                // *target*, not the consumer's, is what disambiguates
                // — a kernel-target consumer that pulls a host-built
                // proc-macro still looks it up under `Host`.
                let key = gluon_model::ExternalArtifactKey {
                    target: ext.target,
                    cargo_target_name: ext.name.replace('-', "_"),
                };
                let path = model.external_artifacts.get(&key).ok_or_else(|| {
                    Error::Compile(format!(
                        "crate '{}' depends on external crate '{}' but no \
                         vendor artifact is registered for it — run \
                         `gluon vendor` to build the external dep set",
                        crate_name, ext.name,
                    ))
                })?;
                if let Some(parent) = path.parent() {
                    external_dep_dirs.insert(parent.to_path_buf());
                }
                if ext.target == gluon_model::TargetSpec::Host {
                    external_dep_dirs.extend(model.external_search_dirs.iter().cloned());
                }
                // Nightly cargo (1.101+) writes rlibs with only a
                // metadata stub and the real metadata in a sibling
                // `.rmeta`. rustc needs both, and cargo passes both
                // under the same extern name, so we do the same.
                // Stable cargo writes the `.rmeta` too; passing it is
                // harmless there.
                let rmeta = path.with_extension("rmeta");
                if path.extension().is_some_and(|e| e == "rlib") && rmeta.is_file() {
                    builder.extern_crate(extern_name, &rmeta);
                }
                path.clone()
            }
        };
        builder.extern_crate(extern_name, &artifact_path);
    }

    // Transitive vendored deps. Adding `-L dependency=<dir>` lets
    // rustc resolve a vendored crate's own deps (e.g. `unicode-ident`
    // behind `syn`) without forcing the user to declare them in
    // `cfg.dependency()`. Stable cargo keeps every unit in
    // `target/release/deps/`, so the direct dep's parent was enough.
    // Nightly cargo (1.101+) gives each unit its own directory, so
    // host external deps also bring in every directory `gluon vendor`
    // recorded in `gluon.lock` (`model.external_search_dirs`).
    //
    // Sorted iteration: BTreeSet is already deterministic, but state
    // it explicitly — argv order matters for cache-key stability.
    for dir in &external_dep_dirs {
        builder.library_search_dependency(dir);
    }

    // Transitive search paths. rustc validates the recorded dep
    // graph of every `--extern`'d rlib at consumer-compile time:
    // it walks each rlib's metadata, looks at the deps the rlib
    // *itself* records, and tries to load each by name from a
    // search path. `--extern` only covers the consumer's *direct*
    // deps; transitives (siblings, proc-macros, libs-of-libs) need
    // a search path.
    //
    // Cargo handles this by tracking the full transitive graph and
    // emitting `-L dependency=<dir>` for every directory holding a
    // transitive artifact. We mirror that: BFS the consumer's
    // direct Project deps, follow each one's deps, and collect the
    // parent dir of every reached artifact into a dedup set. Both
    // proc-macros (host dylibs in `build/host/<crate>/`) and cross
    // libs (rlibs in `build/cross/<target>/<profile>/<crate>/`)
    // need to be reachable; one BTreeSet handles both.
    //
    // External deps don't need walking — the search dirs added for
    // them above already cover their transitive deps (e.g.
    // `unicode-ident` behind `syn`).
    let mut transitive_dirs: std::collections::BTreeSet<PathBuf> = Default::default();
    let mut visited: std::collections::BTreeSet<gluon_model::Handle<gluon_model::CrateDef>> =
        Default::default();
    let mut queue: std::collections::VecDeque<gluon_model::Handle<gluon_model::CrateDef>> =
        Default::default();
    for dep_def in crate_def.deps.values() {
        if let DepResolution::Project(h) = dep_def.resolution
            && visited.insert(h)
        {
            queue.push_back(h);
        }
    }
    while let Some(h) = queue.pop_front() {
        let Some(dep_crate) = model.crates.get(h) else {
            continue;
        };
        if let Some(p) = artifacts.get(h)
            && let Some(parent) = p.parent()
        {
            transitive_dirs.insert(parent.to_path_buf());
        }
        for d in dep_crate.deps.values() {
            if let DepResolution::Project(child_h) = d.resolution
                && visited.insert(child_h)
            {
                queue.push_back(child_h);
            }
        }
    }
    for dir in &transitive_dirs {
        builder.library_search_dependency(dir);
    }

    // Modern proc-macro source (no `extern crate proc_macro;` line)
    // imports `proc_macro` directly. Without `--extern proc_macro`,
    // rustc fails with `E0432: unresolved import proc_macro`. The
    // `proc_macro` crate is a sysroot built-in for the host target;
    // bare `--extern proc_macro` (no `=path`) makes rustc resolve it
    // via its sysroot's `target-libdir`, matching what cargo does for
    // every `--crate-type=proc-macro` build. Cheap to emit
    // unconditionally for proc-macro crates, and harmless when the
    // source happens to use the older `extern crate` form.
    if crate_def.crate_type == CrateType::ProcMacro {
        builder.extern_crate_bare("proc_macro");
    }

    // Artifact env vars: deterministic, BTreeMap-ordered, canonicalised.
    // Resolution is centralised in `compile::env::resolve_crate_env` so
    // analyzer-time consumers (`rust-project.json` generation) and
    // build-time consumers (this site) produce *identical* env-var
    // pairs for the same crate. Symmetry between the two is what makes
    // `env!("FOO")` resolve the same way under rustc and under
    // rust-analyzer.
    //
    // The closure here reads from the just-built ArtifactMap; missing
    // entries indicate a scheduler bug (the `artifact_deps` ordering
    // edge auto-added by the Rhai builder should have ensured the
    // producer was built first).
    let env_vars = crate::compile::env::resolve_crate_env(crate_def, model, |h| {
        artifacts.get(h).map(std::path::Path::to_path_buf)
    })?;
    for (k, v) in &env_vars {
        builder.env(k.as_str(), v.as_str());
    }

    // Profile-driven code-generation knobs.
    builder
        .opt_level(resolved.profile.opt_level)
        .debug_info(resolved.profile.debug_info);
    if let Some(mode) = &resolved.profile.lto {
        builder.lto(mode);
    }

    // Per-crate cfg flags (Vec order is preserved for determinism; the
    // Rhai evaluator always produces them in declaration order).
    for flag in &crate_def.cfg_flags {
        builder.cfg(flag);
    }

    // Panic strategy (cross crates only) — must be consistent across
    // sysroot + user crates for the same target, otherwise rustc rejects
    // the link with a "different panic strategy" error. Emitted at this
    // position to match the canonical token order documented at the top
    // of this module.
    if !crate_ref.host {
        let target = model.targets.get(crate_ref.target).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, crate_ref.target
            ))
        })?;
        if let Some(s) = target.panic_strategy {
            builder
                .raw_arg("-C")
                .raw_arg(format!("panic={}", s.as_str()));
        }
    }

    // Extra rustc flags — appended last (before extra-filename) so they
    // cannot accidentally interleave with the structured flags above.
    for flag in &crate_def.rustc_flags {
        builder.raw_arg(flag.as_str());
    }

    // Incremental compilation directory.
    builder.incremental(&layout.incremental_dir(crate_def));

    // Linker script (executable crates only — Bin or RawBinary).
    // Resolved relative to the project root.
    if crate_def.crate_type.is_executable() {
        if let Some(ls) = &crate_def.linker_script {
            builder.linker_script(&project_root.join(ls));
        }
        // For Bin (cross): we use --out-dir = cross_final_dir and let
        // rustc produce <name>{exe-suffix}. No -o needed.
        //
        // For RawBinary: rustc emits <name>.elf into cross_final_dir.
        // We pass -o <name>.elf explicitly because rustc's default
        // executable name has no extension and we want an unambiguous
        // .elf suffix the post-link objcopy step (compile_crate.rs
        // wires it via post_compile_step) can target. The flat
        // <name>.bin is produced by objcopy after rustc returns
        // success — see compile/objcopy.rs.
        if crate_def.crate_type == CrateType::RawBinary {
            let elf_path = output_path.with_extension("elf");
            builder.raw_arg("-o").raw_arg(elf_path.as_os_str());
        }
    }

    // Append -C extra-filename LAST (mirrors sysroot convention). This must
    // come after all other flags so the canonical token order (and therefore
    // the cache hash) stays stable.
    //
    // Exception: cross executables (Bin and RawBinary) skip extra-filename
    // entirely. See the output-path derivation comment above for the full
    // rationale. The `if` here ensures the cache key is still stable —
    // the absence of the flag for executable crates is part of the
    // canonical token order.
    if !crate_def.crate_type.is_executable() || crate_ref.host {
        builder
            .raw_arg("-C")
            .raw_arg(format!("extra-filename={extra_filename}"));
    }

    // For Check/Clippy on non-proc-macro crates, the artifact path
    // computed above (rlib / bin / dylib) is *never written* — only
    // metadata + dep-info land on disk. The cache freshness check in
    // `compile_and_cache` uses `output_path.exists()` as a final
    // sanity guard, which would fail forever under those drivers and
    // re-run rustc on every invocation.
    //
    // Use the depfile as the cache stamp instead: rustc reliably
    // writes it under both Rustc and Check/Clippy, and its existence
    // is a faithful proxy for "we ran a successful compile". The
    // BuildRecord stores it the same way as a real artifact path; the
    // `compile()` caller's return value is the path that downstream
    // crates use to wire `--extern`, so for proc-macros (which still
    // produce a real artifact under Check) we keep the original
    // dylib path.
    let cache_output_path =
        if ctx.driver().emit_override().is_some() && crate_def.crate_type != CrateType::ProcMacro {
            depfile_path.clone()
        } else {
            output_path.clone()
        };

    Ok((builder, cache_output_path, depfile_path))
}
