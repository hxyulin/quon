//! Compile primitives used by later pipeline stages.
//!
//! This module exposes the building blocks the rest of quon's pipeline
//! funnels through:
//!
//! - [`BuildLayout`] — pure path arithmetic for every `build/...` directory
//!   quon touches. All other modules must route through this type rather
//!   than hardcoding relative `build/...` strings.
//! - [`RustcInfo`] — cached host `rustc` metadata, probed once per run and
//!   persisted to disk, invalidated by the rustc binary's mtime.
//! - [`RustcCommandBuilder`] / [`Emit`] — the single point in quon that
//!   assembles a rustc invocation, producing both a `Command` and a stable
//!   cache-key hash.
//! - [`CompileCtx`] — layout, rustc info and fingerprint store, threaded
//!   through every compile step (sysroot, host crates, cross crates).

pub mod artifact_path;
pub mod build_step;
pub mod command_builder;
pub mod compile_and_cache;
pub mod compile_crate;
pub mod compile_utils;
pub mod doctest;
pub mod driver;
pub mod env;
pub mod extern_map;
pub mod layout;
pub mod objcopy;
pub mod rustc;
pub mod rustc_info;

pub use driver::DriverKind;

pub(crate) use compile_and_cache::compile_and_cache as run_compile_and_cache;
pub(crate) use compile_and_cache::{PostCompileStep, RustcJob};

pub use compile_crate::{CompileUnitInput, compile as compile_unit};
pub use extern_map::ArtifactMap;
pub use layout::BuildLayout;
pub use rustc::{Emit, RustcCommandBuilder};
pub use rustc_info::RustcInfo;

/// Optimisation level for host units on the fixed host profile: proc
/// macros, vendored host crates, the crates linked into them, and
/// (later) build scripts and their build-deps.
///
/// These units use a fixed profile instead of the active quon profile,
/// matching Cargo's `build-override` defaults. Switching between `dev`
/// and `release` then does not rebuild them, and their output
/// directories can stay profile-independent without serving an
/// artifact built with the other profile's flags. Host tools declared
/// in `quon.rs` use the active profile; see [`crate::unit`].
pub const HOST_OPT_LEVEL: quon_model::OptLevel = quon_model::OptLevel::Zero;

/// Debug-info setting for host units. See [`HOST_OPT_LEVEL`].
pub const HOST_DEBUG_INFO: bool = false;

use crate::cache::{FingerprintStore, StaleReason};
use std::sync::Arc;

/// The shared context threaded through every compile step: path
/// arithmetic, the probed toolchain, and the fingerprint store.
///
/// Nothing in it is mutable. Each node reads and writes only its own
/// fingerprint records, so parallel workers share the context without
/// a lock.
pub struct CompileCtx {
    pub layout: BuildLayout,
    pub rustc_info: Arc<RustcInfo>,
    pub store: FingerprintStore,
    /// Print why each node reruns (`quon -v`).
    pub verbose: bool,
    /// Ask rustc for colored diagnostics. rustc writes into a buffer,
    /// so it never sees a terminal and would not color on its own; the
    /// caller sets this when the stream the buffer ends up on takes
    /// color.
    pub color: bool,
    /// Extra arguments for `clippy-driver`, such as `-D warnings`, from
    /// `quon clippy -- <args>`. They are part of the argv, so of the
    /// cache key: a crate linted clean with warnings allowed lints
    /// again once they are denied.
    pub lint_args: Vec<String>,
}

impl CompileCtx {
    /// Construct a context. The active [`DriverKind`] is carried by
    /// `layout` (constructed via [`BuildLayout::new`] for the default
    /// `quon build` flow, or [`BuildLayout::with_driver`] for
    /// `quon check` / `quon clippy`). Keeping the driver on the
    /// layout means every path-arithmetic call automatically routes
    /// user-crate output into the correct per-driver subtree without
    /// any caller having to thread the driver explicitly.
    pub fn new(layout: BuildLayout, rustc_info: Arc<RustcInfo>) -> Self {
        let store = FingerprintStore::for_layout(&layout);
        Self {
            layout,
            rustc_info,
            store,
            verbose: false,
            color: false,
            lint_args: Vec::new(),
        }
    }

    /// Convenience: returns the driver this context's layout is
    /// configured for. Used by `compile_crate` to pick the program
    /// binary and emit overrides.
    pub fn driver(&self) -> DriverKind {
        self.layout.driver()
    }

    /// The toolchain hash every record carries; see
    /// [`RustcInfo::toolchain_hash`].
    pub fn toolchain(&self) -> [u8; 32] {
        self.rustc_info.toolchain_hash()
    }

    /// Under `quon -v`, write why `what` reruns to the job's stderr,
    /// such as `rebuilding crate kernel (x86_64-unknown-none, dev):
    /// input src/main.rs changed`.
    pub(crate) fn report_stale(&self, sink: &mut Vec<u8>, what: &str, reason: &StaleReason) {
        if self.verbose {
            sink.extend_from_slice(format!("rebuilding {what}: {reason}\n").as_bytes());
        }
    }
}
