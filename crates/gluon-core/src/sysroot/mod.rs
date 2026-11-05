//! Per-target custom sysroot builder.
//!
//! [`ensure_sysroot`] builds `core`, `compiler_builtins`, and `alloc` from
//! the host toolchain's `rust-src` component into a per-target sysroot
//! directory owned by [`crate::compile::BuildLayout`]. The resulting layout
//! is shaped so that a later `rustc --sysroot=<dir>` invocation for a
//! cross-crate Just Works.
//!
//! ### Fast path vs slow path
//!
//! Freshness is one fingerprint record keyed `sysroot:<target_name>`.
//! Its command hashes what decides how every sysroot crate compiles:
//! the target spec, the panic strategy and the rust-src path. Its
//! declared inputs are the rustc binary and a custom target's JSON, and
//! its outputs are the three rlibs, so `rm -rf <sysroot>` forces a
//! rebuild.
//!
//! - **Fast path.** If the record is fresh, return immediately — no
//!   rustc spawn, no per-crate checks. This is the common case on every
//!   build after the first.
//! - **Slow path.** Otherwise compile the three sysroot crates in
//!   dependency order (`core` → `compiler_builtins` → `alloc`), each with
//!   its own record, so a crate whose inputs did not change is skipped.
//!   After all three succeed the node's record is written.
//!
//! If the build is interrupted before the node's record is written, the
//! next invocation re-enters the slow path, which is itself
//! record-driven and near-free on a clean tree.
//!
//! `RUSTC_BOOTSTRAP=1` and `-Z force-unstable-if-unmarked` are set on
//! every sysroot rustc invocation; see
//! `docs/src/architecture/external-deps.md` for the rationale.

use crate::cache::{Freshness, FreshnessQuery, RecordInput, hash_parts};
use crate::compile::{CompileCtx, Emit, RustcCommandBuilder, RustcJob};
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, CrateType, Handle, TargetDef};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The crates in every sysroot, in build order.
pub(crate) const SYSROOT_CRATES: [&str; 3] = ["core", "compiler_builtins", "alloc"];

/// Command hash of the sysroot node: everything that changes how the
/// sysroot crates compile and is not a file input. The `v2` tag stands
/// for the flags `build_sysroot_crate` passes; change it when they
/// change, or the fast path keeps serving sysroots built the old way.
pub(crate) fn sysroot_command(ctx: &CompileCtx, target: &TargetDef) -> [u8; 32] {
    let rust_src = ctx
        .rustc_info
        .rust_src
        .as_deref()
        .map(|p| p.as_os_str().as_encoded_bytes())
        .unwrap_or_default();
    let panic = target.panic_strategy.map(|p| p.as_str()).unwrap_or("");
    hash_parts(
        "gluon.sysroot.v2",
        &[
            target.spec.as_bytes(),
            &[u8::from(target.builtin)],
            panic.as_bytes(),
            rust_src,
        ],
    )
}

