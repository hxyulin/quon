//! Config options and presets.
//!
//! An option becomes a constant in the generated `<project>_config`
//! crate (a list option has none). Declare one with the function for its type, and add it:
//!
//! ```
//! use quon_config::prelude::*;
//!
//! # let mut q = Quon::new();
//! let log = q.add(opt::bool("LOG_ENABLED").default(true).help("Enable logging"));
//! let debug = q.add(opt::bool("DEBUG"));
//! let level = q.add(opt::u32("LOG_LEVEL").default(3).range(0..=5).depends_on(&log));
//! q.add(opt::bool("FANCY").depends_on(&log & !&debug));
//!
//! q.add(Preset::new("verbose").set(&level, 5).set(&debug, true));
//! ```
//!
//! Options can also come from a `.kconfig` file through
//! [`Quon::include_kconfig`]. Refer to those by name, with
//! [`Cond::named`] and [`Preset::set_named`].

use crate::quon::{Declare, Quon};
use quon_model::{Binding, ConfigOptionDef, ConfigType, ConfigValue, Expr, TristateVal};
use std::marker::PhantomData;
use std::ops::{BitAnd, BitOr, Not, RangeInclusive};

/// The type of a config option: [`Bool`], [`Tristate`], [`U32`],
/// [`U64`], [`Str`], [`Choice`] or [`List`].
pub trait Kind: sealed::Sealed {
    #[doc(hidden)]
    const TYPE: ConfigType;
}

/// A value an option of kind `K` accepts: `bool` for [`Bool`], `u32`
/// for [`U32`], a string for [`Str`] and [`Choice`], and so on. One
/// impl per integer kind lets a literal like `3` infer its type.
pub trait IntoValue<K: Kind> {
    #[doc(hidden)]
    fn into_value(self) -> ConfigValue;
}

mod sealed {
    pub trait Sealed {}
}

/// An option a condition can test: [`Bool`] or [`Tristate`].
pub trait Switch: Kind {}

