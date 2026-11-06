//! Compile targets.

/// A compile target: the host, a rustc builtin triple, or a custom
/// JSON target spec.
///
/// A `Target` is a plain value. Quon registers it the first time a
/// declaration uses it, so the same target can be named from several
/// files without coordination.
///
/// ```
/// use quon_config::Target;
///
/// let kernel = Target::builtin("x86_64-unknown-none");
/// let custom = Target::spec("x86_64-myos", "targets/x86_64-myos.json");
/// let host = Target::host();
/// # let _ = (kernel, custom, host);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target(pub(crate) TargetKind);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TargetKind {
    Host,
    Builtin(String),
    Spec { name: String, path: String },
}

/// The name the raw model uses for the host target.
pub(crate) const HOST: &str = "host";

impl Target {
    /// The machine Quon runs on. Crates built for it link the host's
    /// standard library, and Quon builds no sysroot for them.
    #[must_use]
    pub fn host() -> Self {
        Self(TargetKind::Host)
    }

    /// A target rustc knows, such as `x86_64-unknown-none` or
    /// `thumbv7em-none-eabihf`.
    #[must_use]
    pub fn builtin(triple: impl Into<String>) -> Self {
        Self(TargetKind::Builtin(triple.into()))
    }

    /// A custom target named `name`, defined by the JSON spec at
    /// `path` (relative to the project root).
    #[must_use]
    pub fn spec(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self(TargetKind::Spec {
            name: name.into(),
            path: path.into(),
        })
    }

    /// The target's name: the triple, the spec's name, or `host`.
    #[must_use]
    pub fn name(&self) -> &str {
        match &self.0 {
            TargetKind::Host => HOST,
            TargetKind::Builtin(triple) => triple,
            TargetKind::Spec { name, .. } => name,
        }
    }

    #[must_use]
    pub fn is_host(&self) -> bool {
        self.0 == TargetKind::Host
    }
}
