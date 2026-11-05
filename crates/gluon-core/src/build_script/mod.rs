//! Build-script (`build.rs`) support: the pure parts.
//!
//! A vendored or path crate's build script is compiled for the host,
//! run with the environment Cargo would give it, and its stdout is
//! parsed for `cargo:` directives that change the owning crate's rustc
//! invocation. This module holds the two pieces of that which need no
//! I/O:
//!
//! - [`directive`]: parse a script's stdout into a [`BuildScriptOutput`].
//! - [`env`]: assemble the environment a script runs with.
//!
//! Compiling and running scripts, their scheduler nodes, and run-node
//! freshness are not wired up yet. Vendored crates with a build script
//! are still rejected by `vendor::cross::graph::build_vendor_nodes`.

pub mod directive;
pub mod env;

pub use directive::{
    BuildScriptOutput, DirectiveError, DirectiveErrorKind, LinkArg, LinkArgTarget, ParseContext,
    parse_output,
};
pub use env::{
    PackageInfo, ScriptEnvInput, ScriptPlatform, ScriptTarget, script_env, should_remove_inherited,
};
