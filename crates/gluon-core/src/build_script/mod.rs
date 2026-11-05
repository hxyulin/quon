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
//! `crate::unit::lower` creates a script unit and its runs for every
//! vendored crate with a build script. The scheduler runs each one as a
//! `RunBuildScript` node and passes its output to the owner's rustc
//! command (`compile::command_builder`). Crates declared in `gluon.rs`
//! and manifest packages do not run build scripts yet.

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
