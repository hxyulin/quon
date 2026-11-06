//! Crates: workspace packages, crates declared without a manifest, and
//! external dependencies.

use crate::artifact::Artifact;
use crate::quon::{Declare, Quon};
use crate::target::Target;
use quon_model::{CrateType, DepSource, GitRef};
use std::collections::BTreeMap;

/// A Rust edition, for crates declared with [`Crate`]. A workspace
/// package takes its edition from its `Cargo.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Edition {
    E2015,
    E2018,
    E2021,
    #[default]
    E2024,
}

impl Edition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::E2015 => "2015",
            Self::E2018 => "2018",
            Self::E2021 => "2021",
            Self::E2024 => "2024",
        }
    }
}

/// What [`Package`] and [`Crate`] share. Lowered into one
/// `RawCrateDef` and one group per crate.
#[derive(Debug, Clone, Default)]
pub(crate) struct CrateDecl {
    pub name: String,
    pub path: String,
    /// `Some` for a workspace package.
    pub package: Option<PackageRoot>,
    pub crate_type: CrateType,
    pub root: Option<String>,
    pub edition: Edition,
    /// `None`: the project's default target, else the host.
    pub target: Option<Target>,
    pub deps: Vec<Dep>,
    pub linker_script: Option<String>,
    pub cfg_flags: Vec<String>,
    pub rustc_flags: Vec<String>,
    pub artifact_env: BTreeMap<String, Artifact>,
    pub link_config: bool,
    pub no_sysroot: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PackageRoot {
    pub features: Vec<String>,
    pub default_features: bool,
}

/// Methods shared by [`Package`] and [`Crate`].
macro_rules! crate_settings {
    () => {
        /// Build for `target` instead of the project's default target.
        #[must_use]
        pub fn target(mut self, target: &Target) -> Self {
            self.0.target = Some(target.clone());
            self
        }

        /// Link with the linker script at `path`, relative to the
        /// project root.
        #[must_use]
        pub fn linker_script(mut self, path: impl Into<String>) -> Self {
            self.0.linker_script = Some(path.into());
            self
        }

        /// Pass `--cfg <flag>` to rustc.
        #[must_use]
        pub fn cfg(mut self, flag: impl Into<String>) -> Self {
            self.0.cfg_flags.push(flag.into());
            self
        }

        /// Pass `flag` to rustc, after the flags Quon sets.
        #[must_use]
        pub fn rustc_flag(mut self, flag: impl Into<String>) -> Self {
            self.0.rustc_flags.push(flag.into());
            self
        }

        /// Set the environment variable `var`, at compile time, to the
        /// path of `artifact`'s output, and build `artifact` first. The
        /// crate can then embed it:
        ///
        /// ```ignore
        /// static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
        /// ```
        #[must_use]
        pub fn artifact_env(mut self, var: impl Into<String>, artifact: &Artifact) -> Self {
            self.0.artifact_env.insert(var.into(), artifact.clone());
            self
        }

        /// Build without Quon's sysroot and config crate, for targets
        /// that cannot build `core`, `alloc` and `compiler_builtins`.
        #[must_use]
        pub fn no_sysroot(mut self) -> Self {
            self.0.no_sysroot = true;
            self
        }
    };
}

/// A package in the Cargo workspace named by
/// [`Quon::cargo_workspace`]. Its dependencies, features, edition and
/// lib and bin targets come from its `Cargo.toml`. The builder takes
/// what Cargo has no concept of: the target, linker script, rustc
/// flags and [`artifact_env`](Self::artifact_env).
///
/// `q.add` returns the [`Artifact`] for the bin (or lib) named after
/// the package. Reach other bins with [`Artifact::bin`].
#[derive(Debug, Clone)]
#[must_use = "a package does nothing until it is passed to `q.add`"]
pub struct Package(pub(crate) CrateDecl);

impl Package {
    pub fn new(name: impl Into<String>) -> Self {
        Self(CrateDecl {
            name: name.into(),
            package: Some(PackageRoot {
                features: Vec::new(),
                default_features: true,
            }),
            ..CrateDecl::default()
        })
    }

    crate_settings!();

