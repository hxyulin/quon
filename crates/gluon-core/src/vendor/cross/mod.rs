//! Cross-target vendored builds.
//!
//! Whereas the host pipeline (in [`super::cargo_cmd`]) lets cargo do
//! the entire compile, the cross pipeline drives `rustc` directly so
//! the resulting `.rlib`s link against gluon's per-target sysroot
//! rather than the system `core`/`alloc`. The two pipelines share
//! `cargo vendor` for dependency fetch and `cargo metadata` for graph
//! resolution, but diverge at compile time.
//!
//! # Module layout
//!
//! - [`metadata`] — invoke `cargo metadata` against a per-target
//!   scratch workspace and parse the JSON into a target-agnostic
//!   [`metadata::ResolvedGraph`].
//! - [`graph`] — lower a `ResolvedGraph` into scheduler-node
//!   descriptors ([`graph::VendorCrateBuild`]), applying proc-macro
//!   re-targeting (proc-macros are dlopen'd by rustc and must always
//!   be host) and rejecting `build.rs`-bearing crates (MVP scope:
//!   build scripts are not yet supported).
//! - [`context`] — the side-table the scheduler reads to dispatch
//!   vendor builds. Lives outside [`gluon_model::BuildModel`] because
//!   vendor builds are synthesised at `vendor_sync` time, not declared
//!   by the user; keying it externally keeps the model faithful to the
//!   user's `gluon.rs`.
//!
//! Future modules (next slices, per the design spec):
//!
//! - scheduler integration — a `DagNode::VendorCrate(VendorBuildId)`
//!   variant plus a dispatcher that turns each `VendorCrateBuild` into
//!   a rustc invocation against gluon's per-target sysroot.

pub mod context;
pub mod graph;
pub mod metadata;
