// Minimal UEFI-flavored quon fixture. Self-contained (duplicates the
// `minimal` fixture's tiny crate sources) so integration tests can
// copy this directory alone into a tempdir without a sibling
// dependency. Dry-run tests never compile anything, so source content
// is essentially ceremony.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("minimal-uefi", "0.1.0");

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

    // UEFI boot mode: OVMF pflash + ESP directory mounted via VVFAT.
    // OVMF paths are left unset here — tests inject them via the
    // OVMF_CODE / OVMF_VARS env vars so the fixture stays hermetic.
    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(512)
            .cores(2)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi)
            .esp_dir("./esp"),
    );
    q.run(&kernel).runner(&qemu);

    Ok(())
}