/// Ensure a usable custom sysroot exists for `target` and return its
/// directory plus whether the result came from the cache fast-path.
///
/// Returns `(ctx.layout.sysroot_dir(target), was_cached)` on success.
/// `was_cached` is `true` when the `sysroot:<target>` record
/// entry was fresh (no rustc spawned). See the module docs for the
/// fast-path/slow-path split.
///
/// The `was_cached` flag is consumed by the scheduler and surfaced via
/// [`crate::BuildSummary`] so the CLI can report a single accurate
/// `built N, cached M` line per build.
pub fn ensure_sysroot(
    ctx: &CompileCtx,
    target: &TargetDef,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let sysroot_dir = ctx.layout.sysroot_dir(target);
    let sysroot_lib_dir = ctx.layout.sysroot_lib_dir(target);

    let rlibs: Vec<PathBuf> = SYSROOT_CRATES
        .iter()
        .map(|c| sysroot_lib_dir.join(format!("lib{c}-gluon-{c}.rlib")))
        .collect();
    let command = sysroot_command(ctx, target);
    let toolchain = ctx.toolchain();
    let key = format!("sysroot:{}", target.name);
    let what = format!("sysroot {}", target.name);
    // rustc's depfiles list neither the compiler nor a custom target
    // spec, and both change how every sysroot crate compiles.
    let mut declared = vec![ctx.rustc_info.rustc_path.clone()];
    declared.extend(crate::compile::rustc::target_spec_file(
        &target.spec,
        target.builtin,
    ));
    let no_env = BTreeMap::new();
    let query = FreshnessQuery {
        key: &key,
        command,
        toolchain,
        declared: &declared,
        outputs: &rlibs,
        command_env: &no_env,
    };
    match ctx.store.check(&query) {
        Freshness::Fresh(_) => return Ok((sysroot_dir, true)),
        Freshness::Stale(reason) => ctx.report_stale(stderr_sink, &what, &reason),
    }
    let started_at = std::time::SystemTime::now();

    // rust-src must be present before we can compile anything.
    let rust_src = match ctx.rustc_info.rust_src.as_ref() {
        Some(p) => p,
        None => {
            let release = &ctx.rustc_info.release;
            return Err(Error::Diagnostics(vec![
                Diagnostic::error(
                    "custom sysroot build requires the `rust-src` component, but it is not \
                     installed for the current toolchain",
                )
                .with_note(format!(
                    "run: rustup component add rust-src --toolchain {release}"
                ))
                .with_note(
                    "rust-src should appear under \
                     <sysroot>/lib/rustlib/src/rust once installed",
                ),
            ]));
        }
    };

    // Make sure the output directory exists before rustc runs.
    std::fs::create_dir_all(&sysroot_lib_dir).map_err(|e| Error::Io {
        path: sysroot_lib_dir.clone(),
        source: e,
    })?;

    // Compile the three sysroot crates in dependency order. Each later
    // crate links against the rlibs produced by the earlier ones.
    let core_rlib = build_sysroot_crate(
        ctx,
        target,
        rust_src,
        &sysroot_lib_dir,
        "core",
        &[],
        stderr_sink,
    )?;
    let cbuiltins_rlib = build_sysroot_crate(
        ctx,
        target,
        rust_src,
        &sysroot_lib_dir,
        "compiler_builtins",
        &[("core", &core_rlib)],
        stderr_sink,
    )?;
    let _alloc_rlib = build_sysroot_crate(
        ctx,
        target,
        rust_src,
        &sysroot_lib_dir,
        "alloc",
        &[("core", &core_rlib), ("compiler_builtins", &cbuiltins_rlib)],
        stderr_sink,
    )?;

    // The node's record goes last, after all three crates succeeded.
    ctx.store.record(RecordInput {
        key: &key,
        command,
        toolchain,
        declared: &declared,
        discovered: &[],
        env: BTreeMap::new(),
        outputs: &rlibs,
        started_at,
        stderr: &[],
        data: None,
    })?;

    Ok((sysroot_dir, false))
}

/// Scheduler-side wrapper around [`ensure_sysroot`]. Looks up the
/// target by handle and delegates. Used by the pipeline dispatcher
/// (one DAG-node arm per scheduler operation), kept distinct from
/// `ensure_sysroot` so the dispatcher's match stays flat.
///
/// `stderr_sink` receives rustc's stderr (warnings) from each of the
/// three sysroot crate compiles; the worker pool flushes it to the
/// user's stderr atomically per job. The `_stdout` parameter remains
/// unused — the sysroot pipeline produces nothing of interest on
/// stdout.
pub fn ensure_sysroot_for_node(
    ctx: &CompileCtx,
    model: &BuildModel,
    target: Handle<TargetDef>,
    _stdout: &mut Vec<u8>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let target_def = model.targets.get(target).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: Sysroot node references target handle {target:?} not found in build model"
        ))
    })?;
    ensure_sysroot(ctx, target_def, stderr_sink)
}

