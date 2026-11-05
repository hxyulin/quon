//! Build-script (`build.rs`) support: the pure parts.
//!
//! A vendored or path crate's build script is compiled for the host,
//! run with the environment Cargo would give it, and its stdout is
//! parsed for `cargo:` directives that change the owning crate's rustc
//! invocation.
//!
//! - [`directive`]: parse a script's stdout into a [`BuildScriptOutput`].
//! - [`env`]: assemble the environment a script runs with.
//! - [`run`]: run a compiled script as a `RunBuildScript` node, with a
//!   fingerprint record that stores the parsed output.
//!
//! The scheduler has the run node and passes its output to the owner's
//! rustc command (`compile::command_builder`). No unit has a run yet:
//! lowering does not create them, and vendored crates with a build
//! script are still rejected by `vendor::cross::graph::build_vendor_nodes`.

pub mod directive;
pub mod env;
pub mod run;

pub use directive::{
    BuildScriptOutput, DirectiveError, DirectiveErrorKind, LinkArg, LinkArgTarget, ParseContext,
    parse_output,
};
pub use env::{
    PackageInfo, ScriptEnvInput, ScriptPlatform, ScriptTarget, script_env, should_remove_inherited,
};
