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
    BuildModel, CrateDef, CrateType, GroupDef, Handle, ImageDef, ImageEntry,
    ProfileDef, ProjectDef, RawBuildModel, TargetDef,
};
