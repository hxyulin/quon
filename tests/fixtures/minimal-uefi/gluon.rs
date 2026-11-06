// Minimal UEFI-flavored gluon fixture. Self-contained (duplicates the
// `minimal` fixture's tiny crate sources) so integration tests can
// copy this directory alone into a tempdir without a sibling
// dependency. Dry-run tests never compile anything, so source content
// is essentially ceremony.

use gluon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("minimal-uefi", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .runner("qemu")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");

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

    // UEFI boot mode: OVMF pflash + ESP directory mounted via VVFAT.
    // OVMF paths are left unset here — tests inject them via the
    // OVMF_CODE / OVMF_VARS env vars so the fixture stays hermetic.
    cfg.runner("qemu").qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(512)
        .cores(2)
        .serial_stdio()
        .boot_mode("uefi")
        .esp_dir("./esp");

    Ok(())
}
