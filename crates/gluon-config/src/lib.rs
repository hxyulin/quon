//! Public Rust DSL for Gluon build scripts.

pub mod builders;
pub mod config;
pub mod error;

pub use config::Config;
pub use error::{ConfigError, Result};

// Re-export model types the user references inside their `gluon.rs`.
// Stage 4 will expand this list as typed enums (`BootloaderKind`, etc.)
// land in `gluon-model`.
pub use gluon_model::{
    Binding, BootMode, BuildModel, ConfigValue, CrateDef, CrateType,
    DepSource, DisplayMode, Expr, GitRef, GroupDef, Handle, ImageDef,
    ImageEntry, PartitionTable, ProfileDef, ProjectDef, RawBuildModel,
    TargetDef, TristateVal,
};

#[cfg(test)]
mod reexports_compile {
    // Compile-only test: ensures every type the fixtures and examples
    // reach for from `gluon_config::*` is actually re-exported. If a
    // future change drops one, this test fails to compile loudly
    // rather than waiting for a fixture port to break.
    #[test]
    fn types_are_visible() {
        use crate::{
            Binding, BootMode, BuildModel, ConfigValue, CrateDef,
            CrateType, DepSource, DisplayMode, Expr, GitRef, GroupDef,
            Handle, ImageDef, ImageEntry, PartitionTable, ProfileDef,
            ProjectDef, RawBuildModel, TargetDef, TristateVal,
        };
        let _ = std::mem::size_of::<(
            Binding,
            BootMode,
            BuildModel,
            ConfigValue,
            CrateDef,
            CrateType,
            DepSource,
            DisplayMode,
            Expr,
            GitRef,
            GroupDef,
            Handle<TargetDef>,
            ImageDef,
            ImageEntry,
            PartitionTable,
            ProfileDef,
            ProjectDef,
            RawBuildModel,
            TristateVal,
        )>();
    }
}