macro_rules! kinds {
    ($($(#[$doc:meta])* $kind:ident => $ty:ident;)*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $kind;
        impl sealed::Sealed for $kind {}
        impl Kind for $kind {
            const TYPE: ConfigType = ConfigType::$ty;
        }
    )*};
}

kinds! {
    /// `bool`.
    Bool => Bool;
    /// Yes, module or no.
    Tristate => Tristate;
    /// `u32`.
    U32 => U32;
    /// `u64`.
    U64 => U64;
    /// A string.
    Str => Str;
    /// One of a fixed set of names.
    Choice => Choice;
    /// A list of strings. It has no constant in the config crate.
    List => List;
}

macro_rules! values {
    ($($kind:ident: $($t:ty),+ => |$v:ident| $wrap:expr;)*) => {$($(
        impl IntoValue<$kind> for $t {
            fn into_value(self) -> ConfigValue {
                let $v = self;
                $wrap
            }
        }
    )+)*};
}

values! {
    Bool: bool => |v| ConfigValue::Bool(v);
    Tristate: TristateVal => |v| ConfigValue::Tristate(v);
    U32: u32 => |v| ConfigValue::U32(v);
    U64: u64 => |v| ConfigValue::U64(v);
    Str: &str => |v| ConfigValue::Str(v.to_string());
    Str: String => |v| ConfigValue::Str(v);
    Choice: &str => |v| ConfigValue::Choice(v.to_string());
    Choice: String => |v| ConfigValue::Choice(v);
    List: Vec<String> => |v| ConfigValue::List(v);
    List: &[&str] => |v| ConfigValue::List(v.iter().map(|s| (*s).to_string()).collect());
}

impl Switch for Bool {}
impl Switch for Tristate {}

/// A config option of type `K`, built by [`bool`], [`u32`] and the
/// other functions in this module.
#[derive(Debug, Clone)]
#[must_use = "an option does nothing until it is passed to `q.add`"]
pub struct Opt<K: Kind> {
    pub(crate) def: ConfigOptionDef,
    kind: PhantomData<K>,
}

fn new<K: Kind>(name: impl Into<String>, default: ConfigValue) -> Opt<K> {
    Opt {
        def: ConfigOptionDef {
            name: name.into(),
            ty: K::TYPE,
            default,
            help: None,
            selects: Vec::new(),
            range: None,
            choices: None,
            menu: None,
            bindings: Vec::new(),
            span: None,
            depends_on_expr: None,
            visible_if_expr: None,
        },
        kind: PhantomData,
    }
}

/// A boolean option, `false` by default.
pub fn bool(name: impl Into<String>) -> Opt<Bool> {
    new(name, ConfigValue::Bool(false))
}

/// A tristate option, no by default.
pub fn tristate(name: impl Into<String>) -> Opt<Tristate> {
    new(name, ConfigValue::Tristate(TristateVal::No))
}

/// A `u32` option, 0 by default.
pub fn u32(name: impl Into<String>) -> Opt<U32> {
    new(name, ConfigValue::U32(0))
}

/// A `u64` option, 0 by default.
pub fn u64(name: impl Into<String>) -> Opt<U64> {
    new(name, ConfigValue::U64(0))
}

/// A string option, empty by default.
pub fn str(name: impl Into<String>) -> Opt<Str> {
    new(name, ConfigValue::Str(String::new()))
}

/// An option whose value is one of `variants`, the first by default.
pub fn choice<S: AsRef<str>>(
    name: impl Into<String>,
    variants: impl IntoIterator<Item = S>,
) -> Opt<Choice> {
    let variants: Vec<String> = variants
        .into_iter()
        .map(|s| s.as_ref().to_owned())
        .collect();
    let first = variants.first().cloned().unwrap_or_default();
    let mut o = new(name, ConfigValue::Choice(first));
    o.def.choices = Some(variants);
    o
}

/// A list-of-strings option, empty by default.
pub fn list(name: impl Into<String>) -> Opt<List> {
    new(name, ConfigValue::List(Vec::new()))
}

impl<K: Kind> Opt<K> {
    /// The value when no preset or override sets one.
    pub fn default(mut self, value: impl IntoValue<K>) -> Self {
        self.def.default = value.into_value();
        self
    }

    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.def.help = Some(text.into());
        self
    }

    /// The menu the option belongs to, for tools that list options.
    pub fn menu(mut self, name: impl Into<String>) -> Self {
        self.def.menu = Some(name.into());
        self
    }

    /// Turn the option off unless `cond` holds.
    pub fn depends_on(mut self, cond: impl Into<Cond>) -> Self {
        self.def.depends_on_expr = Some(cond.into().0);
        self
    }

    /// Turn `other` on whenever this option is on.
    pub fn selects<S: Switch>(mut self, other: &OptRef<S>) -> Self {
        self.def.selects.push(other.name.clone());
        self
    }

    /// Also expose the option through `binding`. Options are constants
    /// by default.
    pub fn binding(mut self, binding: Binding) -> Self {
        self.def.bindings.push(binding);
        self
    }
}

impl Opt<U32> {
    pub fn range(mut self, range: RangeInclusive<u32>) -> Self {
        self.def.range = Some((u64::from(*range.start()), u64::from(*range.end())));
        self
    }
}

impl Opt<U64> {
    pub fn range(mut self, range: RangeInclusive<u64>) -> Self {
        self.def.range = Some((*range.start(), *range.end()));
        self
    }
}

impl<K: Kind> Declare for Opt<K> {
    type Handle = OptRef<K>;
    fn declare(self, q: &mut Quon) -> OptRef<K> {
        let name = self.def.name.clone();
        q.add_option(self.def);
        OptRef {
            name,
            kind: PhantomData,
        }
    }
}

/// A handle to a config option of type `K`.
#[derive(Debug)]
pub struct OptRef<K> {
    pub(crate) name: String,
    kind: PhantomData<K>,
}

