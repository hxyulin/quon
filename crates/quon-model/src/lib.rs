//! Pure data types for the Quon build system.
//!
//! This crate depends only on `serde` and `serde_json`. It is safe for
//! embedders (e.g., future menuconfig binary, LSP plugin, CI validator)
//! to depend on without dragging in the build engine.

pub mod build_model;
pub mod handle;
pub mod intern;
pub mod kconfig;
pub mod raw;
pub mod raw_defs;
pub mod resolved;
pub mod script_protocol;
pub mod source;

pub use build_model::*;
pub use handle::{Arena, Handle};
pub use intern::{InternDiagnostic, intern};
pub use kconfig::*;
pub use raw::RawBuildModel;
pub use raw_defs::{
    RawBootTestDef, RawBootloaderDef, RawBuildStepDef, RawBuildStepInput, RawCargoWorkspace,
    RawCrateDef, RawDepDef, RawEspDef, RawEspEntry, RawEspSource, RawGroupDef, RawManifestRoot,
    RawPipelineDef, RawPipelineStep, RawProfileDef, RawRunDef, RawRunnerDef,
};
pub use resolved::*;
pub use script_protocol::WorkspaceMember;
pub use source::SourceSpan;
