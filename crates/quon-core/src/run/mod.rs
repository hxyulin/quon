//! `quon run` and `quon test`: start a built binary with the
//! profile's runner.
//!
//! Quon keeps the build, finding the binary, the per-test ESP copies,
//! timeouts, spawning and signals. The command comes from the runner
//! (see [`runner`]): a command runner's argv, the binary itself on the
//! host, or a runner plugin such as QEMU, which is served by the
//! `quon-runner-qemu` crate over the [`quon_runner`] protocol.
//!
//! - `entry` holds [`prepare`] and [`launch`], and the session that
//!   validates a plugin runner before the build and asks it for the
//!   command after.
//! - [`prepare_tests`] boots each integration-test binary in place of
//!   the run binary. [`host`] finds the host test executables, which
//!   run without a runner.

mod entry;
pub mod host;
pub mod runner;
mod test;

pub use entry::{Prepared, RunOptions, launch, prepare};
pub use host::{HostTest, host_tests};
pub use runner::{Invocation, QEMU_PLUGIN, Runner, Runners, profile_runner};
pub use test::{
    PreparedTest, TestOptions, TestStatus, build_tests, integration_tests, prepare_tests, run_test,
};