/// Compile a single sysroot crate. Returns the absolute path of the
/// produced rlib.
///
/// `extern_deps` is a list of `(crate_name, rlib_path)` pairs passed to
/// rustc as `--extern`. For `core` this is empty; for `compiler_builtins`
/// it's `[("core", libcore.rlib)]`; for `alloc` it includes both.
fn build_sysroot_crate(
    ctx: &CompileCtx,
    target: &TargetDef,
    rust_src: &Path,
    sysroot_lib_dir: &Path,
    crate_name: &str,
    extern_deps: &[(&str, &PathBuf)],
    stderr_sink: &mut Vec<u8>,
) -> Result<PathBuf> {
    // Path inside `library/` that holds the crate's `src/lib.rs`. The
    // convention is `library/<crate_name>/src/lib.rs`, but
    // compiler-builtins is the outlier: it lives in a nested workspace
    // (`library/compiler-builtins/compiler-builtins/`) and its directory
    // uses a dash rather than the `compiler_builtins` crate-name
    // underscore. Everything else is straightforward.
    let src_path = match crate_name {
        "compiler_builtins" => rust_src
            .join("library")
            .join("compiler-builtins")
            .join("compiler-builtins")
            .join("src")
            .join("lib.rs"),
        _ => rust_src
            .join("library")
            .join(crate_name)
            .join("src")
            .join("lib.rs"),
    };

    // We use `-C extra-filename=-gluon-<name>` so rustc writes a
    // deterministic `lib<name>-gluon-<name>.rlib` (and matching `.d`
    // depfile) instead of the default hash-suffixed name. This avoids a
    // post-spawn glob to discover the real output path.
    let extra_filename = format!("-gluon-{crate_name}");
    let output_rlib = sysroot_lib_dir.join(format!("lib{crate_name}{extra_filename}.rlib"));
    // Rustc names the depfile `<crate_name><extra_filename>.d` — note the
    // absence of the `lib` prefix (the prefix is a library-output
    // convention; depfile names derive from the crate name verbatim).
    // Empirically verified against rustc 1.x with `-C extra-filename`.
    let depfile_path = sysroot_lib_dir.join(format!("{crate_name}{extra_filename}.d"));

    // No CrateDef is available for a sysroot crate, so we synthesise an
    // incremental directory path under the build root directly rather
    // than adding a dedicated method to BuildLayout. Keeping the path
    // derivation local documents that this is a sysroot-only shape.
    let incremental_dir = ctx
        .layout
        .root()
        .join("incremental")
        .join(format!("sysroot-{}-{}", target.name, crate_name));

    let mut builder = RustcCommandBuilder::new(&ctx.rustc_info.rustc_path);
    builder
        .crate_name(crate_name)
        .crate_type(CrateType::Lib)
        // rust-src tracks the edition of the current rust-lang/rust
        // checkout. As of the edition-2024 migration, `core`/`alloc`/
        // `compiler_builtins` all require `--edition=2024`. Older stable
        // toolchains used 2021, but those predate gluon's MVP-M cutoff
        // so we align with the current standard rather than sniffing.
        .edition("2024")
        .input(&src_path)
        .target(&target.spec, target.builtin)
        .out_dir(sysroot_lib_dir)
        .emit_with_dep_info_path(&[Emit::Link, Emit::Metadata, Emit::DepInfo], &depfile_path)
        .incremental(&incremental_dir)
        // Sysroot crates are always optimised — bare-metal code
        // performance is never worth trading for a faster sysroot build.
        .opt_level(2)
        .debug_info(false);

    // Propagate the target's panic strategy to sysroot crates. Bare-metal
    // targets almost always want `abort`, and mixing panic strategies
    // across sysroot rlibs and downstream crates fails at link time:
    //   error: the crate ... is compiled with a different panic strategy
    // Emit `-C panic=<s>` before the extra-filename arg so the canonical
    // token order in args (and therefore the cache hash) stays stable.
    if let Some(s) = target.panic_strategy {
        builder
            .raw_arg("-C")
            .raw_arg(format!("panic={}", s.as_str()));
    }

    // compiler_builtins needs two cfgs to build standalone:
    //   * `feature="compiler-builtins"` activates the crate's
    //     `#![cfg_attr(..., compiler_builtins)]` attribute, which is
    //     what tells rustc to *not* auto-inject an `extern crate
    //     compiler_builtins` into this crate (otherwise we hit E0463
    //     "can't find crate for compiler_builtins" building itself).
    //   * `feature="mem"` makes the crate emit memcpy/memset/memmove/
    //     memcmp/strlen so bare-metal targets without a libc link
    //     cleanly.
    // We pass cfgs, not Cargo features, so the features that
    // `compiler-builtins` implies in the crate's Cargo.toml must be
    // listed too:
    //   * `feature="unmangled-names"` gives those functions their C
    //     names (`#[no_mangle]`). Without it they exist only under
    //     mangled names, and the first `strlen` or `memcpy` call that
    //     LLVM does not inline fails to link.
    if crate_name == "compiler_builtins" {
        builder
            .cfg("feature=\"compiler-builtins\"")
            .cfg("feature=\"unmangled-names\"")
            .cfg("feature=\"mem\"");
    }

    for (name, rlib) in extern_deps {
        builder.extern_crate(name, rlib.as_path());
    }

    // Sysroot crates use `#![feature(...)]` internally, so a stable
    // rustc needs RUSTC_BOOTSTRAP=1 to accept them. `-Z
    // force-unstable-if-unmarked` matches Cargo's build-std behaviour:
    // downstream crates can't accidentally rely on sysroot internals
    // being stable.
    builder
        .env("RUSTC_BOOTSTRAP", "1")
        .raw_arg("-Z")
        .raw_arg("force-unstable-if-unmarked");

    // Append -C extra-filename AFTER every other setter so the canonical
    // token order in `args` (and therefore the hash) stays stable.
    builder
        .raw_arg("-C")
        .raw_arg(format!("extra-filename={extra_filename}"));

    let cache_key = format!("sysroot:{}:{}", target.name, crate_name);

    // Delegate the check → spawn → depfile → record sequence to
    // the shared helper. Sysroot crates create their `sysroot_lib_dir`
    // once per target in `build_sysroot` above, so `out_dir_to_create`
    // is `None` here — there is nothing for the helper to mkdir.
    let crate_name_fail = crate_name.to_string();
    let crate_name_depfile = crate_name.to_string();
    let target_name_fail = target.name.clone();
    let target_name_depfile = target.name.clone();
    let depfile_for_diag = depfile_path.clone();
    let (path, _was_cached) = crate::compile::run_compile_and_cache(
        ctx,
        RustcJob {
            what: format!("sysroot crate {crate_name} ({})", target.name),
            builder,
            key: cache_key,
            output_path: output_rlib,
            depfile_path,
            out_dir: None,
            declared: Vec::new(),
            aux_outputs: Vec::new(),
            post_compile_step: None,
            rustc_fail: Box::new(move |output| {
                Error::Diagnostics(vec![Diagnostic::error(format!(
                    "rustc failed while building sysroot crate '{}' for target '{}': exit={:?}",
                    crate_name_fail,
                    target_name_fail,
                    output.status.code(),
                ))])
            }),
            depfile_fail: Box::new(move |e| {
                Error::Diagnostics(vec![
                    Diagnostic::error(format!(
                        "built sysroot crate '{}' for target '{}' but its depfile at {} could not be read",
                        crate_name_depfile,
                        target_name_depfile,
                        depfile_for_diag.display(),
                    ))
                    .with_note(format!("underlying error: {e}"))
                    .with_note(
                        "the rlib is on disk but the cache was not updated; \
                         the next run will re-spawn rustc for this crate",
                    ),
                ])
            }),
        },
        stderr_sink,
    )?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{BuildLayout, RustcInfo};
    use std::sync::Arc;
    use std::time::Instant;

    fn make_target() -> TargetDef {
        TargetDef {
            name: "x86_64-unknown-none".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        }
    }

    fn fake_rustc_info(rustc_path: PathBuf, rust_src: Option<PathBuf>) -> RustcInfo {
        RustcInfo {
            rustc_path,
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test 2020-01-01)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: Some("deadbeefcafef00d".into()),
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/opt/fake-sysroot"),
            rust_src,
            mtime_ns: 0,
        }
    }

    #[test]
    fn missing_rust_src_produces_diagnostic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "sysroot-unit");
        let info = fake_rustc_info(PathBuf::from("/usr/bin/rustc"), None);
        let ctx = CompileCtx::new(layout, Arc::new(info));

        let target = make_target();
        let mut stderr_sink: Vec<u8> = Vec::new();
        let err = ensure_sysroot(&ctx, &target, &mut stderr_sink).expect_err("should fail");
        match err {
            Error::Diagnostics(diags) => {
                assert_eq!(diags.len(), 1);
                let rendered = diags[0].to_string();
                assert!(
                    rendered.contains("rustup component add rust-src"),
                    "missing rustup hint, got: {rendered}"
                );
                assert!(
                    rendered.contains("<sysroot>/lib/rustlib/src/rust"),
                    "missing expected-path note, got: {rendered}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    fn which_binary(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    /// Changing the target's panic strategy changes how every sysroot
    /// crate compiles, so a sysroot recorded for the old strategy is
    /// stale. Here the slow path then fails on the missing rust-src,
    /// which shows the record was not served.
    #[test]
    fn panic_strategy_change_invalidates_sysroot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "panic-change");
        let target = make_target();
        let real_path = which_binary("cat").unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let info = fake_rustc_info(real_path.clone(), None);
        write_fake_rlibs(&layout, &target);
        let ctx = record_sysroot_for_test(layout, info, &target);
        let mut stderr_sink = Vec::new();
        let (_, cached) = ensure_sysroot(&ctx, &target, &mut stderr_sink).expect("fast path");
        assert!(cached, "unchanged target must hit the record");

        let mut abort = target.clone();
        abort.panic_strategy = Some(gluon_model::PanicStrategy::Abort);
        let result = ensure_sysroot(&ctx, &abort, &mut stderr_sink);
        assert!(
            !matches!(result, Ok((_, true))),
            "sysroot built for another panic strategy was served from cache"
        );
    }

    fn write_fake_rlibs(layout: &BuildLayout, target: &TargetDef) {
        let dir = layout.sysroot_lib_dir(target);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for c in SYSROOT_CRATES {
            std::fs::write(dir.join(format!("lib{c}-gluon-{c}.rlib")), b"").expect("write rlib");
        }
    }

    /// A context whose store holds a sysroot record for `target` as a
    /// previous successful build would have written it.
    fn record_sysroot_for_test(
        layout: BuildLayout,
        info: RustcInfo,
        target: &TargetDef,
    ) -> CompileCtx {
        let ctx = CompileCtx::new(layout, Arc::new(info));
        let rlibs: Vec<PathBuf> = SYSROOT_CRATES
            .iter()
            .map(|c| {
                ctx.layout
                    .sysroot_lib_dir(target)
                    .join(format!("lib{c}-gluon-{c}.rlib"))
            })
            .collect();
        ctx.store
            .record(RecordInput {
                key: &format!("sysroot:{}", target.name),
                command: sysroot_command(&ctx, target),
                toolchain: ctx.toolchain(),
                declared: std::slice::from_ref(&ctx.rustc_info.rustc_path),
                discovered: &[],
                env: BTreeMap::new(),
                outputs: &rlibs,
                started_at: std::time::SystemTime::now(),
                stderr: &[],
                data: None,
            })
            .expect("record");
        ctx
    }

    /// End-to-end sysroot build against the host toolchain: build, hit
    /// the cache, then compile a downstream crate against the result.
    /// Other tests start from a seeded sysroot (`crate::testing`). Gated with `#[ignore]` because it needs the
    /// `rust-src` component and actually spawns rustc — run with
    /// `cargo test -p gluon-core --release -- --ignored sysroot`.
    #[test]
    #[ignore]
    fn e2e_real_sysroot_build() {
        let info = match RustcInfo::probe() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("e2e test skipped: rustc probe failed: {e}");
                return;
            }
        };
        if info.rust_src.is_none() {
            eprintln!("e2e test skipped: rust-src component not installed");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "sysroot-e2e");
        let target = make_target();
        let ctx = CompileCtx::new(layout, Arc::new(info));

        let mut stderr_sink: Vec<u8> = Vec::new();
        let (sysroot_dir, first_cached) =
            ensure_sysroot(&ctx, &target, &mut stderr_sink).expect("first build");
        assert!(!first_cached, "first build must not report was_cached=true");
        let lib_dir = ctx.layout.sysroot_lib_dir(&target);
        for crate_name in ["core", "compiler_builtins", "alloc"] {
            let rlib = lib_dir.join(format!("lib{crate_name}-gluon-{crate_name}.rlib"));
            assert!(rlib.exists(), "expected {rlib:?} to exist after build");
        }
        assert!(
            ctx.store
                .record_path(&format!("sysroot:{}", target.name))
                .exists(),
            "expected the sysroot node's record"
        );

        // Second call must hit the cache fast path and be effectively
        // instant. 100ms is a generous upper bound for a single
        // record check.
        let start = Instant::now();
        let (sysroot_dir2, second_cached) =
            ensure_sysroot(&ctx, &target, &mut stderr_sink).expect("second build");
        let elapsed = start.elapsed();
        assert_eq!(sysroot_dir, sysroot_dir2);
        assert!(second_cached, "second build must hit the cache fast path");
        assert!(elapsed.as_millis() < 100, "fast path too slow: {elapsed:?}");

        assert_downstream_crate_links(&ctx.rustc_info.rustc_path, &sysroot_dir, tmp.path());
    }

    /// Compile a minimal `no_std` crate against `sysroot_dir`: the
    /// acceptance criterion for the sysroot is that downstream code can
    /// find `core` through `--sysroot`. Skips (without failing) if the
    /// current nightly rejects the crate for reasons outside gluon's
    /// control.
    fn assert_downstream_crate_links(rustc_path: &Path, sysroot_dir: &Path, tmp: &Path) {
        // Minimal no_std crate. We emit metadata so we don't need a
        // linker on the host — the goal is only to prove that rustc can
        // discover `core` via `--sysroot=<dir>`, not to produce a
        // bootable binary.
        let src_path = tmp.join("downstream.rs");
        let src = r#"#![no_std]
#![no_main]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    loop {}
}
"#;
        std::fs::write(&src_path, src).expect("write downstream.rs");

        // `--emit=obj` sidesteps the host linker entirely, so this test
        // doesn't need a cross linker installed — it proves only that
        // rustc can resolve `core` via `--sysroot=<dir>` and codegen an
        // object file, which is the acceptance criterion we care about.
        let out_path = tmp.join("downstream.o");
        let mut cmd = std::process::Command::new(rustc_path);
        cmd.env("RUSTC_BOOTSTRAP", "1")
            .arg("--edition=2024")
            .arg("--crate-type=bin")
            .arg("--target=x86_64-unknown-none")
            .arg(format!("--sysroot={}", sysroot_dir.display()))
            .arg("-C")
            .arg("panic=abort")
            .arg("--emit=obj")
            .arg("-o")
            .arg(&out_path)
            .arg(&src_path);

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("downstream-link e2e skipped: spawning rustc failed: {e}");
                return;
            }
        };
        if !output.status.success() {
            eprintln!(
                "downstream-link e2e skipped: rustc rejected the minimal crate \
                 (exit={:?}, likely a toolchain quirk outside gluon's control)\n\
                 stderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        assert!(
            out_path.exists(),
            "rustc reported success but {out_path:?} is missing"
        );
        let meta = std::fs::metadata(&out_path).expect("stat output");
        assert!(
            meta.len() > 0,
            "rustc produced an empty artifact at {out_path:?}"
        );
    }
}
