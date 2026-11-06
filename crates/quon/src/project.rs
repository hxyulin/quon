//! [`Project`]: a read-only view of a project's `quon.rs`, for tools
//! that drive Quon through [`crate::run`] and need to know what it
//! builds and where.

use crate::cmd;
use quon_core::config::RunSelection;
use std::fmt;
use std::path::{Path, PathBuf};

/// An error from [`Project`]. Its `Display` gives the message with its
/// causes, as `quon` prints them.
pub struct Error(anyhow::Error);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for Error {}

fn err(e: impl Into<anyhow::Error>) -> Error {
    Error(e.into())
}

/// A run entry: one `q.run(..)` call in `quon.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// `.name(..)`, which commands such as `quon build <name>` take.
    /// Only a project's single entry may have none.
    pub name: Option<String>,
    /// The `Package` or `Crate` declaration the binary belongs to: for
    /// a `Package`, the Cargo package name.
    pub package: String,
    /// The binary crate `quon run` starts.
    pub binary: String,
    /// The name of the target the entry builds for.
    pub target: String,
}

/// A project's `quon.rs`, evaluated once. Loading builds nothing.
#[derive(Debug)]
pub struct Project {
    root: PathBuf,
    entries: Vec<Entry>,
}

impl Project {
    /// Find `quon.rs` in `dir` or the nearest parent, as `quon` does,
    /// and evaluate it.
    pub fn load(dir: &Path) -> Result<Project, Error> {
        let dir = std::path::absolute(dir).map_err(err)?;
        let root = quon_core::find_project_root(&dir).map_err(err)?;
        let raw = quon_core::load_raw_build_model(&root).map_err(err)?;
        let entries = raw
            .runs
            .into_iter()
            .map(|r| Entry {
                name: r.name,
                package: r.owner,
                binary: r.binary,
                target: r.target,
            })
            .collect();
        Ok(Project { root, entries })
    }

    /// The directory holding `quon.rs`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The run entries, in declaration order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry named `name`.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.name.as_deref() == Some(name))
    }

    /// Where `quon build <entry> -p <profile>` writes `entry`'s binary,
    /// the file `quon run` starts. It reads the overrides in
    /// `.quon-config` and `QUON_*` variables as `quon` does, and builds
    /// nothing, so the file may be missing or stale.
    pub fn artifact(&self, entry: &Entry, profile: &str) -> Result<PathBuf, Error> {
        let selection = match &entry.name {
            Some(name) => RunSelection::Named(name.clone()),
            None => RunSelection::Only,
        };
        let ctx =
            cmd::build_context(&self.root, Some(profile), None, None, &selection).map_err(Error)?;
        quon_core::run::run_binary_path(&ctx.ctx, &ctx.model, &ctx.resolved).map_err(err)
    }
}
