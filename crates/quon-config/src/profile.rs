//! Profiles: how crates are compiled.

use crate::opt::PresetRef;
use crate::quon::{Declare, Quon};
use crate::runner::Runner;

/// Link-time optimisation, passed to rustc as `-C lto=<mode>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lto {
    Off,
    Thin,
    Fat,
}

impl Lto {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Thin => "thin",
            Self::Fat => "fat",
        }
    }
}

/// A build profile, like a Cargo profile: optimisation, debug info,
/// LTO, and the preset of config options. Select one with
/// `quon -p <name>`, or `-r` for `release`.
///
/// Every project has `dev` and `release`, with the settings of
/// [`Profile::dev`] and [`Profile::release`], unless it adds its own
/// profile of that name. What `quon run` starts is set once with
/// [`Quon::run`], not per profile.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// q.add(Profile::release().lto(Lto::Thin));
/// let dev = q.profile("dev");
/// q.add(Profile::new("debug-log").inherits(&dev));
/// ```
#[derive(Debug, Clone)]
#[must_use = "a profile does nothing until it is passed to `q.add`"]
pub struct Profile {
    pub(crate) name: String,
    pub(crate) inherits: Option<String>,
    pub(crate) opt_level: Option<u8>,
    pub(crate) debug_info: Option<bool>,
    pub(crate) lto: Option<Lto>,
    pub(crate) preset: Option<String>,
    pub(crate) runner: Option<String>,
}

impl Profile {
    /// A profile that sets nothing. Unset settings come from the
    /// profile it [inherits](Self::inherits), else opt-level 0, no
    /// debug info and no LTO.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inherits: None,
            opt_level: None,
            debug_info: None,
            lto: None,
            preset: None,
            runner: None,
        }
    }

    /// `dev`: opt-level 0 with debug info.
    pub fn dev() -> Self {
        Self::new("dev").opt_level(0).debug_info(true)
    }

    /// `release`: inherits `dev`, at opt-level 3 without debug info.
    /// rustc turns debug assertions and overflow checks off above
    /// opt-level 0, as in Cargo's release profile.
    pub fn release() -> Self {
        let mut p = Self::new("release").opt_level(3).debug_info(false);
        p.inherits = Some("dev".into());
        p
    }

    /// Take every setting this profile leaves unset from `parent`.
    pub fn inherits(mut self, parent: &ProfileRef) -> Self {
        self.inherits = Some(parent.name.clone());
        self
    }

    /// rustc's `-C opt-level`, 0 to 3.
    pub fn opt_level(mut self, level: u8) -> Self {
        self.opt_level = Some(level);
        self
    }

    pub fn debug_info(mut self, enable: bool) -> Self {
        self.debug_info = Some(enable);
        self
    }

    pub fn lto(mut self, lto: Lto) -> Self {
        self.lto = Some(lto);
        self
    }

    /// Apply `preset`'s config option values.
    pub fn preset(mut self, preset: &PresetRef) -> Self {
        self.preset = Some(preset.name.clone());
        self
    }

    /// Run with `runner` instead of the one [`Quon::run`] names, for
    /// example to boot the same kernel under BIOS and UEFI.
    pub fn runner(mut self, runner: &Runner) -> Self {
        self.runner = Some(runner.name.clone());
        self
    }
}

impl Declare for Profile {
    type Handle = ProfileRef;
    fn declare(self, q: &mut Quon) -> ProfileRef {
        q.add_profile(self)
    }
}

/// A handle to a profile, from `q.add(Profile::..)` or
/// [`Quon::profile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRef {
    pub(crate) name: String,
}

impl ProfileRef {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}
