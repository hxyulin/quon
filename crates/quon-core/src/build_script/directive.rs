//! Parse a build script's stdout into a [`BuildScriptOutput`].
//!
//! A line is a directive if it starts with `cargo::` (new syntax) or
//! `cargo:` (old syntax). Every other line is ignored; the caller keeps
//! the raw stdout in the run's `output` file for inspection.
//!
//! The two syntaxes differ only in how unknown keys are treated:
//!
//! - `cargo::KEY=VALUE`: `KEY` must be a known directive, otherwise the
//!   line is an error.
//! - `cargo:KEY=VALUE`: a known key is that directive, and any other key
//!   is metadata (`cargo:root=/x` means `metadata=root=/x`). As in
//!   Cargo, `error` and `metadata` exist only in the new syntax, so
//!   `cargo:error=x` is the metadata key `error`.
//!
//! Cargo accepts `cargo::` only from packages with `rust-version >=
//! 1.77`. Quon accepts both syntaxes from every package, because
//! rejecting a script that works under Cargo gains nothing.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything a build script asked for, in emission order.
///
/// Vectors keep the order the script printed, because `--cfg` and link
/// order can matter. Serialised as JSON next to the script's `OUT_DIR`
/// so a cached run can be replayed without rerunning the script.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildScriptOutput {
    /// `rustc-cfg`: each becomes `--cfg <value>` on the owning crate.
    pub cfgs: Vec<String>,
    /// `rustc-check-cfg`: each becomes `--check-cfg <value>`.
    pub check_cfgs: Vec<String>,
    /// `rustc-env`: set on the owning crate's rustc process.
    pub env: Vec<(String, String)>,
    /// `rustc-link-arg*`, with the crate kinds each applies to.
    pub link_args: Vec<LinkArg>,
    /// `rustc-link-lib` (and `-l` from `rustc-flags`), as
    /// `[KIND[:MODIFIERS]=]NAME[:RENAME]`.
    pub link_libs: Vec<String>,
    /// `rustc-link-search` (and `-L` from `rustc-flags`), as
    /// `[KIND=]PATH`. Propagates to every crate that depends on the
    /// owning crate.
    pub link_search: Vec<String>,
    /// `rerun-if-changed`, resolved against the manifest directory.
    pub rerun_if_changed: Vec<PathBuf>,
    /// `rerun-if-env-changed`.
    pub rerun_if_env_changed: Vec<String>,
    /// `metadata` (and unknown old-syntax keys), passed to direct
    /// dependents' scripts as `DEP_<LINKS>_<KEY>`.
    pub metadata: Vec<(String, String)>,
    /// `warning`, plus warnings quon adds about the output itself.
    pub warnings: Vec<String>,
    /// `error`. A non-empty list fails the run even if the script
    /// exited with status 0.
    pub errors: Vec<String>,
}

/// One `rustc-link-arg*` directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkArg {
    pub applies_to: LinkArgTarget,
    pub arg: String,
}

/// Which owning crates a link argument applies to. Libs never take
/// link arguments, as in Cargo. `-tests`, `-examples` and `-benches`
/// variants are dropped while parsing, because quon builds none of
/// those targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkArgTarget {
    /// `rustc-link-arg`: executables, `staticlib` and `cdylib` crates.
    All,
    /// `rustc-link-arg-bins`: every executable.
    Bins,
    /// `rustc-link-arg-bin=NAME=FLAG`: the executable named `NAME`.
    Bin(String),
    /// `rustc-link-arg-cdylib` and the old `rustc-cdylib-link-arg`.
    Cdylib,
}

/// A stdout line quon could not accept. The caller adds the crate,
/// version and platform; this names the line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("build script output line {line_no}: {kind}: `{line}`")]
pub struct DirectiveError {
    /// 1-based line number in stdout.
    pub line_no: usize,
    pub line: String,
    pub kind: DirectiveErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectiveErrorKind {
    #[error("unknown directive `cargo::{0}`")]
    UnknownKey(String),
    #[error("expected `cargo::KEY=VALUE`")]
    MissingValue,
    #[error("`{key}` expects `{expected}`")]
    Malformed {
        key: &'static str,
        expected: &'static str,
    },
    #[error("`rustc-flags` allows only -l and -L, found `{0}`")]
    BadRustcFlag(String),
}

/// Inputs the parser needs besides stdout.
#[derive(Debug, Clone, Copy)]
pub struct ParseContext<'a> {
    /// Package directory. Relative `rerun-if-changed` paths resolve
    /// against it, because that is the script's working directory.
    pub manifest_dir: &'a Path,
    /// Whether the package declares `links`. Metadata from a package
    /// without `links` has no `DEP_*` name to travel under, so it is
    /// dropped with a warning, as Cargo does.
    pub has_links: bool,
}

