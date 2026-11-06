//! Generate and compile the `<project>_config` crate.
//!
//! This helper synthesises a small `no_std` Rust library that exposes every
//! resolved config option as a `pub const`. Downstream cross crates consume
//! it via `--extern <name>_config` so that build-time configuration is
//! available as typed compile-time constants rather than raw environment
//! variables or link-time symbols.
//!
//! ### Source generation
//!
//! The source is rendered deterministically from `ResolvedConfig::options`
//! (a `BTreeMap`, so already in sorted key order). Two runs with identical
//! resolved configs produce byte-identical source, which is the prerequisite
//! for the mtime fast-path in the cache.
//!
//! ### Compilation target
//!
//! One config crate is compiled per distinct cross target in the resolved
//! crate list, per profile. Downstream cross crates consume the config
//! crate for their own target via `--extern`.
//!
//! ### Identity and layout
//!
//! The crate's metadata hash ([`crate::unit::UnitGraph::config_metadata`])
//! covers its name, the target and the rendered source, so each set of
//! resolved config values is a separate artifact. The source lives at
//! `generated/<project>_config/<target>/<h16>/src/lib.rs` and the rlib at
//! `cross/<target>/<profile>/deps/lib<name>-<h16>.rlib` under the build
//! root (shared by every driver). Profiles and presets therefore each keep
//! their own copy, and switching between them rebuilds nothing.

use crate::compile::compile_utils::sanitise_crate_name;
use crate::compile::{CompileCtx, Emit, RustcCommandBuilder, RustcJob};
use crate::error::{Diagnostic, Error, Result};
use crate::unit::Metadata;
use quon_model::{BuildModel, CrateType, Handle, ResolvedConfig, ResolvedValue, TargetDef};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Source rendering
// ---------------------------------------------------------------------------

/// Sanitise an option name to a valid Rust SCREAMING_SNAKE_CASE identifier.
///
/// Uppercases every character and replaces any byte that is not `[A-Z0-9_]`
/// with `_`. If the result starts with an ASCII digit, a leading `_` is
/// prepended so the identifier is valid Rust (e.g. `"1foo"` → `"_1FOO"`).
///
/// Returns `Err(Error::Compile(...))` if the input produces an empty string
/// after sanitisation (e.g. an all-punctuation name with no alphanumeric
/// content that was not already mapped to `_`).
pub(super) fn sanitise_option_name(s: &str) -> Result<String> {
    let sanitised: String = s
        .chars()
        .map(|c| {
            let uc = c.to_ascii_uppercase();
            if uc.is_ascii_alphanumeric() || uc == '_' {
                uc
            } else {
                '_'
            }
        })
        .collect();

    if sanitised.is_empty() {
        return Err(Error::Compile(format!(
            "config crate: option name '{s}' produces an empty identifier after sanitisation; \
             the name must contain at least one alphanumeric or underscore character"
        )));
    }

    // Prepend `_` if the sanitised result starts with a digit, since
    // digit-leading identifiers are not valid Rust.
    if sanitised.starts_with(|c: char| c.is_ascii_digit()) {
        Ok(format!("_{sanitised}"))
    } else {
        Ok(sanitised)
    }
}

/// Render the `pub const` declaration for a single option.
///
/// Returns `None` if the variant cannot be represented as a Rust const
/// (currently only `List`, which has no natural `const` type in `no_std`).
fn render_const(ident: &str, value: &ResolvedValue) -> Option<String> {
    match value {
        ResolvedValue::Bool(b) => Some(format!(
            "pub const {ident}: bool = {};",
            if *b { "true" } else { "false" }
        )),
        ResolvedValue::Tristate(t) => {
            use quon_model::kconfig::TristateVal;
            let v = match t {
                TristateVal::Yes => "2u8",
                TristateVal::Module => "1u8",
                TristateVal::No => "0u8",
            };
            // Tristate doesn't map to a standard Rust type — represent as u8
            // with 2=yes, 1=module, 0=no. Document the encoding inline.
            Some(format!(
                "/// Tristate: 2=yes, 1=module, 0=no.\npub const {ident}: u8 = {v};"
            ))
        }
        ResolvedValue::U32(u) => Some(format!("pub const {ident}: u32 = {u}u32;")),
        ResolvedValue::U64(u) => Some(format!("pub const {ident}: u64 = {u}u64;")),
        ResolvedValue::String(s) => {
            // `{:?}` gives Rust-debug quoting, which is valid Rust string literal
            // syntax (escapes backslashes, quotes, control chars, etc.).
            Some(format!("pub const {ident}: &str = {s:?};"))
        }
        ResolvedValue::Choice(s) => {
            // Choice values are strings naming the selected variant.
            Some(format!("pub const {ident}: &str = {s:?};"))
        }
        ResolvedValue::List(_) => {
            // Lists cannot be expressed as a `no_std` const without a const
            // generic array and a known length. Emit as a comment so the
            // option is visible without causing a compile error.
            None
        }
    }
}

