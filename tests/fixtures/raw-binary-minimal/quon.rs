// Minimal fixture for the RawBinary ELF-sidecar integration test.
//
// Declares one project, one builtin target, one profile, and one
// RawBinary crate with a linker script. The linker script intentionally
// omits OUTPUT_FORMAT — Task 9 covers the negative case where it's
// present.

use quon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("raw-binary-minimal", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.group("boot")
        .target(target)
        .edition("2021")
        .add("boot", "crates/boot")
        .crate_type(CrateType::RawBinary)
        .root("src/main.rs")
        .linker_script("crates/boot/boot.ld");

    Ok(())
}
