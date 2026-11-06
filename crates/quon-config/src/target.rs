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
/// let firmware = Target::builtin("thumbv7em-none-eabihf").build_std();
/// let custom = Target::spec("x86_64-myos", "targets/x86_64-myos.json");
/// let host = Target::host();
/// # let _ = (kernel, firmware, custom, host);
/// ```
///
/// ### Flags for every crate
///
/// [`Target::rustc_flag`] passes a flag to every crate built for the
/// target, dependencies included, as Cargo's
/// `[target.<triple>] rustflags` does. [`Target::named`] gives a
/// builtin triple a name of its own, so one triple can be several
/// targets with different flags. Each named target resolves features,
/// builds and stores its crates separately:
///
/// ```
/// use quon_config::Target;
///
/// let m7 = Target::builtin("thumbv7em-none-eabihf")
///     .named("cortex-m7")
///     .rustc_flag("-Ctarget-cpu=cortex-m7");
/// let m4 = Target::builtin("thumbv7em-none-eabihf")
///     .named("cortex-m4")
///     .rustc_flag("-Ctarget-cpu=cortex-m4");
/// # let _ = (m7, m4);
/// ```
///
/// ### Sysroot
///
/// A builtin target links the toolchain's prebuilt `core`,
/// `compiler_builtins` and `alloc`, as Cargo does, so the target must
/// be installed (`rustup target add <triple>`). [`Target::build_std`]
/// builds them from the `rust-src` component instead. A custom target
/// has no prebuilt libraries, so Quon always builds them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub(crate) kind: TargetKind,
    pub(crate) build_std: bool,
    /// From [`Target::named`].
    pub(crate) name: Option<String>,
    pub(crate) rustc_flags: Vec<String>,
}

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
        Self::of(TargetKind::Host)
    }

    /// A target rustc knows, such as `x86_64-unknown-none` or
    /// `thumbv7em-none-eabihf`.
    #[must_use]
    pub fn builtin(triple: impl Into<String>) -> Self {
        Self::of(TargetKind::Builtin(triple.into()))
    }

    /// A custom target named `name`, defined by the JSON spec at
    /// `path` (relative to the project root).
    #[must_use]
    pub fn spec(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::of(TargetKind::Spec {
            name: name.into(),
            path: path.into(),
        })
    }

    fn of(kind: TargetKind) -> Self {
        Self {
            kind,
            build_std: false,
            name: None,
            rustc_flags: Vec::new(),
        }
    }

    /// Build `core`, `compiler_builtins` and `alloc` for this builtin
    /// target from the `rust-src` component, instead of linking the
    /// toolchain's prebuilt ones. Use it for a target rustup does not
    /// ship, or to compile those crates with the profile's settings.
    /// A custom target always builds them; the host never does, and
    /// asking for it there is an error.
    #[must_use]
    pub fn build_std(mut self) -> Self {
        self.build_std = true;
        self
    }

    /// Name the target `name` instead of after its triple, as
    /// `quon build -t <name>` and the build directory
    /// (`build/cross/<name>/`) call it. Two targets with one triple
    /// and different [`rustc_flag`](Self::rustc_flag)s need different
    /// names: a name is defined once, and crates of different targets
    /// never share an artifact. Rustc still gets the triple, and
    /// `[target.<triple>]` manifest keys still match it.
    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Pass `flag` to rustc for every crate built for this target, such
    /// as `-Ctarget-cpu=cortex-m7`: workspace crates, dependencies, and
    /// a sysroot Quon builds. Build scripts see it in
    /// `CARGO_ENCODED_RUSTFLAGS`, and Quon's `rustc --print cfg` probe
    /// gets it too, since a flag can change the target's cfgs. A
    /// prebuilt sysroot was compiled without it, as with Cargo. Call it
    /// once per flag.
    ///
    /// [`Package::rustc_flag`](crate::Package::rustc_flag) reaches only
    /// the package's own crate.
    #[must_use]
    pub fn rustc_flag(mut self, flag: impl Into<String>) -> Self {
        self.rustc_flags.push(flag.into());
        self
    }

    /// The target's name: its [`named`](Self::named) name, else the
    /// triple, the spec's name, or `host`.
    #[must_use]
    pub fn name(&self) -> &str {
        if let Some(name) = &self.name {
            return name;
        }
        match &self.kind {
            TargetKind::Host => HOST,
            TargetKind::Builtin(triple) => triple,
            TargetKind::Spec { name, .. } => name,
        }
    }

    #[must_use]
    pub fn is_host(&self) -> bool {
        self.kind == TargetKind::Host
    }
}