/// Parse a build script's whole stdout.
///
/// Stops at the first malformed line: a script whose output quon
/// cannot read must not have the rest of it applied.
pub fn parse_output(
    stdout: &str,
    ctx: ParseContext<'_>,
) -> Result<BuildScriptOutput, DirectiveError> {
    let mut out = BuildScriptOutput::default();
    for (i, raw) in stdout.lines().enumerate() {
        // `lines()` strips `\n` and `\r\n`; a lone trailing `\r` from a
        // script that wrote `\r\r\n` or mixed endings is still there.
        let line = raw.trim_end_matches('\r');
        parse_line(line, ctx, &mut out).map_err(|kind| DirectiveError {
            line_no: i + 1,
            line: line.to_string(),
            kind,
        })?;
    }
    Ok(out)
}

fn parse_line(
    line: &str,
    ctx: ParseContext<'_>,
    out: &mut BuildScriptOutput,
) -> Result<(), DirectiveErrorKind> {
    if let Some(rest) = line.strip_prefix("cargo::") {
        let (key, value) = rest
            .split_once('=')
            .ok_or(DirectiveErrorKind::MissingValue)?;
        match key {
            "error" => out.errors.push(value.to_string()),
            "metadata" => {
                let (k, v) = value.split_once('=').ok_or(DirectiveErrorKind::Malformed {
                    key: "metadata",
                    expected: "metadata=KEY=VALUE",
                })?;
                push_metadata(out, ctx, k, v);
            }
            _ => {
                if !apply_known(key, value, ctx, out)? {
                    return Err(DirectiveErrorKind::UnknownKey(key.to_string()));
                }
            }
        }
    } else if let Some(rest) = line.strip_prefix("cargo:") {
        // Cargo skips an old-syntax line without `=`; so does quon.
        let Some((key, value)) = rest.split_once('=') else {
            return Ok(());
        };
        if !apply_known(key, value, ctx, out)? {
            push_metadata(out, ctx, key, value);
        }
    }
    Ok(())
}

fn push_metadata(out: &mut BuildScriptOutput, ctx: ParseContext<'_>, key: &str, value: &str) {
    if ctx.has_links {
        out.metadata.push((key.to_string(), value.to_string()));
    } else {
        out.warnings.push(format!(
            "metadata `{key}` ignored: the package declares no `links` key"
        ));
    }
}

/// Apply a directive known to both syntaxes. Returns `Ok(false)` if
/// `key` is not one.
fn apply_known(
    key: &str,
    value: &str,
    ctx: ParseContext<'_>,
    out: &mut BuildScriptOutput,
) -> Result<bool, DirectiveErrorKind> {
    let link_arg = |applies_to| LinkArg {
        applies_to,
        arg: value.to_string(),
    };
    match key {
        "rustc-cfg" => out.cfgs.push(value.to_string()),
        "rustc-check-cfg" => out.check_cfgs.push(value.to_string()),
        "rustc-env" => {
            let (k, v) = value.split_once('=').ok_or(DirectiveErrorKind::Malformed {
                key: "rustc-env",
                expected: "rustc-env=VAR=VALUE",
            })?;
            out.env.push((k.to_string(), v.to_string()));
        }
        "rustc-link-arg" => out.link_args.push(link_arg(LinkArgTarget::All)),
        "rustc-link-arg-bins" => out.link_args.push(link_arg(LinkArgTarget::Bins)),
        "rustc-link-arg-cdylib" | "rustc-cdylib-link-arg" => {
            out.link_args.push(link_arg(LinkArgTarget::Cdylib));
        }
        "rustc-link-arg-bin" => {
            let (name, arg) = value.split_once('=').ok_or(DirectiveErrorKind::Malformed {
                key: "rustc-link-arg-bin",
                expected: "rustc-link-arg-bin=BIN=FLAG",
            })?;
            out.link_args.push(LinkArg {
                applies_to: LinkArgTarget::Bin(name.to_string()),
                arg: arg.to_string(),
            });
        }
        "rustc-link-arg-tests" | "rustc-link-arg-examples" | "rustc-link-arg-benches" => {}
        "rustc-link-lib" => out.link_libs.push(value.to_string()),
        "rustc-link-search" => out.link_search.push(value.to_string()),
        "rustc-flags" => parse_rustc_flags(value, out)?,
        "rerun-if-changed" => out.rerun_if_changed.push(ctx.manifest_dir.join(value)),
        "rerun-if-env-changed" => out.rerun_if_env_changed.push(value.to_string()),
        "warning" => out.warnings.push(value.to_string()),
        _ => return Ok(false),
    }
    Ok(true)
}

