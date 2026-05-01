// examples/basic-kernel.rs
//
// Minimal bare-metal kernel project.
//
// This is the simplest useful gluon.rs: one target, one profile, one
// crate compiled into a kernel binary.  Run with:
//
//     gluon build
//     gluon run
//
// This file is illustrative — it is not a runnable project on its own
// (no Rust source or linker script is provided).  Its purpose is to
// document the configuration API.

use gluon_config::{Config, CrateType, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    // ── Project declaration ───────────────────────────────────────
    cfg.project("my-kernel", "0.1.0");

    // ── Target ────────────────────────────────────────────────────
    // Declare the target triple. The optional second argument to
    // `target_spec(name, spec_path)` points at a custom JSON spec
    // file. Use `target(name)` for built-in/rustup targets.
    let target = cfg.target("x86_64-unknown-none");

    // ── Profile ───────────────────────────────────────────────────
    // Profiles control optimisation level, debug info, LTO, and
    // which target to build for. `boot_binary` tells `gluon run`
    // which crate is the entry-point binary for QEMU.
    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("kernel");

    // ── Group + crate ─────────────────────────────────────────────
    // A group collects crates that share a target and edition.
    cfg.group("kernel")
        .target(target)
        .edition("2021")
        .add("kernel", "crates/kernel")
        .crate_type(CrateType::Bin)
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

    // ── QEMU ──────────────────────────────────────────────────────
    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(128)
        .cores(1)
        .serial_stdio();

    cfg.emit()
}
