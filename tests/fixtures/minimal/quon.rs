// Minimal quon fixture used by the quon integration tests.
//
// Declares:
//   - one project "minimal"
//   - a host proc-macro crate, `minimal_derive`
//   - a bare-metal kernel crate for the builtin target
//     "x86_64-unknown-none" that depends on the proc macro
//   - a QEMU runner that `quon run` boots the kernel with
//
// The kernel names its target and the proc macro does not, so the
// proc macro builds for the host. The built-in `dev` and `release`
// profiles build for the kernel's target, since `quon run` starts it.
//
// Note on naming: crate names use underscores rather than dashes because
// quon-core forwards the declared name to `rustc --crate-name` verbatim,
// and rustc rejects dashes in crate names.
//
// No pipeline is declared: the kernel crate is a Bin whose output lands in
// the per-(target,profile) `final/` directory automatically, which is all
// the integration tests need to assert on.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("minimal", "0.1.0");

    let target = Target::builtin("x86_64-unknown-none");

    let derive = q.add(
        Crate::proc_macro("minimal_derive", "crates/proc_macro").edition(Edition::E2021),
    );
    let kernel = q.add(
        Crate::bin("kernel", "crates/kernel")
            .target(&target)
            .edition(Edition::E2021)
            .linker_script("crates/kernel/kernel.ld")
            .dep(&derive),
    );

    // Direct-kernel boot by default; integration tests override to UEFI
    // via --uefi.
    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .cores(1)
            .serial(Serial::Stdio),
    );
    q.run(&kernel).runner(&qemu);

    Ok(())
}