/// Split `rustc-flags` into `-l` and `-L` entries. Both the separated
/// (`-l foo`) and attached (`-lfoo`) forms are accepted, as in Cargo.
/// Any other flag is an error: Cargo restricts this directive to
/// library flags, and passing arbitrary rustc flags through it would
/// bypass the rest of the output model.
fn parse_rustc_flags(value: &str, out: &mut BuildScriptOutput) -> Result<(), DirectiveErrorKind> {
    let mut tokens = value.split_whitespace();
    while let Some(tok) = tokens.next() {
        let (flag, attached) = if let Some(rest) = tok.strip_prefix("-l") {
            ("-l", rest)
        } else if let Some(rest) = tok.strip_prefix("-L") {
            ("-L", rest)
        } else {
            return Err(DirectiveErrorKind::BadRustcFlag(tok.to_string()));
        };
        let arg = if attached.is_empty() {
            tokens.next().ok_or(DirectiveErrorKind::Malformed {
                key: "rustc-flags",
                expected: "a value after -l or -L",
            })?
        } else {
            attached
        };
        match flag {
            "-l" => out.link_libs.push(arg.to_string()),
            _ => out.link_search.push(arg.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = "/pkg";

    fn ctx(has_links: bool) -> ParseContext<'static> {
        ParseContext {
            manifest_dir: Path::new(DIR),
            has_links,
        }
    }

    fn parse(stdout: &str) -> BuildScriptOutput {
        parse_output(stdout, ctx(true)).expect("parses")
    }

    fn parse_err(stdout: &str) -> DirectiveError {
        parse_output(stdout, ctx(true)).expect_err("must fail")
    }

    #[test]
    fn both_syntaxes_map_known_keys_to_the_same_fields() {
        for prefix in ["cargo:", "cargo::"] {
            let stdout = [
                "rustc-cfg=has_foo",
                "rustc-cfg=level=\"2\"",
                "rustc-check-cfg=cfg(has_foo)",
                "rustc-env=GREETING=hi=there",
                "rustc-link-lib=static=foo",
                "rustc-link-search=native=/opt/lib",
                "rerun-if-changed=build.rs",
                "rerun-if-changed=/abs/file",
                "rerun-if-env-changed=CC",
                "warning=careful",
            ]
            .map(|l| format!("{prefix}{l}"))
            .join("\n");
            let out = parse(&stdout);
            assert_eq!(out.cfgs, vec!["has_foo", "level=\"2\""], "{prefix}");
            assert_eq!(out.check_cfgs, vec!["cfg(has_foo)"]);
            // Only the first `=` after the variable name splits.
            assert_eq!(out.env, vec![("GREETING".into(), "hi=there".into())]);
            assert_eq!(out.link_libs, vec!["static=foo"]);
            assert_eq!(out.link_search, vec!["native=/opt/lib"]);
            assert_eq!(
                out.rerun_if_changed,
                vec![PathBuf::from("/pkg/build.rs"), PathBuf::from("/abs/file")]
            );
            assert_eq!(out.rerun_if_env_changed, vec!["CC"]);
            assert_eq!(out.warnings, vec!["careful"]);
            assert!(out.metadata.is_empty());
        }
    }

    #[test]
    fn old_syntax_unknown_key_is_metadata() {
        let out = parse("cargo:root=/x\ncargo:include=/x/inc=1");
        assert_eq!(
            out.metadata,
            vec![
                ("root".into(), "/x".into()),
                ("include".into(), "/x/inc=1".into())
            ]
        );
    }

    #[test]
    fn error_and_metadata_are_new_syntax_only() {
        let out = parse("cargo:error=boom\ncargo:metadata=k=v");
        assert!(out.errors.is_empty());
        assert_eq!(
            out.metadata,
            vec![
                ("error".into(), "boom".into()),
                ("metadata".into(), "k=v".into())
            ]
        );

        let out = parse("cargo::error=boom\ncargo::metadata=key=4=2");
        assert_eq!(out.errors, vec!["boom"]);
        assert_eq!(out.metadata, vec![("key".into(), "4=2".into())]);
    }

    #[test]
    fn new_syntax_unknown_key_is_an_error_naming_the_line() {
        let err = parse_err("hello\ncargo::rustc-cfg=a\ncargo::bogus=1");
        assert_eq!(err.line_no, 3);
        assert_eq!(err.line, "cargo::bogus=1");
        assert_eq!(err.kind, DirectiveErrorKind::UnknownKey("bogus".into()));
        assert!(err.to_string().contains("cargo::bogus=1"), "{err}");
    }

    #[test]
    fn new_syntax_without_value_is_an_error_old_syntax_is_skipped() {
        assert_eq!(
            parse_err("cargo::rustc-cfg").kind,
            DirectiveErrorKind::MissingValue
        );
        assert_eq!(parse("cargo:rustc-cfg"), BuildScriptOutput::default());
    }

    #[test]
    fn malformed_values_are_errors() {
        for line in [
            "cargo::rustc-env=NOVALUE",
            "cargo::rustc-link-arg-bin=noflag",
            "cargo::metadata=noeq",
            "cargo:rustc-flags=-l",
        ] {
            let err = parse_err(line);
            assert!(
                matches!(err.kind, DirectiveErrorKind::Malformed { .. }),
                "{line}: {err:?}"
            );
        }
    }

    #[test]
    fn link_args_carry_their_crate_filter() {
        let out = parse(
            "cargo::rustc-link-arg=-Tlink.x\n\
             cargo::rustc-link-arg-bins=-nostartfiles\n\
             cargo::rustc-link-arg-bin=kernel=-Tkernel.ld=x\n\
             cargo::rustc-link-arg-cdylib=-soname\n\
             cargo:rustc-cdylib-link-arg=-old\n\
             cargo::rustc-link-arg-tests=-ignored\n\
             cargo::rustc-link-arg-examples=-ignored\n\
             cargo::rustc-link-arg-benches=-ignored",
        );
        let arg = |applies_to, arg: &str| LinkArg {
            applies_to,
            arg: arg.into(),
        };
        assert_eq!(
            out.link_args,
            vec![
                arg(LinkArgTarget::All, "-Tlink.x"),
                arg(LinkArgTarget::Bins, "-nostartfiles"),
                arg(LinkArgTarget::Bin("kernel".into()), "-Tkernel.ld=x"),
                arg(LinkArgTarget::Cdylib, "-soname"),
                arg(LinkArgTarget::Cdylib, "-old"),
            ]
        );
    }

    #[test]
    fn rustc_flags_split_into_libs_and_search_paths() {
        let out = parse("cargo:rustc-flags=-l foo -Lnative=/a -lstatic=bar  -L /b");
        assert_eq!(out.link_libs, vec!["foo", "static=bar"]);
        assert_eq!(out.link_search, vec!["native=/a", "/b"]);
    }

    #[test]
    fn rustc_flags_reject_other_flags() {
        let err = parse_err("cargo:rustc-flags=-l foo -C opt-level=3");
        assert_eq!(err.kind, DirectiveErrorKind::BadRustcFlag("-C".into()));
    }

    #[test]
    fn trailing_carriage_returns_and_non_directives_are_handled() {
        let out = parse(
            "compiling...\r\ncargo:rustc-cfg=a\r\r\nCARGO:rustc-cfg=b\n  cargo:rustc-cfg=c\n",
        );
        assert_eq!(out.cfgs, vec!["a"]);
    }

    #[test]
    fn metadata_without_links_is_dropped_with_a_warning() {
        let out = parse_output("cargo::metadata=k=v\ncargo:root=/x", ctx(false)).unwrap();
        assert!(out.metadata.is_empty());
        assert_eq!(out.warnings.len(), 2);
        assert!(out.warnings[0].contains("`k`"), "{:?}", out.warnings);
    }
}
