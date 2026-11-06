// End-to-end fixture: x86_64 UEFI bootloader with an embedded kernel.
//
// This fixture exercises the full cross-target build chain that none of
// the other fixtures cover:
//
//   - Two targets in one project (`x86_64-unknown-uefi` for the
//     bootloader, `x86_64-unknown-none` for the kernel), each crate
//     naming its own.
//   - `artifact_env` injection: the bootloader crate receives
//     `KERNEL_PATH` at rustc invocation time and uses `include_bytes!`
//     to embed the compiled kernel ELF.
//   - Build ordering across targets: the kernel must compile first
//     (enforced by the `artifact_deps` edge that `artifact_env` adds).
//   - Auto-assembled ESP: `Esp::new("default")` declares which artifact
//     lands where inside a FAT directory; the `EspBuild` scheduler
//     node populates it from the built bootloader binary.
//   - `quon run --uefi` auto-wires the assembled ESP into QEMU's
//     `-drive fat:rw:` flag, so the user runs it without any manual
//     file copying.
//
// The bootloader deliberately does NOT hand off to the kernel at
// runtime (no ELF parsing, no exit-boot-services, no long-mode
// transition). The fixture's job is to prove the build pipeline
// works end-to-end; a real handoff would balloon this into a hobby
// OS. Runtime verification is via a single byte written to QEMU's
// debugcon port — see crates/bootloader/src/main.rs.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("uefi-bootloader-kernel", "0.1.0");

    let target_none = Target::builtin("x86_64-unknown-none");
    let target_uefi = Target::builtin("x86_64-unknown-uefi");

    // The bootloader `include_bytes!`-es the kernel's output.
    let kernel = q.add(
        Crate::bin("kernel", "crates/kernel")
            .target(&target_none)
            .edition(Edition::E2021)
            .linker_script("crates/kernel/kernel.ld"),
    );

    let bootloader = q.add(
        Crate::bin("bootloader", "crates/bootloader")
            .target(&target_uefi)
            .edition(Edition::E2021)
            .artifact_env("KERNEL_PATH", &kernel),
    );

    // The build copies the compiled bootloader to EFI/BOOT/BOOTX64.EFI,
    // and `quon run --uefi` mounts this directory as the ESP via QEMU's
    // VVFAT driver.
    q.add(Esp::new("default").add(&bootloader, "EFI/BOOT/BOOTX64.EFI"));

    // OVMF paths are intentionally omitted — the runner probes
    // OVMF_CODE / OVMF_VARS env vars at spawn time so the fixture stays
    // hermetic. Tests that actually spawn QEMU (gated on
    // QUON_TEST_QEMU=1) inject the OVMF paths themselves.
    //
    // No `.esp_dir(...)`: QEMU picks up the single ESP declared above.
    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .cores(1)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi),
    );

    // Profiles build for the bootloader's target, since `quon run`
    // starts it.
    q.run(&bootloader).runner(&qemu);

    Ok(())
}
