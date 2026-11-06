//! Questions a running `quon.rs` asks the quon process that started it.
//!
//! The script binary's stdout carries the final [`crate::RawBuildModel`]
//! JSON. A question is one stdout line that starts with
//! [`REQUEST_PREFIX`], followed by a [`ScriptRequest`] as JSON; quon
//! answers with one stdin line holding a [`ScriptReply`]. The model is
//! pretty-printed, so none of its lines starts with the prefix.
//!
//! The answers come from files quon reads at run time, so the script
//! binary never needs recompiling when they change. quon reruns the
//! script on every command and keeps none of its output; anything that
//! starts caching that output must add the files behind these answers
//! to its key.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Marks a stdout line of the script binary as a [`ScriptRequest`].
pub const REQUEST_PREFIX: &str = "quon-request ";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptRequest {
    /// The members of the Cargo workspace in `dir`, relative to the
    /// project root.
    Members { dir: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptReply {
    Members(Vec<WorkspaceMember>),
    Error(String),
}

/// A package of a Cargo workspace, as `quon.rs` sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceMember {
    /// The package name.
    pub name: String,
    /// The package directory relative to the workspace root, with `/`
    /// between components, such as `robots/sentry/chassis-board`.
    pub path: String,
    /// The package names in `[dependencies]`, platform-specific and
    /// optional ones included, after renames. Build and dev
    /// dependencies are left out.
    pub dependencies: BTreeSet<String>,
}

impl WorkspaceMember {
    #[must_use]
    pub fn new(name: String, path: String, dependencies: BTreeSet<String>) -> Self {
        Self {
            name,
            path,
            dependencies,
        }
    }

    /// Whether `[dependencies]` names the package `package`.
    #[must_use]
    pub fn depends_on(&self, package: &str) -> bool {
        self.dependencies.contains(package)
    }
}