/// Render the full source text for the generated config crate.
///
/// `pub(crate)` so that unit tests can call it without going through the
/// filesystem + rustc. The production path calls this then writes to disk.
pub(crate) fn render_source(resolved: &ResolvedConfig) -> Result<String> {
    const HEADER: &str = "\
// Auto-generated by quon. Do not edit.
// This file is regenerated from ResolvedConfig.options on every build;
// edits will be silently overwritten.
#![no_std]
#![allow(dead_code)]
";

    // Check for identifier collisions before generating any output. Using a
    // BTreeMap means we detect the *first* collision deterministically.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for name in resolved.options.keys() {
        let ident = sanitise_option_name(name)?;
        if let Some(prev) = seen.get(&ident) {
            return Err(Error::Compile(format!(
                "config crate: option name collision: '{prev}' and '{name}' \
                 both map to identifier '{ident}'"
            )));
        }
        seen.insert(ident, name.clone());
    }

    // Render each option in BTreeMap order (already sorted).
    let mut lines: Vec<String> = Vec::new();
    for (name, value) in &resolved.options {
        let ident = sanitise_option_name(name)?;
        match render_const(&ident, value) {
            Some(decl) => lines.push(decl),
            None => {
                // Unsupported type — emit a comment so the option is visible.
                lines.push(format!("// {ident}: <unsupported type, skipped>"));
            }
        }
    }

    let mut out = HEADER.to_string();
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// `--crate-name` and extern name of the config crate.
pub(crate) fn config_crate_name(resolved: &ResolvedConfig) -> String {
    resolved
        .project
        .config_crate_name
        .clone()
        .unwrap_or_else(|| sanitise_crate_name(&resolved.project.name) + "_config")
}

