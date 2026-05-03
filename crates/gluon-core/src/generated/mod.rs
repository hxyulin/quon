//! Generated artifacts — outputs that are produced *from* the
//! [`gluon_model::BuildModel`] rather than being a direct rustc
//! invocation against a user-authored crate.
//!
//! - [`esp`] — assembles an EFI System Partition directory by copying
//!   source artifacts into the model-specified layout, with
//!   [`crate::cache::Composite`]-cache freshness tracking.
//! - [`config_crate`] — generates a `<project>_config` Rust source
//!   file from the resolved configuration and compiles it via
//!   [`crate::compile::run_compile_and_cache`].
//!
//! Both used to live under `scheduler/helpers/`. They were moved out
//! as part of Stage 6 (compile pipeline cleanup) to make the
//! distinction between "compile is rustc invocation" and "generate
//! these artifacts from the model" explicit at module-tree level.

pub mod esp;