    /// Enable features, like `cargo build --features`.
    pub fn features<S: AsRef<str>>(mut self, features: impl IntoIterator<Item = S>) -> Self {
        if let Some(p) = &mut self.0.package {
            p.features
                .extend(features.into_iter().map(|s| s.as_ref().to_owned()));
        }
        self
    }

    /// Leave the `default` feature off, like
    /// `cargo build --no-default-features`.
    pub fn no_default_features(mut self) -> Self {
        if let Some(p) = &mut self.0.package {
            p.default_features = false;
        }
        self
    }

    /// Turn the package's bins into flat binaries: Quon links an ELF
    /// and copies it out with `objcopy -O binary`.
    pub fn raw_binary(mut self) -> Self {
        self.0.crate_type = CrateType::RawBinary;
        self
    }

    /// Link the generated config crate. A crate declared with
    /// [`Crate`] links it whenever it links the sysroot; a package
    /// links it only when asked, so its workspace dependencies don't.
    pub fn link_config(mut self) -> Self {
        self.0.link_config = true;
        self
    }
}

impl Declare for Package {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_crate(self.0)
    }
}

/// A crate Quon builds without a `Cargo.toml`: its sources, type and
/// dependencies are all declared here. Prefer [`Package`] for
/// anything in a Cargo workspace.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// let util = q.add(Crate::lib("util", "crates/util"));
/// q.add(Crate::bin("kernel", "crates/kernel").dep(&util));
/// ```
#[derive(Debug, Clone)]
#[must_use = "a crate does nothing until it is passed to `q.add`"]
pub struct Crate(pub(crate) CrateDecl);

impl Crate {
    /// A crate of type `crate_type` named `name`, with its sources in
    /// the directory `path` (relative to the project root).
    pub fn new(name: impl Into<String>, path: impl Into<String>, crate_type: CrateType) -> Self {
        Self(CrateDecl {
            name: name.into(),
            path: path.into(),
            crate_type,
            ..CrateDecl::default()
        })
    }

    /// A library, rooted at `src/lib.rs`.
    pub fn lib(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, path, CrateType::Lib)
    }

    /// A binary, rooted at `src/main.rs`.
    pub fn bin(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, path, CrateType::Bin)
    }

    /// A procedural macro crate, built for the host.
    pub fn proc_macro(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, path, CrateType::ProcMacro)
    }

    /// A flat binary: Quon links an ELF and copies it out with
    /// `objcopy -O binary`.
    pub fn raw_binary(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, path, CrateType::RawBinary)
    }

    crate_settings!();

    /// The crate root, relative to the crate directory, when it is not
    /// `src/lib.rs` or `src/main.rs`.
    pub fn root(mut self, path: impl Into<String>) -> Self {
        self.0.root = Some(path.into());
        self
    }

    pub fn edition(mut self, edition: Edition) -> Self {
        self.0.edition = edition;
        self
    }

    /// Depend on a crate or [`Dependency`]. Takes an `&Artifact` for a
    /// plain dependency, or a [`Dep`] to rename it or set features.
    pub fn dep(mut self, dep: impl Into<Dep>) -> Self {
        self.0.deps.push(dep.into());
        self
    }
}

impl Declare for Crate {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_crate(self.0)
    }
}

/// A dependency edge from a [`Crate`], with the options Cargo puts in
/// a `[dependencies]` entry.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// # let log = q.add(Crate::lib("log", "crates/log"));
/// Crate::bin("app", "crates/app").dep(Dep::on(&log).rename("logger").features(["serial"]));
/// ```
#[derive(Debug, Clone)]
#[must_use]
pub struct Dep {
    pub(crate) on: Artifact,
    pub(crate) rename: Option<String>,
    pub(crate) features: Vec<String>,
    pub(crate) version: Option<String>,
    pub(crate) optional: bool,
    pub(crate) default_features: bool,
}

impl Dep {
    pub fn on(artifact: &Artifact) -> Self {
        Self {
            on: artifact.clone(),
            rename: None,
            features: Vec::new(),
            version: None,
            optional: false,
            default_features: true,
        }
    }

