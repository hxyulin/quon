// examples/limine-kernel/quon.rs
//
// A kernel booted by Limine over UEFI. Quon fetches Limine's prebuilt
// BOOTX64.EFI from the pinned release, checks its SHA-256, and puts it
// in the ESP next to the kernel and this directory's limine.conf.
//
//     quon run
//
// The kernel prints the bootloader's name and version on the serial
// port, then exits QEMU through `isa-debug-exit` with status 33
// ((0x10 << 1) | 1). `success_exit_code(33)` makes `quon run` treat
// that status as success and exit 0. The kernel depends on the
// `limine` crate from crates.io, declared in crates/kernel/Cargo.toml.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("limine-kernel", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // x86_64-unknown-none links a position-independent executable by
    // default. Limine loads that only with a PT_DYNAMIC segment, so
    // link a plain executable at the address kernel.ld sets.
    let kernel = q.add(
        Package::new("kernel")
            .linker_script("crates/kernel/kernel.ld")
            .rustc_flag("-Crelocation-model=static"),
    );

    // To upgrade, change the version and the hash. An empty hash makes
    // the build fail and print the hash of the file it fetched.
    let limine = q.add(Download::limine("11.4.1", "BOOTX64.EFI", "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06"));

    // UEFI firmware starts EFI/BOOT/BOOTX64.EFI. Limine finds
    // limine.conf at the ESP root, and the conf names boot/kernel.
    q.add(
        Esp::new("default")
            .add(&limine, "EFI/BOOT/BOOTX64.EFI")
            .add("limine.conf", "limine.conf")
            .add(&kernel, "boot/kernel"),
    );

    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .cores(1)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi)
            .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
            .success_exit_code(33),
    );
    q.run(&kernel).runner(&qemu);
    Ok(())
}
