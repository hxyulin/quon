// Integration-test fixture: the kernel package has two `harness = false`
// tests. `quon test` boots each from a copy of the ESP with the test
// binary in place of the kernel. The CLI tests swap QEMU for a script
// that fails the `fails` test. The `boot` test links `testkit`, a
// dev-dependency that `quon build` leaves out. The `smoke` boot test
// builds the `test` profile and boots the kernel itself.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("cargo-tests", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    let kernel = q.add(Package::new("kernel"));
    q.add(
        Esp::new("default")
            .add("boot.conf", "boot.conf")
            .add(&kernel, "boot/kernel"),
    );

    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .boot(BootMode::Uefi)
            .success_exit_code(33),
    );
    q.run(&kernel).runner(&qemu);

    let dev = q.profile("dev");
    let test = q.add(Profile::new("test").inherits(&dev));
    q.add(BootTest::new("smoke", &test));
    Ok(())
}
