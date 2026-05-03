//! Per-node work helpers for the pipeline dispatcher.
//!
//! - [`sysroot`] — wraps [`crate::sysroot::ensure_sysroot`] for the DAG-node case.
//!
//! ESP assembly and config-crate generation moved to
//! [`crate::generated`] in Stage 6; the `sysroot` helper is scheduled
//! to fold into [`crate::sysroot`] next.

pub mod sysroot;
