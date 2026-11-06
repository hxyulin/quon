// Minimal gluon fixture used by the gluon-cli integration tests.
//
// Declares:
//   - one project "minimal"
//   - one builtin target "x86_64-unknown-none" (rustup component, not a
//     custom JSON spec — custom-target support requires -Zunstable-options
//     which gluon-core does not yet emit; tracked separately)
//   - one profile "dev" pinned to that target
//   - one host group "host_tools" with a proc-macro crate
//   - one cross group "kernel" with a bare-metal kernel crate that depends
//     on the proc-macro
//
// Note on naming: crate names use underscores rather than dashes because
// gluon-core forwards the declared name to `rustc --crate-name` verbatim,
// and rustc rejects dashes in crate names. Cargo normally normalises dashes
// to underscores; gluon may grow that sugar later but does not have it yet.
//
// No pipeline is declared: the kernel crate is a Bin whose output lands in
// the per-(target,profile) `final/` directory automatically, which is all
// the integration tests need to assert on.

use gluon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("minimal", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("kernel");

    // QEMU config used by `gluon run`. Direct-kernel boot by default;
    // integration tests override to UEFI via --uefi.
    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(256)
        .cores(1)
        .serial_stdio();

    cfg.group("host_tools")
        .target_host()
        .edition("2021")
        .add("minimal_derive", "crates/proc_macro")
        .crate_type(CrateType::ProcMacro)
        .root("src/lib.rs");

    cfg.group("kernel")
        .target(target)
        .edition("2021")
        .add("kernel", "crates/kernel")
        .crate_type(CrateType::Bin)
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld")
        .dep("minimal_derive", "minimal_derive");

    Ok(())
}
