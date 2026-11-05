// Integration-test fixture: the kernel package has two `harness = false`
// tests. `gluon test` boots each from a copy of the ESP with the test
// binary in place of the kernel. The CLI tests swap QEMU for a script
// that fails the `fails` test.

use gluon_config::{Config, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("cargo-tests", "0.1.0");
    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("kernel");

    cfg.group("kernel").target(target).package("kernel");

    cfg.esp("default")
        .add_file("boot.conf", "boot.conf")
        .add("kernel", "boot/kernel");

    cfg.qemu("qemu-system-x86_64")
        .boot_mode("uefi")
        .success_exit_code(33);

    cfg.emit()
}
