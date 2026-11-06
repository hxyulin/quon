// Integration-test fixture: the kernel package has two `harness = false`
// tests. `gluon test` boots each from a copy of the ESP with the test
// binary in place of the kernel. The CLI tests swap QEMU for a script
// that fails the `fails` test. The `boot` test links `testkit`, a
// dev-dependency that `gluon build` leaves out. The `smoke` boot test
// builds the `test` profile and boots the kernel itself.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("cargo-tests", "0.1.0");
    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");
    cfg.profile("test").inherits("dev");
    cfg.boot_test("smoke").profile("test");

    cfg.group("kernel").target(target).package("kernel");

    cfg.esp("default")
        .add_file("boot.conf", "boot.conf")
        .add("kernel", "boot/kernel");

    cfg.qemu("qemu-system-x86_64")
        .boot_mode("uefi")
        .success_exit_code(33);

    Ok(())
}
