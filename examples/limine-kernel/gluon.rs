// examples/limine-kernel/gluon.rs
//
// A kernel booted by Limine over UEFI. Gluon fetches Limine's prebuilt
// BOOTX64.EFI from the pinned release, checks its SHA-256, and puts it
// in the ESP next to the kernel and this directory's limine.conf.
//
//     gluon run
//
// The kernel prints the bootloader's name and version on the serial
// port, then exits QEMU through `isa-debug-exit` with status 33
// ((0x10 << 1) | 1), which `gluon run` reports as "QEMU exited with
// status 33". The kernel depends on the `limine` crate from
// crates.io, declared in crates/kernel/Cargo.toml.

use gluon_config::{Config, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("limine-kernel", "0.1.0");
    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("kernel");

    // x86_64-unknown-none links a position-independent executable by
    // default. Limine loads that only with a PT_DYNAMIC segment, so
    // link a plain executable at the address kernel.ld sets.
    cfg.group("kernel")
        .target(target)
        .package("kernel")
        .linker_script("crates/kernel/kernel.ld")
        .rustc_flag("-Crelocation-model=static");

    // ── Limine ────────────────────────────────────────────────────
    // To upgrade, change the version and the hash. An empty hash makes
    // the build fail and print the hash of the file it fetched.
    cfg.limine("11.4.1").file("BOOTX64.EFI", "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06");

    // ── ESP ───────────────────────────────────────────────────────
    // UEFI firmware starts EFI/BOOT/BOOTX64.EFI. Limine finds
    // limine.conf at the ESP root, and the conf names boot/kernel.
    cfg.esp("default")
        .add_download("limine/BOOTX64.EFI", "EFI/BOOT/BOOTX64.EFI")
        .add_file("limine.conf", "limine.conf")
        .add("kernel", "boot/kernel");

    // ── QEMU ──────────────────────────────────────────────────────
    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(256)
        .cores(1)
        .serial_stdio()
        .boot_mode("uefi")
        .extra_args(&["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);

    cfg.emit()
}
