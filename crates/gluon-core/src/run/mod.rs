//! `gluon run` — QEMU-based kernel launcher.
//!
//! This module owns the runtime side of `gluon run`: resolving a
//! [`gluon_model::QemuDef`] into a concrete QEMU invocation (with both
//! direct-kernel and UEFI boot modes), spawning the process, and
//! managing timeouts and serial forwarding.
//!
//! Layering:
//!
//! - [`qemu_cmd`] assembles the QEMU argv as a pure function. No
//!   filesystem or environment access — all inputs are explicit.
//! - [`ovmf`] probes for OVMF firmware files (explicit → env → system)
//!   and handles the writable-vars copy. Touches the filesystem.
//! - [`resolve`] merges `QemuDef` + profile-level overrides into a
//!   fully-defaulted [`resolve::ResolvedQemu`].
//! - `entry` wires those three together behind the public [`prepare`]
//!   and [`launch`] entry points.
//!
//! [`prepare_tests`] reuses the same steps through `entry`'s helpers, booting
//! each integration-test binary in place of the boot binary.

pub mod boot_mode;
mod entry;
pub mod ovmf;
pub mod qemu_cmd;
pub mod resolve;
mod test;

pub use entry::{Prepared, RunOptions, launch, prepare};
pub use ovmf::{ResolvedOvmf, resolve_ovmf};
pub use qemu_cmd::{QemuInvocation, build_qemu_command};
pub use resolve::{ResolvedQemu, resolve_qemu};
pub use test::{PreparedTest, PreparedTests, TestOptions, TestStatus, prepare_tests, run_test};
