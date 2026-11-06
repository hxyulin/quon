// examples/uefi-with-bootloader/quon.rs
//
// UEFI project with a bootloader that embeds a kernel binary.
//
// The kernel and the bootloader build for different targets, so each
// package names its own. The bootloader include_bytes!-es the kernel
// ELF through `artifact_env`.
//
//     quon build
//     quon run --uefi
//
// Both packages come from the Cargo workspace in this directory and
// depend on the `boot-info` package. Quon resolves features once per
// target, so `boot-info` is compiled once for each target: as
// `boot-info` for the kernel's target and `boot-info@x86_64-unknown-uefi`
// for the bootloader's.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-os", "0.1.0");
    q.cargo_workspace(".");

    let kernel_target = Target::builtin("x86_64-unknown-none");
    let uefi_target = Target::builtin("x86_64-unknown-uefi");

    let kernel = q.add(
        Package::new("kernel")
            .target(&kernel_target)
            .linker_script("crates/kernel/kernel.ld"),
    );

    // `artifact_env("KERNEL_PATH", &kernel)` passes the kernel's
    // output path to rustc as KERNEL_PATH and builds the kernel first,
    // so the bootloader can use:
    //
    //     static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
    let bootloader = q.add(
        Package::new("bootloader")
            .target(&uefi_target)
            .artifact_env("KERNEL_PATH", &kernel),
    );

    q.add(Esp::new("default").add(&bootloader, "EFI/BOOT/BOOTX64.EFI"));

    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .cores(1)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi),
    );

    // Profiles build for the bootloader's target, since that is what
    // `quon run` starts.
    q.run(&bootloader).runner(&qemu);

    // `debug` has the built-in `dev` settings under the name this
    // project uses by default.
    let debug = q.add(Profile::new("debug").opt_level(0).debug_info(true));
    q.default_profile(&debug);
    q.add(Profile::release().opt_level(2).lto(Lto::Thin));
    Ok(())
}
