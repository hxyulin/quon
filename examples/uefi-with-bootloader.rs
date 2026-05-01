// examples/uefi-with-bootloader.rs
//
// UEFI project with a bootloader that embeds a kernel binary.
//
// Demonstrates the two-group pattern used when a UEFI bootloader
// needs to include_bytes! a bare-metal kernel ELF. Gluon's
// `artifact_env` mechanism injects the kernel's output path into
// the bootloader's compile-time environment, and automatically
// adds a build dependency so the kernel compiles first.
//
//     gluon build
//     gluon run --uefi
//
// This file is illustrative — it is not a runnable project on its
// own.

use gluon_config::{Config, CrateType, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("my-os", "0.1.0").default_profile("debug");

    let kernel_target = cfg.target("x86_64-unknown-none");
    let uefi_target = cfg.target("x86_64-unknown-uefi");

    cfg.profile("debug")
        .target(uefi_target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("bootloader");

    cfg.profile("release")
        .target(uefi_target)
        .opt_level(2)
        .debug_info(false)
        .lto("thin")
        .boot_binary("bootloader");

    // ── Kernel group ──────────────────────────────────────────────
    cfg.group("kernel")
        .target(kernel_target)
        .edition("2021")
        .add("kernel", "crates/kernel")
        .crate_type(CrateType::Bin)
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

    // ── Bootloader group ──────────────────────────────────────────
    // `artifact_env("KERNEL_PATH", "kernel")` does two things:
    //   1. Passes `--env KERNEL_PATH=<absolute path to kernel binary>`
    //      to rustc so the bootloader can use:
    //          static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
    //   2. Auto-adds a build-order dependency: the kernel group
    //      compiles before the bootloader group.
    cfg.group("uefi")
        .target(uefi_target)
        .edition("2021")
        .add("bootloader", "crates/bootloader")
        .crate_type(CrateType::Bin)
        .root("src/main.rs")
        .artifact_env("KERNEL_PATH", "kernel");

    // ── ESP assembly ──────────────────────────────────────────────
    cfg.esp("default")
        .add("bootloader", "EFI/BOOT/BOOTX64.EFI");

    // ── QEMU ──────────────────────────────────────────────────────
    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(256)
        .cores(1)
        .serial_stdio()
        .boot_mode("uefi");

    cfg.emit()
}
