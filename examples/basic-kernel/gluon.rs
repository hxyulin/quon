// examples/basic-kernel/gluon.rs
//
// Minimal bare-metal kernel project.
//
// This is the simplest useful gluon.rs: one target, one profile, and
// one package from the Cargo workspace in this directory, compiled
// into a kernel binary. Run with:
//
//     gluon build
//     gluon run
//
// The kernel's edition, source path and dependencies come from
// crates/kernel/Cargo.toml. gluon.rs holds only what Cargo has no
// concept of: the target, the profile, the linker script and QEMU.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    // ── Project declaration ───────────────────────────────────────
    cfg.project("my-kernel", "0.1.0");

    // ── Target ────────────────────────────────────────────────────
    // Declare the target triple. `target_spec(name, spec_path)`
    // points at a custom JSON spec file instead. Use `target(name)`
    // for built-in/rustup targets.
    let target = cfg.target("x86_64-unknown-none");

    // ── Cargo workspace ───────────────────────────────────────────
    // The directory holding the workspace Cargo.toml, relative to
    // this file.
    cfg.cargo_workspace(".");

    // ── Profile ───────────────────────────────────────────────────
    // Profiles control optimisation level, debug info, LTO, and
    // which target to build for. `run_binary` tells `gluon run`
    // which binary QEMU boots.
    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");

    // ── Group + package ───────────────────────────────────────────
    // A group pins a target. `package` builds a workspace package for
    // it: its lib and bins, plus everything they depend on.
    cfg.group("kernel")
        .target(target)
        .package("kernel")
        .linker_script("crates/kernel/kernel.ld");

    // ── QEMU ──────────────────────────────────────────────────────
    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(128)
        .cores(1)
        .serial_stdio();

    Ok(())
}