    /// The name the dependent crate uses in `--extern`. Defaults to
    /// the dependency's name with `-` replaced by `_`.
    pub fn rename(mut self, name: impl Into<String>) -> Self {
        self.rename = Some(name.into());
        self
    }

    pub fn features<S: AsRef<str>>(mut self, features: impl IntoIterator<Item = S>) -> Self {
        self.features = features
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect();
        self
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Link the dependency only when a feature enables it.
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    pub fn no_default_features(mut self) -> Self {
        self.default_features = false;
        self
    }

    pub(crate) fn extern_name(&self) -> String {
        self.rename
            .clone()
            .unwrap_or_else(|| self.on.name.replace('-', "_"))
    }
}

impl From<&Artifact> for Dep {
    fn from(a: &Artifact) -> Self {
        Dep::on(a)
    }
}

/// A crate from crates.io, a git repository or a local path, built
/// by Quon outside any Cargo workspace. [`Crate::dep`] links it.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// let kernel = Target::builtin("x86_64-unknown-none");
/// let bitflags = q.add(Dependency::crates_io("bitflags", "2.11").target(&kernel));
/// # let _ = bitflags;
/// ```
#[derive(Debug, Clone)]
#[must_use = "a dependency does nothing until it is passed to `q.add`"]
pub struct Dependency {
    pub(crate) name: String,
    pub(crate) source: DepSource,
    pub(crate) features: Vec<String>,
    pub(crate) default_features: bool,
    pub(crate) cfg_flags: Vec<String>,
    pub(crate) rustc_flags: Vec<String>,
    pub(crate) target: Target,
    /// `.rev`/`.tag`/`.branch` on a source that is not git.
    pub(crate) misplaced_git_ref: bool,
}

impl Dependency {
    fn new(name: impl Into<String>, source: DepSource) -> Self {
        Self {
            name: name.into(),
            source,
            features: Vec::new(),
            default_features: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            target: Target::host(),
            misplaced_git_ref: false,
        }
    }

    /// `version` of `name` from crates.io.
    pub fn crates_io(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self::new(
            name,
            DepSource::CratesIo {
                version: version.into(),
            },
        )
    }

    /// The crate in the directory `path`, relative to the project root.
    pub fn path(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, DepSource::Path { path: path.into() })
    }

    /// The crate in the git repository at `url`, on its default branch
    /// unless [`rev`](Self::rev), [`tag`](Self::tag) or
    /// [`branch`](Self::branch) pins it.
    pub fn git(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::new(
            name,
            DepSource::Git {
                url: url.into(),
                reference: GitRef::Default,
            },
        )
    }

    pub fn rev(self, rev: impl Into<String>) -> Self {
        self.git_ref(GitRef::Rev(rev.into()))
    }

    pub fn tag(self, tag: impl Into<String>) -> Self {
        self.git_ref(GitRef::Tag(tag.into()))
    }

    pub fn branch(self, branch: impl Into<String>) -> Self {
        self.git_ref(GitRef::Branch(branch.into()))
    }

    /// Only a git source has a reference to pin; the type system can't
    /// see the source, so a pin on another source is reported by
    /// `Quon::finish`.
    fn git_ref(mut self, new: GitRef) -> Self {
        match &mut self.source {
            DepSource::Git { reference, .. } => *reference = new,
            _ => self.misplaced_git_ref = true,
        }
        self
    }

    pub fn features<S: AsRef<str>>(mut self, features: impl IntoIterator<Item = S>) -> Self {
        self.features = features
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect();
        self
    }

    pub fn no_default_features(mut self) -> Self {
        self.default_features = false;
        self
    }

    pub fn cfg(mut self, flag: impl Into<String>) -> Self {
        self.cfg_flags.push(flag.into());
        self
    }

    pub fn rustc_flag(mut self, flag: impl Into<String>) -> Self {
        self.rustc_flags.push(flag.into());
        self
    }

    /// Build for `target` rather than the host. Each dependency builds
    /// for one target; declare it twice, under two names, for both.
    pub fn target(mut self, target: &Target) -> Self {
        self.target = target.clone();
        self
    }
}

impl Declare for Dependency {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_dependency(self)
    }
}
