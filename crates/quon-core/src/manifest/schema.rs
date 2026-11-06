//! Serde structs for the subset of `Cargo.toml` that Quon reads.
//!
//! Quon owns these structs instead of using `cargo-util-schemas`: the
//! subset is small, and owning them gives `toml::Spanned` positions on
//! the values diagnostics point at (dependency entries, feature values,
//! target tables). Keys Quon does not read are ignored by serde, as
//! Cargo ignores them with a warning.
//!
//! `toml::Spanned` cannot sit inside an `#[serde(untagged)]` enum (the
//! untagged buffer drops the span), so spans wrap untagged values from
//! the outside. Spans on implicit tables (`[target.'cfg(x)'.dependencies]`)
//! fail to deserialize in toml 0.8, so target tables carry no span of
//! their own; diagnostics about them point at their first entry. For the
//! same reason a dependency's span is its key's: a dotted entry
//! (`x.workspace = true`) is an implicit table, and only its key can
//! carry a span.

use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use toml::Spanned;

pub type DepTable = BTreeMap<String, Spanned<TomlDependency>>;

/// Read a dependency table with each entry spanned by its key, so dotted
/// entries (`x.workspace = true`) parse like inline ones.
fn dep_table<'de, D: Deserializer<'de>>(d: D) -> Result<DepTable, D::Error> {
    let raw = BTreeMap::<Spanned<String>, TomlDependency>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|(key, dep)| {
            let span = key.span();
            (key.into_inner(), Spanned::new(span, dep))
        })
        .collect())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlManifest {
    pub package: Option<TomlPackage>,
    pub workspace: Option<TomlWorkspace>,
    pub lib: Option<TomlLib>,
    #[serde(default)]
    pub bin: Vec<TomlBin>,
    #[serde(default)]
    pub test: Vec<TomlTest>,
    #[serde(default, deserialize_with = "dep_table")]
    pub dependencies: DepTable,
    #[serde(default, alias = "dev_dependencies", deserialize_with = "dep_table")]
    pub dev_dependencies: DepTable,
    #[serde(default, alias = "build_dependencies", deserialize_with = "dep_table")]
    pub build_dependencies: DepTable,
    #[serde(default)]
    pub target: BTreeMap<String, TomlPlatform>,
    #[serde(default)]
    pub features: BTreeMap<String, Vec<Spanned<String>>>,
    pub lints: Option<TomlLints>,
    pub patch: Option<toml::Value>,
    pub replace: Option<toml::Value>,
    pub profile: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlPlatform {
    #[serde(default, deserialize_with = "dep_table")]
    pub dependencies: DepTable,
    #[serde(default, alias = "dev_dependencies", deserialize_with = "dep_table")]
    pub dev_dependencies: DepTable,
    #[serde(default, alias = "build_dependencies", deserialize_with = "dep_table")]
    pub build_dependencies: DepTable,
}

/// A value that may be inherited from `[workspace.package]` with
/// `field.workspace = true`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Inheritable<T> {
    Value(T),
    Workspace { workspace: bool },
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlPackage {
    pub name: String,
    pub version: Option<Inheritable<String>>,
    pub edition: Option<Inheritable<String>>,
    pub authors: Option<Inheritable<Vec<String>>>,
    pub description: Option<Inheritable<String>>,
    pub homepage: Option<Inheritable<String>>,
    pub repository: Option<Inheritable<String>>,
    pub license: Option<Inheritable<String>>,
    pub license_file: Option<Inheritable<String>>,
    pub readme: Option<Inheritable<TomlReadme>>,
    pub rust_version: Option<Inheritable<String>>,
    pub links: Option<String>,
    pub build: Option<TomlBuild>,
    pub autobins: Option<bool>,
    pub autolib: Option<bool>,
    pub autotests: Option<bool>,
}

/// `readme = "README.md"` or `readme = false`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TomlReadme {
    Path(String),
    Flag(bool),
}

/// `build = "build.rs"` or `build = false`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TomlBuild {
    Path(String),
    Flag(bool),
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlWorkspace {
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub resolver: Option<String>,
    pub package: Option<TomlWorkspacePackage>,
    #[serde(default, deserialize_with = "dep_table")]
    pub dependencies: DepTable,
    pub lints: Option<BTreeMap<String, BTreeMap<String, TomlLint>>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlWorkspacePackage {
    pub version: Option<String>,
    pub edition: Option<String>,
    pub authors: Option<Vec<String>>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub readme: Option<TomlReadme>,
    pub rust_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TomlDependency {
    /// `foo = "1.2"`
    Simple(String),
    Detailed(DetailedDependency),
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DetailedDependency {
    pub version: Option<String>,
    pub path: Option<String>,
    pub git: Option<String>,
    /// With `git`: the commit, branch or tag Cargo checks out. Quon
    /// builds the commit `Cargo.lock` records, and checks that it was
    /// locked for this reference.
    pub rev: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub registry: Option<String>,
    pub package: Option<String>,
    pub workspace: Option<bool>,
    pub optional: Option<bool>,
    #[serde(alias = "default_features")]
    pub default_features: Option<bool>,
    pub features: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlLib {
    pub name: Option<String>,
    pub path: Option<String>,
    #[serde(alias = "crate_type")]
    pub crate_type: Option<Vec<String>>,
    #[serde(alias = "proc_macro")]
    pub proc_macro: Option<bool>,
    pub edition: Option<String>,
    pub test: Option<bool>,
    pub doctest: Option<bool>,
    pub bench: Option<bool>,
    pub harness: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlBin {
    pub name: String,
    pub path: Option<String>,
    #[serde(default, alias = "required_features")]
    pub required_features: Vec<String>,
    pub edition: Option<String>,
    pub test: Option<bool>,
    pub harness: Option<bool>,
}

/// One `[[test]]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlTest {
    pub name: String,
    pub path: Option<String>,
    #[serde(default, alias = "required_features")]
    pub required_features: Vec<String>,
    pub edition: Option<String>,
    pub test: Option<bool>,
    pub harness: Option<bool>,
}

/// `[lints]`: either `workspace = true` or per-tool tables.
#[derive(Debug, Default, Deserialize)]
pub struct TomlLints {
    pub workspace: Option<bool>,
    #[serde(flatten)]
    pub tools: BTreeMap<String, BTreeMap<String, TomlLint>>,
}

/// `lint = "warn"` or `lint = { level = "warn", priority = -1 }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TomlLint {
    Level(String),
    Detailed {
        level: String,
        #[serde(default)]
        priority: i8,
        #[serde(default, rename = "check-cfg")]
        check_cfg: Vec<String>,
    },
}
