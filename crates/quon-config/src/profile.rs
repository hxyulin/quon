//! Profiles: how crates are compiled.

use crate::opt::PresetRef;
use crate::quon::{Declare, Quon};
use crate::runner::Runner;
use quon_model::OptLevel;

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

/// A value [`Profile::opt_level`] takes: a number from 0 to 3, `"s"`,
/// `"z"`, or an [`OptLevel`]. Another value is reported when `quon.rs`
/// finishes.
pub trait IntoOptLevel {
    #[doc(hidden)]
    fn into_opt_level(self) -> Result<OptLevel, String>;
}

impl IntoOptLevel for OptLevel {
    fn into_opt_level(self) -> Result<OptLevel, String> {
        Ok(self)
    }
}

impl IntoOptLevel for &str {
    fn into_opt_level(self) -> Result<OptLevel, String> {
        OptLevel::parse(self).ok_or_else(|| format!("\"{self}\""))
    }
}

// An unsuffixed literal such as `opt_level(2)` is an `i32`, so the
// signed type needs an impl too.
macro_rules! numbers_into_opt_level {
    ($($t:ty),*) => {$(
        impl IntoOptLevel for $t {
            fn into_opt_level(self) -> Result<OptLevel, String> {
                u64::try_from(self)
                    .ok()
                    .and_then(OptLevel::from_number)
                    .ok_or_else(|| self.to_string())
            }
        }
    )*};
}
numbers_into_opt_level!(i32, u8, u32);

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
    /// The level, or the value `opt_level` was given that is none.
    pub(crate) opt_level: Option<Result<OptLevel, String>>,
    pub(crate) debug_info: Option<bool>,
    pub(crate) debug_assertions: Option<bool>,
    pub(crate) overflow_checks: Option<bool>,
    pub(crate) lto: Option<Lto>,
    pub(crate) preset: Option<String>,
    pub(crate) runner: Option<String>,
}

impl Profile {
    /// A profile that sets nothing. Unset settings come from the
    /// profile it [inherits](Self::inherits), else opt-level 0, no
    /// debug info and no LTO, with debug assertions and overflow checks
    /// on only at opt-level 0 (rustc's default).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inherits: None,
            opt_level: None,
            debug_info: None,
            debug_assertions: None,
            overflow_checks: None,
            lto: None,
            preset: None,
            runner: None,
        }
    }

    /// `dev`: opt-level 0 with debug info, debug assertions and
    /// overflow checks, as in Cargo's dev profile. The checks stay on
    /// when the opt-level is raised: `Profile::dev().opt_level(1)`.
    pub fn dev() -> Self {
        Self::new("dev")
            .opt_level(0)
            .debug_info(true)
            .debug_assertions(true)
            .overflow_checks(true)
    }

    /// `release`: inherits `dev`, at opt-level 3 without debug info,
    /// debug assertions or overflow checks, as in Cargo's release
    /// profile.
    pub fn release() -> Self {
        let mut p = Self::new("release")
            .opt_level(3)
            .debug_info(false)
            .debug_assertions(false)
            .overflow_checks(false);
        p.inherits = Some("dev".into());
        p
    }

    /// Take every setting this profile leaves unset from `parent`.
    pub fn inherits(mut self, parent: &ProfileRef) -> Self {
        self.inherits = Some(parent.name.clone());
        self
    }

    /// rustc's `-C opt-level`: 0 to 3, `"s"` to optimize for size, or
    /// `"z"` to optimize for size more aggressively.
    ///
    /// ```
    /// # use quon_config::prelude::*;
    /// let fw = Profile::release().opt_level("s");
    /// let dev = Profile::dev().opt_level(1);
    /// # let _ = (fw, dev);
    /// ```
    pub fn opt_level(mut self, level: impl IntoOptLevel) -> Self {
        self.opt_level = Some(level.into_opt_level());
        self
    }

    pub fn debug_info(mut self, enable: bool) -> Self {
        self.debug_info = Some(enable);
        self
    }

    /// rustc's `-C debug-assertions`: `debug_assert!` and
    /// `cfg(debug_assertions)`, in this profile's target crates. Build
    /// scripts and their dependencies keep a fixed host profile.
    pub fn debug_assertions(mut self, enable: bool) -> Self {
        self.debug_assertions = Some(enable);
        self
    }

    /// rustc's `-C overflow-checks`: panic on integer overflow. Unset,
    /// it follows [`debug_assertions`](Self::debug_assertions).
    pub fn overflow_checks(mut self, enable: bool) -> Self {
        self.overflow_checks = Some(enable);
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
