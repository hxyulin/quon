//! Per-node work helpers for the pipeline dispatcher.
//!
//! Each submodule handles one `DagNode` variant:
//!
//! - [`sysroot`] — wraps [`crate::sysroot::ensure_sysroot`] for the DAG-node case.
//! - [`config_crate`] — generates + compiles the `<project>_config` rlib.
//!
//! ESP assembly moved to [`crate::generated::esp`] in Stage 6;
//! `config_crate` and `sysroot` are scheduled to follow.

pub mod config_crate;
pub mod sysroot;