// Derived `Clone` would require `K: Clone`.
impl<K> Clone for OptRef<K> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            kind: PhantomData,
        }
    }
}

impl<K> OptRef<K> {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A condition on config options, for [`Opt::depends_on`]. Combine
/// handles with `&`, `|` and `!`: `&log & !&debug`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cond(pub(crate) Expr);

impl Cond {
    /// The option `name`, for one declared in a `.kconfig` file.
    pub fn named(name: impl Into<String>) -> Self {
        Self(Expr::Ident(name.into()))
    }
}

impl<S: Switch> From<&OptRef<S>> for Cond {
    fn from(o: &OptRef<S>) -> Self {
        Self::named(o.name.clone())
    }
}

impl From<bool> for Cond {
    fn from(b: bool) -> Self {
        Self(Expr::Const(b))
    }
}

/// Join two conditions with `and` (else `or`), flattening nested
/// clauses of the same operator so `a & b & c` is one clause.
fn join(and: bool, a: Cond, b: Cond) -> Cond {
    let mut v = Vec::new();
    for e in [a.0, b.0] {
        match e {
            Expr::And(xs) if and => v.extend(xs),
            Expr::Or(xs) if !and => v.extend(xs),
            e => v.push(e),
        }
    }
    Cond(if and { Expr::And(v) } else { Expr::Or(v) })
}

impl<R: Into<Cond>> BitAnd<R> for Cond {
    type Output = Cond;
    fn bitand(self, rhs: R) -> Cond {
        join(true, self, rhs.into())
    }
}

impl<R: Into<Cond>> BitOr<R> for Cond {
    type Output = Cond;
    fn bitor(self, rhs: R) -> Cond {
        join(false, self, rhs.into())
    }
}

impl Not for Cond {
    type Output = Cond;
    fn not(self) -> Cond {
        Cond(Expr::Not(Box::new(self.0)))
    }
}

impl<S: Switch, R: Into<Cond>> BitAnd<R> for &OptRef<S> {
    type Output = Cond;
    fn bitand(self, rhs: R) -> Cond {
        Cond::from(self) & rhs
    }
}

impl<S: Switch, R: Into<Cond>> BitOr<R> for &OptRef<S> {
    type Output = Cond;
    fn bitor(self, rhs: R) -> Cond {
        Cond::from(self) | rhs
    }
}

impl<S: Switch> Not for &OptRef<S> {
    type Output = Cond;
    fn not(self) -> Cond {
        !Cond::from(self)
    }
}

/// A named set of config option values, applied by
/// [`Profile::preset`](crate::Profile::preset).
#[derive(Debug, Clone)]
#[must_use = "a preset does nothing until it is passed to `q.add`"]
pub struct Preset {
    pub(crate) name: String,
    pub(crate) inherits: Option<String>,
    pub(crate) help: Option<String>,
    pub(crate) values: Vec<(String, ConfigValue)>,
}

impl Preset {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inherits: None,
            help: None,
            values: Vec::new(),
        }
    }

    pub fn set<K: Kind>(mut self, option: &OptRef<K>, value: impl IntoValue<K>) -> Self {
        self.values.push((option.name.clone(), value.into_value()));
        self
    }

    /// Set the option `name`, for one declared in a `.kconfig` file.
    /// Quon checks the value's type when it resolves the preset.
    pub fn set_named(mut self, name: impl Into<String>, value: ConfigValue) -> Self {
        self.values.push((name.into(), value));
        self
    }

    /// Start from `parent`'s values.
    pub fn inherits(mut self, parent: &PresetRef) -> Self {
        self.inherits = Some(parent.name.clone());
        self
    }

    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.help = Some(text.into());
        self
    }
}

impl Declare for Preset {
    type Handle = PresetRef;
    fn declare(self, q: &mut Quon) -> PresetRef {
        q.add_preset(self)
    }
}

/// A handle to a preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetRef {
    pub(crate) name: String,
}