/// Ensure the generated `<project>_config` crate is compiled and cached.
///
/// Returns `(crate_name, rlib_path, was_cached)`. `was_cached` is `true`
/// when the cache freshness check short-circuited rustc entirely.
///
/// ### Source writing discipline
///
/// The source file is only rewritten when its content has changed. This
/// preserves the file's mtime on identical builds so the cache can
/// short-circuit via the mtime fast-path without re-running rustc.
///
/// ### Cache integration
///
/// Goes through `compile_and_cache` like every rustc node. The
/// generated `lib.rs` is a declared input.
pub fn ensure_config_crate(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    target_handle: Handle<TargetDef>,
    metadata: Metadata,
    sysroot_dir: &Path,
    stderr_sink: &mut Vec<u8>,
) -> Result<(String, PathBuf, bool)> {
    let layout = &ctx.layout;

    // Resolve the cross target for this config crate instance.
    let target = model.targets.get(target_handle).ok_or_else(|| {
        Error::Compile(format!(
            "config crate: target handle {:?} not found in build model",
            target_handle
        ))
    })?;

    let crate_name = config_crate_name(resolved);
    let h16 = metadata.short();
    let extra_filename = format!("-{h16}");

    let platform_dir = layout.shared_cross_dir(target, &resolved.profile);
    let out_dir = platform_dir.join("deps");
    let output_path = out_dir.join(format!("lib{crate_name}{extra_filename}.rlib"));
    let depfile_path = out_dir.join(format!("{crate_name}{extra_filename}.d"));

    // The source path carries the hash, which covers the source, so a
    // file there never changes content and switching profiles or
    // presets never rewrites (and so never invalidates) another one.
    let src_dir = layout
        .generated_config_crate_dir()
        .join(&target.name)
        .join(&h16)
        .join("src");
    let src_path = src_dir.join("lib.rs");

    // --- Write source (only if changed) ---
    //
    // Comparing content before writing preserves the mtime so subsequent
    // cache checks can short-circuit via the mtime fast-path. On the cold
    // path the file doesn't exist at all and `fs::read_to_string` returns
    // an error, which we treat as "changed".
    let new_source = render_source(resolved)?;
    let existing_source = fs::read_to_string(&src_path).unwrap_or_default();
    if new_source != existing_source {
        fs::create_dir_all(&src_dir).map_err(|e| Error::Io {
            path: src_dir.clone(),
            source: e,
        })?;
        fs::write(&src_path, new_source.as_bytes()).map_err(|e| Error::Io {
            path: src_path.clone(),
            source: e,
        })?;
    }

    // --- Assemble rustc command ---
    let sysroot_lib = layout.sysroot_lib_dir(target);
    let incremental_dir = platform_dir
        .join("incremental")
        .join(format!("{crate_name}-{h16}"));

    let mut builder = RustcCommandBuilder::new(&ctx.rustc_info.rustc_path);
    builder
        .crate_name(&crate_name)
        .crate_type(CrateType::Lib)
        // The generated source only uses stable 2021 features (pub const,
        // #![no_std], #![allow(...)]) — no 2024 features needed here.
        .edition("2021")
        .input(&src_path)
        .target(&target.spec, target.builtin)
        .out_dir(&out_dir)
        .emit_with_dep_info_path(&[Emit::Link, Emit::Metadata, Emit::DepInfo], &depfile_path)
        .incremental(&incremental_dir)
        .opt_level(resolved.profile.opt_level)
        .debug_info(resolved.profile.debug_info)
        .debug_assertions(resolved.profile.debug_assertions)
        .overflow_checks(resolved.profile.overflow_checks);

    // Inject implicit sysroot crates. Must match what build_sysroot_crate
    // writes (sysroot crates keep the -quon-<name> extra-filename). A
    // prebuilt sysroot is rustc's own, which it injects itself.
    if target.builds_sysroot() {
        builder
            .sysroot(sysroot_dir)
            .extern_crate("core", &sysroot_lib.join("libcore-quon-core.rlib"))
            .extern_crate(
                "compiler_builtins",
                &sysroot_lib.join("libcompiler_builtins-quon-compiler_builtins.rlib"),
            )
            .extern_crate("alloc", &sysroot_lib.join("liballoc-quon-alloc.rlib"));
    }

    // Panic strategy — must match the sysroot and downstream crates.
    if let Some(s) = target.panic_strategy {
        builder
            .raw_arg("-C")
            .raw_arg(format!("panic={}", s.as_str()));
    }

    // metadata and extra-filename LAST, as for every unit, so the token
    // order (and therefore the cache key hash) stays stable.
    builder
        .raw_arg("-C")
        .raw_arg(format!("metadata={}", metadata.hex()))
        .raw_arg("-C")
        .raw_arg(format!("extra-filename={extra_filename}"));

    // --- Cache integration (delegated to compile::compile_and_cache) ---
    let cache_key = format!(
        "config_crate:{crate_name}:{}:{}:{h16}",
        target.name, resolved.profile.name
    );

    let crate_name_fail = crate_name.clone();
    let crate_name_depfile = crate_name.clone();
    let target_name_fail = target.name.clone();
    let depfile_for_diag = depfile_path.clone();

    let (output_path, was_cached) = crate::compile::run_compile_and_cache(
        ctx,
        RustcJob {
            what: format!("config crate ({})", target.name),
            builder,
            key: cache_key,
            output_path,
            depfile_path,
            out_dir: Some(&out_dir),
            // The generated source; rustc's depfile lists it too.
            declared: vec![src_path],
            aux_outputs: Vec::new(),
            post_compile_step: None,
            rustc_fail: Box::new(move |output| {
                Error::Diagnostics(vec![Diagnostic::error(format!(
                    "rustc failed while building config crate '{}' for target '{}': exit={:?}",
                    crate_name_fail,
                    target_name_fail,
                    output.status.code(),
                ))])
            }),
            depfile_fail: Box::new(move |e| {
                Error::Diagnostics(vec![
                    Diagnostic::error(format!(
                        "built config crate '{}' but its depfile at {} could not be read",
                        crate_name_depfile,
                        depfile_for_diag.display(),
                    ))
                    .with_note(format!("underlying error: {e}"))
                    .with_note(
                        "config crate rlib is on disk but cache not updated; \
             the next run will re-spawn rustc for this crate",
                    ),
                ])
            }),
        },
        stderr_sink,
    )?;

    Ok((crate_name, output_path, was_cached))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use quon_model::{Handle, ProjectDef, ResolvedConfig, ResolvedProfile};

    fn make_resolved_empty() -> ResolvedConfig {
        ResolvedConfig {
            project: ProjectDef {
                name: "testproj".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: quon_model::TargetSpec::Cross(Handle::new(0)),
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
            build_dir: "/tmp/build".into(),
            project_root: "/tmp".into(),
        }
    }

    #[test]
    fn generate_source_empty_options_produces_valid_header_only() {
        let resolved = make_resolved_empty();
        let source = render_source(&resolved).expect("render");
        assert!(
            source.starts_with("// Auto-generated by quon."),
            "expected auto-generated header, got:\n{source}"
        );
        assert!(
            source.contains("#![no_std]"),
            "expected #![no_std], got:\n{source}"
        );
        // No const declarations when options is empty.
        assert!(
            !source.contains("pub const"),
            "expected no consts for empty options, got:\n{source}"
        );
    }

    #[test]
    fn generate_source_emits_sorted_consts() {
        let mut resolved = make_resolved_empty();
        resolved
            .options
            .insert("alpha".into(), ResolvedValue::Bool(true));
        resolved
            .options
            .insert("beta".into(), ResolvedValue::U32(42));
        resolved
            .options
            .insert("gamma".into(), ResolvedValue::String("hello".into()));

        let source = render_source(&resolved).expect("render");

        // BTreeMap order → alphabetical order in rendered source.
        let alpha_pos = source.find("ALPHA").expect("ALPHA not found");
        let beta_pos = source.find("BETA").expect("BETA not found");
        let gamma_pos = source.find("GAMMA").expect("GAMMA not found");
        assert!(
            alpha_pos < beta_pos && beta_pos < gamma_pos,
            "expected ALPHA < BETA < GAMMA in output:\n{source}"
        );

        // Spot-check values.
        assert!(source.contains("pub const ALPHA: bool = true;"));
        assert!(source.contains("pub const BETA: u32 = 42u32;"));
        assert!(source.contains("pub const GAMMA: &str = \"hello\";"));
    }

    #[test]
    fn generate_source_collision_returns_error() {
        let mut resolved = make_resolved_empty();
        // Both sanitise to "FOO_BAR".
        resolved
            .options
            .insert("FOO-BAR".into(), ResolvedValue::Bool(true));
        resolved
            .options
            .insert("FOO_BAR".into(), ResolvedValue::Bool(false));

        let err = render_source(&resolved).expect_err("should error on collision");
        match err {
            Error::Compile(msg) => {
                assert!(
                    msg.contains("FOO-BAR") && msg.contains("FOO_BAR"),
                    "error message should name BOTH colliding options, got: {msg}"
                );
                assert!(
                    msg.contains("collision"),
                    "error message should say 'collision', got: {msg}"
                );
            }
            other => panic!("expected Error::Compile, got: {other:?}"),
        }
    }

    #[test]
    fn sanitise_option_name_cases() {
        let cases = [
            ("digit-leading name gets a _ prefix", "1foo", "_1FOO"),
            ("all-digit name gets a _ prefix", "42", "_42"),
            ("snake_case name is only uppercased", "foo_bar", "FOO_BAR"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(sanitise_option_name(input).unwrap(), expected, "{name}");
        }
    }
}
