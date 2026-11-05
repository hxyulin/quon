//! Chainable builders shared by `Config::profile`, `Config::group`, etc.

pub mod bootloader;
pub mod build_step;
pub mod cargo_workspace;
pub mod config_option;
pub mod crate_;
pub mod dep;
pub mod dependency;
pub mod download;
pub mod esp;
pub mod group;
pub mod image;
pub mod partition;
pub mod preset;
pub mod profile;
pub mod project;
pub mod qemu;
pub mod rule;
pub use bootloader::BootloaderBuilder;
pub use build_step::BuildStepBuilder;
pub use cargo_workspace::CargoWorkspaceBuilder;
pub use config_option::ConfigOptionBuilder;
pub use crate_::CrateBuilder;
pub use dep::DepBuilder;
pub use dependency::DependencyBuilder;
pub use download::LimineBuilder;
pub use esp::EspBuilder;
pub use group::GroupBuilder;
pub use image::ImageBuilder;
pub use partition::PartitionBuilder;
pub use preset::PresetBuilder;
pub use profile::ProfileBuilder;
pub use project::ProjectBuilder;
pub use qemu::QemuBuilder;
pub use rule::RuleBuilder;

use gluon_model::SourceSpan;

/// Per-builder state. Every `XBuilder<'cfg>` carries one of these and
/// short-circuits chained methods when it's `Duplicate`. This collapses
/// the per-method `if is_duplicate { return ... }` boilerplate that the
/// engine's `builder_method!` macro spread across every chained call.
#[derive(Debug)]
pub enum BuilderState<S> {
    /// First-time creation. `state` is whatever sibling data the
    /// builder needs (typically a payload like a Handle or a name).
    Valid {
        state: S,
        name: String,
        span: Option<SourceSpan>,
    },
    /// Duplicate name. Methods on the builder must read this and return
    /// without touching the parent `RawBuildModel`.
    Duplicate {
        name: String,
        span: Option<SourceSpan>,
    },
}

impl<S> BuilderState<S> {
    pub fn is_duplicate(&self) -> bool {
        matches!(self, BuilderState::Duplicate { .. })
    }

    pub fn name(&self) -> &str {
        match self {
            BuilderState::Valid { name, .. } | BuilderState::Duplicate { name, .. } => name,
        }
    }
}
